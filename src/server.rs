//! The LSP front end: document sync, activity signals, and hover docs.

use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use tokio::sync::Semaphore;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};
use tracing::{debug, error, info, warn};

use crate::activity::{self, ActivityTracker, Signal, SymbolId};
use crate::claude;
use crate::docs::{self, DocRequest};
use crate::document::{Document, Encoding};
use crate::lang::Lang;
use crate::store::{DocKey, NewDoc, NewRun, Store};
use crate::symbols::{self, Symbol};

const TICK: Duration = Duration::from_secs(1);
/// Doc generations allowed to run at once.
const DOC_CONCURRENCY: usize = 2;
const DOC_TIMEOUT: Duration = Duration::from_secs(120);

/// Settings from the editor's `initializationOptions`. In Helix:
///
/// ```toml
/// [language-server.llm-lsp]
/// command = "llm_lsp"
/// config = { doc-model = "sonnet" }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// The `claude` executable.
    pub claude: PathBuf,
    pub doc_model: String,
    /// Read-only tools the doc writer may use to look things up.
    pub doc_tools: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            claude: "claude".into(),
            doc_model: "haiku".into(),
            doc_tools: ["Read", "Grep", "Glob"].map(String::from).into(),
        }
    }
}

/// A doc that isn't in the cache yet.
enum Pending {
    Generating,
    /// Shown once on the next hover, which then retries.
    Failed(String),
}

pub struct Backend(Arc<State>);

pub struct State {
    client: Client,
    docs: Mutex<HashMap<Uri, Document>>,
    tracker: Arc<Mutex<ActivityTracker>>,
    encoding: OnceLock<Encoding>,
    root: OnceLock<PathBuf>,
    config: OnceLock<Config>,
    /// `None` inside if the database couldn't be opened.
    store: OnceLock<Option<Store>>,
    pending: Mutex<HashMap<DocKey, Pending>>,
    doc_slots: Semaphore,
    next_token: AtomicU64,
}

impl Deref for Backend {
    type Target = State;
    fn deref(&self) -> &State {
        &self.0
    }
}

/// Everything needed to document one symbol, captured at hover time.
struct DocTarget {
    key: DocKey,
    symbol: Symbol,
    path: PathBuf,
    file_text: String,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend(Arc::new(State {
            client,
            docs: Mutex::new(HashMap::new()),
            tracker: Arc::new(Mutex::new(ActivityTracker::new(activity::Config::default()))),
            encoding: OnceLock::new(),
            root: OnceLock::new(),
            config: OnceLock::new(),
            store: OnceLock::new(),
            pending: Mutex::new(HashMap::new()),
            doc_slots: Semaphore::new(DOC_CONCURRENCY),
            next_token: AtomicU64::new(1),
        }))
    }
}

impl State {
    fn encoding(&self) -> Encoding {
        *self.encoding.get().unwrap_or(&Encoding::Utf16)
    }

    fn config(&self) -> &Config {
        self.config.get_or_init(Config::default)
    }

    fn root(&self) -> PathBuf {
        self.root
            .get()
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    fn store(&self) -> Option<&Store> {
        self.store.get().and_then(Option::as_ref)
    }

    fn record(&self, signal: Signal) {
        let mut tracker = self.tracker.lock().unwrap();
        tracker.record(signal);
        debug!(focus = ?tracker.focus().map(|f| &f.qualified_name), "activity");
    }

    /// The symbol at an LSP position, and whether it's documentable.
    fn symbol_at(&self, uri: &Uri, pos: Position) -> Option<(Symbol, Option<DocTarget>)> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(uri)?;
        let offset = doc.offset_at(pos, self.encoding());
        let symbol = symbol_in(doc, offset)?;
        let target = match doc.lang {
            Some(lang) if symbol.kind.is_callable() => {
                let file = uri.to_file_path().map(|p| p.into_owned()).unwrap_or_default();
                let root = self.root();
                Some(DocTarget {
                    key: DocKey {
                        lang,
                        body_hash: docs::body_hash(&doc.text()[symbol.byte_range.clone()]),
                        prompt_version: docs::PROMPT_VERSION,
                    },
                    symbol: symbol.clone(),
                    path: file.strip_prefix(&root).unwrap_or(&file).to_path_buf(),
                    file_text: doc.text().to_string(),
                })
            }
            _ => None,
        };
        Some((symbol, target))
    }

    fn request_signal(&self, uri: &Uri, symbol: Option<&Symbol>) {
        self.record(Signal::Request {
            symbol: symbol.map(|s| symbol_id(uri, s)),
            at: Instant::now(),
        });
    }

    /// Ticks the activity tracker once a second, for the life of the server.
    fn spawn_ticker(&self) {
        let tracker = Arc::clone(&self.tracker);
        let client = self.client.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                let left = tracker.lock().unwrap().tick(Instant::now());
                for event in left {
                    // Later: queue a review. For now, make it visible.
                    let msg = format!(
                        "leave: {} ({})",
                        event.symbol.qualified_name, event.symbol.uri
                    );
                    info!("{msg}");
                    client.log_message(MessageType::INFO, msg).await;
                }
            }
        });
    }

    /// The hover for a callable: the cached doc, or a placeholder while one
    /// is generated in the background.
    async fn doc_hover(self: &Arc<Self>, target: DocTarget) -> String {
        let name = target.symbol.qualified_name.clone();
        let Some(store) = self.store() else {
            return format!("*llm_lsp: no doc cache for `{name}`. The database failed to open (see `:log-open`).*");
        };
        match store.doc(&target.key).await {
            Ok(Some(doc)) => return doc,
            Ok(None) => {}
            Err(e) => error!("reading doc cache: {e:#}"),
        }

        {
            let mut pending = self.pending.lock().unwrap();
            match pending.remove(&target.key) {
                Some(Pending::Generating) => {
                    pending.insert(target.key, Pending::Generating);
                    return format!("*llm_lsp: still documenting `{name}`…*");
                }
                Some(Pending::Failed(e)) => {
                    return format!(
                        "*llm_lsp: documenting `{name}` failed. Hover again to retry.*\n\n```\n{e}\n```"
                    );
                }
                None => {
                    pending.insert(target.key.clone(), Pending::Generating);
                }
            }
        }
        tokio::spawn(Arc::clone(self).generate_doc(target));
        format!("*llm_lsp: documenting `{name}`… hover again in a few seconds.*")
    }

    async fn generate_doc(self: Arc<Self>, target: DocTarget) {
        let name = target.symbol.qualified_name.clone();
        let _slot = self.doc_slots.acquire().await.expect("semaphore never closed");
        let progress = self.begin_progress(format!("Documenting {name}")).await;

        let result = self.run_doc(&target).await;
        let message = match &result {
            Ok(_) => format!("Documented {name}"),
            Err(_) => format!("Documenting {name} failed"),
        };
        if let Some(p) = progress {
            p.finish_with_message(message).await;
        }

        let mut pending = self.pending.lock().unwrap();
        match result {
            Ok(()) => {
                pending.remove(&target.key);
            }
            Err(e) => {
                warn!("documenting {name}: {e:#}");
                pending.insert(target.key, Pending::Failed(format!("{e:#}")));
            }
        }
    }

    async fn run_doc(&self, target: &DocTarget) -> anyhow::Result<()> {
        let config = self.config();
        let root = self.root();
        let context = read_context_files(&root);
        let prompt = docs::prompt(&DocRequest {
            lang: target.key.lang,
            symbol: &target.symbol,
            path: &target.path,
            file_text: &target.file_text,
            context: &context,
        });
        let run = claude::Run {
            program: config.claude.clone(),
            cwd: root,
            model: config.doc_model.clone(),
            system_prompt: docs::SYSTEM_PROMPT.into(),
            prompt,
            tools: config.doc_tools.clone(),
            timeout: DOC_TIMEOUT,
        };

        let started_at = SystemTime::now();
        let started = Instant::now();
        let result = run.execute().await;
        let store = self.store().expect("only called with a store");
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        let recorded = store
            .record_run(NewRun {
                kind: "doc",
                subject: &target.symbol.qualified_name,
                model: &config.doc_model,
                started_at,
                duration: started.elapsed(),
                cost_usd: result.as_ref().ok().and_then(|o| o.cost_usd),
                error: error.as_deref(),
            })
            .await;
        if let Err(e) = recorded {
            error!("recording run: {e:#}");
        }

        let output = result?;
        info!(
            name = %target.symbol.qualified_name,
            ms = output.duration.as_millis() as u64,
            cost = ?output.cost_usd,
            "documented"
        );
        let doc = docs::clean(&output.text);
        anyhow::ensure!(!doc.is_empty(), "the model returned an empty doc");
        store
            .put_doc(NewDoc {
                key: &target.key,
                qualified_name: &target.symbol.qualified_name,
                doc: &doc,
                model: &config.doc_model,
            })
            .await
    }

    /// Starts a progress spinner in the editor, if the client allows it.
    async fn begin_progress(
        &self,
        message: String,
    ) -> Option<tower_lsp_server::OngoingProgress<tower_lsp_server::Unbounded, tower_lsp_server::NotCancellable>>
    {
        let n = self.next_token.fetch_add(1, Ordering::Relaxed);
        let token = ProgressToken::String(format!("llm_lsp/{n}"));
        if let Err(e) = self.client.create_work_done_progress(token.clone()).await {
            debug!("no progress support: {e}");
            return None;
        }
        Some(
            self.client
                .progress(token, "llm_lsp")
                .with_message(message)
                .begin()
                .await,
        )
    }
}

fn read_context_files(root: &Path) -> Vec<(String, String)> {
    docs::CONTEXT_FILES
        .iter()
        .filter_map(|name| {
            let text = std::fs::read_to_string(root.join(name)).ok()?;
            Some((name.to_string(), text))
        })
        .collect()
}

fn symbol_in(doc: &Document, offset: usize) -> Option<Symbol> {
    symbols::enclosing_symbol(doc.lang?, doc.tree()?, doc.text(), offset)
}

fn symbol_id(uri: &Uri, s: &Symbol) -> SymbolId {
    SymbolId {
        uri: uri.to_string(),
        qualified_name: s.qualified_name.clone(),
    }
}

/// The project root from `initialize`: the first workspace folder, else the
/// (deprecated) root URI.
fn project_root(params: &InitializeParams) -> Option<PathBuf> {
    let uri = params
        .workspace_folders
        .as_ref()
        .and_then(|f| f.first())
        .map(|f| &f.uri);
    #[allow(deprecated)]
    let uri = uri.or(params.root_uri.as_ref())?;
    uri.to_file_path().map(|p| p.into_owned())
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let utf8 = params
            .capabilities
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.as_ref())
            .is_some_and(|encs| encs.contains(&PositionEncodingKind::UTF8));
        let (enc, kind) = if utf8 {
            (Encoding::Utf8, PositionEncodingKind::UTF8)
        } else {
            (Encoding::Utf16, PositionEncodingKind::UTF16)
        };
        let _ = self.encoding.set(enc);

        let config = match params.initialization_options.clone() {
            Some(v) => serde_json::from_value(v).unwrap_or_else(|e| {
                error!("invalid llm-lsp config, using defaults: {e}");
                Config::default()
            }),
            None => Config::default(),
        };
        let _ = self.config.set(config);

        if let Some(root) = project_root(&params) {
            let _ = self.root.set(root);
        }
        let root = self.root();
        let db = crate::store::default_path(&root);
        let store = match Store::open(&db).await {
            Ok(s) => Some(s),
            Err(e) => {
                error!("opening {}: {e:#}", db.display());
                None
            }
        };
        let _ = self.store.set(store);
        info!(?enc, root = %root.display(), db = %db.display(), config = ?self.config(), "initialize");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(kind),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                // Never returns items. Helix sends completion to every server
                // that supports it, with the position after a typing pause,
                // so advertising it is a free "where am I" signal.
                completion_provider: Some(CompletionOptions::default()),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.spawn_ticker();
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let td = params.text_document;
        let lang = Lang::from_language_id(&td.language_id);
        debug!(uri = %td.uri.as_str(), ?lang, "open");
        let doc = Document::new(lang, td.version, td.text);
        self.docs.lock().unwrap().insert(td.uri, doc);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let enc = self.encoding();
        let signal = {
            let mut docs = self.docs.lock().unwrap();
            let Some(doc) = docs.get_mut(&uri) else {
                return;
            };
            let spans = doc.apply_changes(params.text_document.version, &params.content_changes, enc);

            let mut symbols: Vec<SymbolId> = Vec::new();
            let mut lines: Vec<usize> = Vec::new();
            for span in &spans {
                if let Some(s) = symbol_in(doc, span.start) {
                    let id = symbol_id(&uri, &s);
                    if !symbols.contains(&id) {
                        symbols.push(id);
                    }
                }
                let (first, last) = (doc.point_at(span.start).row, doc.point_at(span.end).row);
                lines.extend(first..=last);
            }
            lines.sort_unstable();
            lines.dedup();

            Signal::Edit {
                uri: uri.to_string(),
                symbols,
                ranges: spans.len(),
                lines_touched: lines.len(),
                at: Instant::now(),
            }
        };
        self.record(signal);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.record(Signal::Save {
            uri: params.text_document.uri.to_string(),
            at: Instant::now(),
        });
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.docs.lock().unwrap().remove(&params.text_document.uri);
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let p = params.text_document_position;
        let symbol = self.symbol_at(&p.text_document.uri, p.position).map(|(s, _)| s);
        self.request_signal(&p.text_document.uri, symbol.as_ref());
        Ok(None)
    }

    /// Generated docs for the function or method under the cursor. Other
    /// symbols get no hover from us.
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let p = params.text_document_position_params;
        let found = self.symbol_at(&p.text_document.uri, p.position);
        self.request_signal(&p.text_document.uri, found.as_ref().map(|(s, _)| s));

        let Some((_, Some(target))) = found else {
            return Ok(None);
        };
        let value = self.0.doc_hover(target).await;
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }
}
