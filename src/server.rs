//! The LSP front end: document sync, activity signals, hover docs, and
//! passive review published as diagnostics.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};
use tracing::{debug, error, info, warn};

use crate::activity::{self, ActivityTracker, Signal, SymbolId};
use crate::claude;
use crate::doc_comment;
use crate::docs::{self, DocRequest};
use crate::document::{Document, Encoding};
use crate::lang::Lang;
use crate::notes;
use crate::review::{self, Finding, ReviewRequest};
use crate::store::{CacheKey, NewDoc, NewRun, Store};
use crate::symbols::{self, Symbol};

const TICK: Duration = Duration::from_secs(1);
/// Doc generations allowed to run at once.
const DOC_CONCURRENCY: usize = 2;
const DOC_TIMEOUT: Duration = Duration::from_secs(120);
const REVIEW_TIMEOUT: Duration = Duration::from_secs(300);
const DISMISS_COMMAND: &str = "llm.dismiss";
/// Review a function now. Arguments: `[uri, qualified_name]`, or none for
/// the function in focus (what `:lsp-workspace-command` sends).
const REVIEW_COMMAND: &str = "llm.reviewSymbol";
/// Generate a doc (if not cached) and insert it as a doc comment.
/// Arguments: `[uri, qualified_name]`.
const INSERT_DOC_COMMAND: &str = "llm.insertDoc";
/// Open the notes as an editable Markdown file.
const SHOW_NOTES_COMMAND: &str = "llm.showNotes";
/// Delete every note for the project (asks first).
const CLEAR_NOTES_COMMAND: &str = "llm.clearNotes";
const SOURCE: &str = "llm_lsp";

/// Settings from the editor's `initializationOptions`. In Helix:
///
/// ```toml
/// [language-server.llm-lsp]
/// command = "llm_lsp"
/// config = { doc-model = "sonnet", max-reviews-per-hour = 10 }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// The `claude` executable.
    pub claude: PathBuf,
    pub doc_model: String,
    /// Read-only tools the doc writer may use to look things up.
    pub doc_tools: Vec<String>,
    /// Passive review on/off.
    pub reviews: bool,
    pub review_model: String,
    pub review_tools: Vec<String>,
    /// Reviews wait until there's been no activity at all for this long.
    pub review_idle_secs: u64,
    /// Cap on review runs (cache hits don't count).
    pub max_reviews_per_hour: i64,
}

impl Default for Config {
    fn default() -> Self {
        let read_only = || ["Read", "Grep", "Glob"].map(String::from).into();
        Config {
            claude: "claude".into(),
            doc_model: "haiku".into(),
            doc_tools: read_only(),
            reviews: true,
            review_model: "sonnet".into(),
            review_tools: read_only(),
            review_idle_secs: 10,
            max_reviews_per_hour: 20,
        }
    }
}

/// A doc that isn't in the cache yet.
enum Pending {
    Generating,
    /// Shown once on the next hover, which then retries.
    Failed(String),
}

/// A review finding, tied to the exact function text it was made for.
struct Published {
    qualified_name: String,
    body_hash: String,
    finding: Finding,
}

enum ReviewOutcome {
    /// `shown` excludes dismissed findings; `total` includes them.
    Reviewed { name: String, shown: usize, total: usize, cached: bool },
    Skipped(String),
}

/// The notes file last written by `llm.showNotes`, so edits to it can be
/// read back.
struct NotesView {
    path: PathBuf,
    /// Modification time right after we wrote it.
    written: SystemTime,
    /// Ids the file showed. Only these can be deleted by editing it.
    ids: HashSet<i64>,
}

/// A finding that still applies to the current text, ready to show.
struct LiveFinding {
    diagnostic: Diagnostic,
    qualified_name: String,
    fingerprint: String,
    fix: Option<String>,
}

pub struct Backend(Arc<State>);

pub struct State {
    client: Client,
    docs: Mutex<HashMap<Uri, Document>>,
    tracker: Mutex<ActivityTracker>,
    encoding: OnceLock<Encoding>,
    root: OnceLock<PathBuf>,
    config: OnceLock<Config>,
    /// `None` inside if the database couldn't be opened.
    store: OnceLock<Option<Store>>,
    pending: Mutex<HashMap<CacheKey, Pending>>,
    doc_slots: Semaphore,
    next_token: AtomicU64,
    notes_view: Mutex<Option<NotesView>>,
    review_queue: Mutex<VecDeque<SymbolId>>,
    reviewing: AtomicBool,
    /// Findings by document URI (as a string, like `SymbolId::uri`).
    /// Lock order: `findings`, then `docs`, then `dismissed`.
    findings: Mutex<HashMap<String, Vec<Published>>>,
    /// `(qualified_name, fingerprint)`, mirrored from the store.
    dismissed: Mutex<HashSet<(String, String)>>,
}

impl Deref for Backend {
    type Target = State;
    fn deref(&self) -> &State {
        &self.0
    }
}

/// A symbol to document or review, captured from the live document.
struct Target {
    key: CacheKey,
    symbol: Symbol,
    path: PathBuf,
    file_text: String,
}

impl Target {
    fn body(&self) -> &str {
        &self.file_text[self.symbol.byte_range.clone()]
    }
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend(Arc::new(State {
            client,
            docs: Mutex::new(HashMap::new()),
            tracker: Mutex::new(ActivityTracker::new(activity::Config::default())),
            encoding: OnceLock::new(),
            root: OnceLock::new(),
            config: OnceLock::new(),
            store: OnceLock::new(),
            pending: Mutex::new(HashMap::new()),
            doc_slots: Semaphore::new(DOC_CONCURRENCY),
            next_token: AtomicU64::new(1),
            review_queue: Mutex::new(VecDeque::new()),
            reviewing: AtomicBool::new(false),
            findings: Mutex::new(HashMap::new()),
            dismissed: Mutex::new(HashSet::new()),
            notes_view: Mutex::new(None),
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

    fn target(&self, uri: &Uri, doc: &Document, symbol: Symbol, prompt_version: i64) -> Option<Target> {
        let lang = doc.lang?;
        let file = uri.to_file_path().map(|p| p.into_owned()).unwrap_or_default();
        let root = self.root();
        Some(Target {
            key: CacheKey {
                lang,
                body_hash: docs::body_hash(&doc.text()[symbol.byte_range.clone()]),
                prompt_version,
            },
            path: file.strip_prefix(&root).unwrap_or(&file).to_path_buf(),
            file_text: doc.text().to_string(),
            symbol,
        })
    }

    /// The symbol at an LSP position, and a doc target if it's reviewable.
    fn symbol_at(&self, uri: &Uri, pos: Position) -> Option<(Symbol, Option<Target>)> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(uri)?;
        let offset = doc.offset_at(pos, self.encoding());
        let symbol = symbol_in(doc, offset)?;
        let target = symbol
            .kind
            .is_reviewable()
            .then(|| self.target(uri, doc, symbol.clone(), docs::PROMPT_VERSION))
            .flatten();
        Some((symbol, target))
    }

    fn request_signal(&self, uri: &Uri, symbol: Option<&Symbol>) {
        self.record(Signal::Request {
            symbol: symbol.map(|s| symbol_id(uri, s)),
            at: Instant::now(),
        });
    }

    /// Once a second: tick the tracker, queue reviews for symbols I left,
    /// and start the next review once I've been idle long enough.
    fn spawn_ticker(self: &Arc<Self>) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                state.tick().await;
            }
        });
    }

    async fn tick(self: &Arc<Self>) {
        if let Err(e) = self.sync_notes_view().await {
            warn!("reading edited notes: {e:#}");
        }
        let now = Instant::now();
        let (left, idle) = {
            let mut tracker = self.tracker.lock().unwrap();
            (tracker.tick(now), tracker.idle_for(now))
        };
        let config = self.config();

        for event in left {
            info!("leave: {} ({})", event.symbol.qualified_name, event.symbol.uri);
            if config.reviews {
                let mut queue = self.review_queue.lock().unwrap();
                if !queue.contains(&event.symbol) {
                    queue.push_back(event.symbol);
                }
            }
        }

        let idle_enough = idle >= Duration::from_secs(config.review_idle_secs);
        if idle_enough && !self.reviewing.swap(true, Ordering::AcqRel) {
            let next = self.review_queue.lock().unwrap().pop_front();
            match next {
                Some(id) => {
                    let state = Arc::clone(self);
                    tokio::spawn(async move {
                        match state.review(&id, false).await {
                            Ok(ReviewOutcome::Skipped(why)) => debug!("{why}"),
                            Ok(ReviewOutcome::Reviewed { .. }) => {}
                            Err(e) => warn!("reviewing {}: {e:#}", id.qualified_name),
                        }
                        state.reviewing.store(false, Ordering::Release);
                    });
                }
                None => self.reviewing.store(false, Ordering::Release),
            }
        }
    }

    /// Reviews one symbol (from cache when its text was reviewed before) and
    /// publishes the findings. `explicit` reviews were asked for, so they
    /// ignore the hourly budget.
    async fn review(&self, id: &SymbolId, explicit: bool) -> anyhow::Result<ReviewOutcome> {
        let skip = |why: &str| Ok(ReviewOutcome::Skipped(format!("not reviewing `{}`: {why}", id.qualified_name)));
        let Some(store) = self.store() else {
            return skip("the database failed to open");
        };
        let uri: Uri = id.uri.parse()?;
        let target = {
            let docs = self.docs.lock().unwrap();
            let Some(doc) = docs.get(&uri) else {
                return skip("its file isn't open");
            };
            let found = doc
                .lang
                .zip(doc.tree())
                .and_then(|(lang, tree)| symbols::find_reviewable(lang, tree, doc.text(), &id.qualified_name));
            let Some(symbol) = found else {
                return skip("it isn't a function, method, type or trait in the open file");
            };
            self.target(&uri, doc, symbol, review::PROMPT_VERSION)
        };
        let Some(target) = target else {
            return skip("unsupported language");
        };
        let name = &target.symbol.qualified_name;
        let config = self.config();

        let cached = store.review(&target.key).await?;
        let from_cache = cached.is_some();
        let findings: Vec<Finding> = match cached {
            Some(json) => {
                debug!("review of {name} is cached");
                serde_json::from_str(&json)?
            }
            None => {
                let hour_ago = SystemTime::now() - Duration::from_secs(3600);
                let used = store.runs_since("review", hour_ago).await?;
                if !explicit && used >= config.max_reviews_per_hour {
                    let msg = format!(
                        "review budget reached ({used} in the last hour); not reviewing {name}"
                    );
                    warn!("{msg}");
                    self.client.log_message(MessageType::WARNING, &msg).await;
                    return Ok(ReviewOutcome::Skipped(msg));
                }

                let path = target.path.to_string_lossy();
                self.prune_symbol_notes(&uri, &path).await?;
                let shown_notes = store.notes_for(&path, name).await?;
                let context = read_context_files(&self.root());
                let prompt = review::prompt(&ReviewRequest {
                    lang: target.key.lang,
                    symbol: &target.symbol,
                    path: &target.path,
                    file_text: &target.file_text,
                    context: &context,
                    notes: &shown_notes,
                });
                let run = claude::Run {
                    program: config.claude.clone(),
                    cwd: self.root(),
                    model: config.review_model.clone(),
                    system_prompt: review::SYSTEM_PROMPT.into(),
                    prompt,
                    tools: config.review_tools.clone(),
                    json_schema: Some(review::SCHEMA.into()),
                    timeout: REVIEW_TIMEOUT,
                };
                let progress = self.begin_progress(format!("Reviewing {name}")).await;
                let result = self.run_claude("review", name, &run).await;
                if let Some(p) = progress {
                    p.finish().await;
                }
                let output = result?;
                let structured = output.structured.expect("checked by claude::Run");
                let parsed = review::parse(structured, target.body(), &shown_notes)?;
                store
                    .put_review(&target.key, name, &serde_json::to_string(&parsed.findings)?, &config.review_model)
                    .await?;
                for note in &parsed.notes {
                    store.add_note(note.scope, &path, name, &note.text, name).await?;
                }
                store.delete_notes(&parsed.obsolete_notes).await?;
                store.prune_notes().await?;
                if !parsed.notes.is_empty() || !parsed.obsolete_notes.is_empty() {
                    info!(
                        name = %name,
                        added = ?parsed.notes.iter().map(|n| &n.text).collect::<Vec<_>>(),
                        retired = ?parsed.obsolete_notes,
                        "notes"
                    );
                }
                parsed.findings
            }
        };
        info!(name = %name, findings = findings.len(), "reviewed");
        let total = findings.len();

        {
            let mut all = self.findings.lock().unwrap();
            let list = all.entry(id.uri.clone()).or_default();
            list.retain(|p| &p.qualified_name != name);
            list.extend(findings.into_iter().map(|finding| Published {
                qualified_name: name.clone(),
                body_hash: target.key.body_hash.clone(),
                finding,
            }));
        }
        self.publish(&uri).await;
        let shown = self
            .live_findings(&uri)
            .iter()
            .filter(|l| &l.qualified_name == name)
            .count();
        Ok(ReviewOutcome::Reviewed {
            name: name.clone(),
            shown,
            total,
            cached: from_cache,
        })
    }

    /// Runs an explicit review in the background and reports the result in
    /// a message, even when there's nothing to show.
    fn review_now(self: &Arc<Self>, id: SymbolId) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            state.review_queue.lock().unwrap().retain(|q| q != &id);
            let (typ, msg) = match state.review(&id, true).await {
                Ok(ReviewOutcome::Reviewed { name, shown, total, cached }) => {
                    (MessageType::INFO, review_summary(&name, shown, total, cached))
                }
                Ok(ReviewOutcome::Skipped(why)) => (MessageType::WARNING, format!("llm_lsp: {why}")),
                Err(e) => (
                    MessageType::ERROR,
                    format!("llm_lsp: reviewing `{}` failed: {e:#}", id.qualified_name),
                ),
            };
            state.client.show_message(typ, msg).await;
        });
    }

    /// The reviewable symbol at a position, if any.
    fn reviewable_at(&self, uri: &Uri, pos: Position) -> Option<Symbol> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(uri)?;
        symbol_in(doc, doc.offset_at(pos, self.encoding())).filter(|s| s.kind.is_reviewable())
    }

    /// Findings for `uri` that still apply: not dismissed, and their function
    /// is unchanged since review. Positions are computed from the current
    /// text, so they follow edits elsewhere in the file.
    fn live_findings(&self, uri: &Uri) -> Vec<LiveFinding> {
        let all = self.findings.lock().unwrap();
        let Some(list) = all.get(uri.as_str()).filter(|l| !l.is_empty()) else {
            return Vec::new();
        };
        let docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get(uri) else {
            return Vec::new();
        };
        let (Some(lang), Some(tree)) = (doc.lang, doc.tree()) else {
            return Vec::new();
        };
        let dismissed = self.dismissed.lock().unwrap();
        let enc = self.encoding();

        let mut symbols: HashMap<&str, Option<Symbol>> = HashMap::new();
        let mut live = Vec::new();
        for p in list {
            let fingerprint = p.finding.fingerprint();
            if dismissed.contains(&(p.qualified_name.clone(), fingerprint.clone())) {
                continue;
            }
            let symbol = symbols
                .entry(&p.qualified_name)
                .or_insert_with(|| symbols::find_reviewable(lang, tree, doc.text(), &p.qualified_name));
            let Some(symbol) = symbol else {
                continue;
            };
            let body = &doc.text()[symbol.byte_range.clone()];
            if docs::body_hash(body) != p.body_hash {
                continue; // edited since review: stale until re-reviewed
            }
            let Some(at) = p.finding.locate(body) else {
                continue;
            };
            let start = symbol.byte_range.start + at.start;
            let end = symbol.byte_range.start + at.end;
            live.push(LiveFinding {
                diagnostic: Diagnostic {
                    range: Range::new(doc.position_at(start, enc), doc.position_at(end, enc)),
                    severity: Some(match p.finding.severity {
                        review::Severity::Error => DiagnosticSeverity::ERROR,
                        review::Severity::Warning => DiagnosticSeverity::WARNING,
                        review::Severity::Info => DiagnosticSeverity::INFORMATION,
                    }),
                    code: Some(NumberOrString::String(p.finding.category.clone())),
                    source: Some(SOURCE.into()),
                    message: p.finding.message.clone(),
                    ..Default::default()
                },
                qualified_name: p.qualified_name.clone(),
                fingerprint,
                fix: p.finding.fix.clone(),
            });
        }
        live
    }

    fn has_findings(&self, uri: &Uri) -> bool {
        self.findings
            .lock()
            .unwrap()
            .get(uri.as_str())
            .is_some_and(|l| !l.is_empty())
    }

    async fn publish(&self, uri: &Uri) {
        let diagnostics = self.live_findings(uri).into_iter().map(|l| l.diagnostic).collect();
        self.client.publish_diagnostics(uri.clone(), diagnostics, None).await;
    }

    /// The hover for a reviewable symbol: the cached doc, or a placeholder while one
    /// is generated in the background.
    async fn doc_hover(self: &Arc<Self>, target: Target) -> String {
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

    async fn generate_doc(self: Arc<Self>, target: Target) {
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

    async fn run_doc(&self, target: &Target) -> anyhow::Result<()> {
        let config = self.config();
        let store = self.store().expect("only called with a store");
        let notes = store
            .notes_for(&target.path.to_string_lossy(), &target.symbol.qualified_name)
            .await?;
        let context = read_context_files(&self.root());
        let prompt = docs::prompt(&DocRequest {
            lang: target.key.lang,
            symbol: &target.symbol,
            path: &target.path,
            file_text: &target.file_text,
            context: &context,
            notes: &notes,
        });
        let run = claude::Run {
            program: config.claude.clone(),
            cwd: self.root(),
            model: config.doc_model.clone(),
            system_prompt: docs::SYSTEM_PROMPT.into(),
            prompt,
            tools: config.doc_tools.clone(),
            json_schema: None,
            timeout: DOC_TIMEOUT,
        };
        let output = self.run_claude("doc", &target.symbol.qualified_name, &run).await?;
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

    /// Runs `claude` and records the run (success or failure) in the store.
    async fn run_claude(&self, kind: &str, subject: &str, run: &claude::Run) -> anyhow::Result<claude::Output> {
        let started_at = SystemTime::now();
        let started = Instant::now();
        let result = run.execute().await;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        if let Some(store) = self.store() {
            let recorded = store
                .record_run(NewRun {
                    kind,
                    subject,
                    model: &run.model,
                    started_at,
                    duration: started.elapsed(),
                    cost_usd: result.as_ref().ok().and_then(|o| o.cost_usd),
                    error: error.as_deref(),
                })
                .await;
            if let Err(e) = recorded {
                error!("recording run: {e:#}");
            }
        }
        if let Ok(o) = &result {
            info!(kind, subject, model = %run.model, ms = o.duration.as_millis() as u64, cost = ?o.cost_usd, "claude run");
        }
        result
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

    /// The doc comment edit for `qualified_name` in the current text of `uri`,
    /// and whether it replaces an existing doc comment.
    fn doc_text_edit(&self, uri: &Uri, qualified_name: &str, doc: &str) -> Option<(TextEdit, bool)> {
        let docs = self.docs.lock().unwrap();
        let document = docs.get(uri)?;
        let (lang, tree) = (document.lang?, document.tree()?);
        let symbol = symbols::find_reviewable(lang, tree, document.text(), qualified_name)?;
        let edit = doc_comment::doc_edit(lang, tree, document.text(), &symbol, doc)?;
        let enc = self.encoding();
        let range = Range::new(
            document.position_at(edit.range.start, enc),
            document.position_at(edit.range.end, enc),
        );
        Some((TextEdit { range, new_text: edit.text }, edit.replaces))
    }

    /// "Insert doc comment" for `symbol`: an instant edit when its doc is
    /// cached, otherwise a command that generates the doc first.
    async fn doc_comment_action(&self, uri: &Uri, symbol: &Symbol) -> Option<CodeActionOrCommand> {
        let store = self.store()?;
        let key = {
            let docs = self.docs.lock().unwrap();
            self.target(uri, docs.get(uri)?, symbol.clone(), docs::PROMPT_VERSION)?.key
        };
        let cached = store.doc(&key).await.ok().flatten();
        let action = match cached {
            Some(doc) => {
                let (edit, replaces) = self.doc_text_edit(uri, &symbol.qualified_name, &doc)?;
                CodeAction {
                    title: format!(
                        "llm_lsp: {} doc comment",
                        if replaces { "replace" } else { "insert" }
                    ),
                    kind: Some(CodeActionKind::REFACTOR),
                    edit: Some(WorkspaceEdit {
                        changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            }
            None => CodeAction {
                title: "llm_lsp: generate and insert doc comment".into(),
                kind: Some(CodeActionKind::REFACTOR),
                command: Some(Command {
                    title: "Insert doc".into(),
                    command: INSERT_DOC_COMMAND.into(),
                    arguments: Some(vec![json!(uri.as_str()), json!(symbol.qualified_name)]),
                }),
                ..Default::default()
            },
        };
        Some(CodeActionOrCommand::CodeAction(action))
    }

    /// `llm.insertDoc`: generates the doc in the background if needed, then
    /// asks the editor to insert it where the symbol is *now*.
    fn insert_doc_later(self: &Arc<Self>, id: SymbolId) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = state.insert_doc(&id).await {
                state
                    .client
                    .show_message(
                        MessageType::ERROR,
                        format!("llm_lsp: inserting a doc for `{}` failed: {e:#}", id.qualified_name),
                    )
                    .await;
            }
        });
    }

    async fn insert_doc(&self, id: &SymbolId) -> anyhow::Result<()> {
        let store = self.store().context("the database failed to open")?;
        let uri: Uri = id.uri.parse()?;
        let target = {
            let docs = self.docs.lock().unwrap();
            let doc = docs.get(&uri).context("the file isn't open")?;
            let (lang, tree) = (
                doc.lang.context("unsupported language")?,
                doc.tree().context("no syntax tree")?,
            );
            let symbol = symbols::find_reviewable(lang, tree, doc.text(), &id.qualified_name)
                .context("it no longer exists")?;
            self.target(&uri, doc, symbol, docs::PROMPT_VERSION)
                .context("unsupported language")?
        };
        let doc = match store.doc(&target.key).await? {
            Some(doc) => doc,
            None => {
                let progress = self.begin_progress(format!("Documenting {}", id.qualified_name)).await;
                let result = self.run_doc(&target).await;
                if let Some(p) = progress {
                    p.finish().await;
                }
                result?;
                store.doc(&target.key).await?.context("the doc wasn't saved")?
            }
        };
        // Positions come from the current text, in case it changed meanwhile.
        let (edit, _) = self
            .doc_text_edit(&uri, &id.qualified_name, &doc)
            .context("there's no place for a doc comment here")?;
        let response = self
            .client
            .apply_edit(WorkspaceEdit {
                changes: Some(HashMap::from([(uri, vec![edit])])),
                ..Default::default()
            })
            .await?;
        if !response.applied {
            let why = response.failure_reason.unwrap_or_else(|| "no reason given".into());
            anyhow::bail!("the editor didn't apply it: {why}");
        }
        Ok(())
    }

    /// Deletes notes about symbols that no longer exist in this (open) file.
    async fn prune_symbol_notes(&self, uri: &Uri, path: &str) -> anyhow::Result<()> {
        let Some(store) = self.store() else {
            return Ok(());
        };
        let candidates = store.symbol_notes_in(path).await?;
        if candidates.is_empty() {
            return Ok(());
        }
        let gone: Vec<i64> = {
            let docs = self.docs.lock().unwrap();
            let Some(doc) = docs.get(uri) else {
                return Ok(());
            };
            let (Some(lang), Some(tree)) = (doc.lang, doc.tree()) else {
                return Ok(());
            };
            candidates
                .into_iter()
                .filter(|(_, name)| symbols::find_reviewable(lang, tree, doc.text(), name).is_none())
                .map(|(id, _)| id)
                .collect()
        };
        if !gone.is_empty() {
            info!(?gone, path, "deleting notes about symbols that no longer exist");
            store.delete_notes(&gone).await?;
        }
        Ok(())
    }

    fn notes_view_path(&self) -> PathBuf {
        crate::store::default_path(&self.root()).with_extension("notes.md")
    }

    /// `llm.showNotes`: writes all notes to a Markdown file and opens it.
    async fn show_notes(&self) -> anyhow::Result<()> {
        let store = self.store().context("the database failed to open")?;
        let all = store.all_notes().await?;
        let path = self.notes_view_path();
        std::fs::write(&path, notes::render(&self.root().display().to_string(), &all))?;
        let written = std::fs::metadata(&path)?.modified()?;
        *self.notes_view.lock().unwrap() = Some(NotesView {
            path: path.clone(),
            written,
            ids: all.iter().map(|n| n.id).collect(),
        });
        let uri = Uri::from_file_path(&path).context("notes path isn't a valid URI")?;
        let shown = self
            .client
            .show_document(ShowDocumentParams {
                uri,
                external: Some(false),
                take_focus: Some(true),
                selection: None,
            })
            .await
            .unwrap_or(false);
        if !shown {
            self.client
                .show_message(MessageType::INFO, format!("llm_lsp: notes are in {}", path.display()))
                .await;
        }
        Ok(())
    }

    /// Applies edits I saved to the notes file: removed lines delete their
    /// notes, edited text updates them.
    async fn sync_notes_view(&self) -> anyhow::Result<()> {
        let Some(store) = self.store() else {
            return Ok(());
        };
        let (path, written, ids) = match &*self.notes_view.lock().unwrap() {
            Some(v) => (v.path.clone(), v.written, v.ids.clone()),
            None => return Ok(()),
        };
        let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            return Ok(());
        };
        if modified <= written {
            return Ok(());
        }

        let kept: HashMap<i64, String> = notes::parse_view(&std::fs::read_to_string(&path)?)
            .into_iter()
            .collect();
        let current: HashMap<i64, String> =
            store.all_notes().await?.into_iter().map(|n| (n.id, n.text)).collect();
        let deleted: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| !kept.contains_key(id) && current.contains_key(id))
            .collect();
        let mut edited = 0;
        for (id, text) in &kept {
            if ids.contains(id) && current.get(id).is_some_and(|old| old != text) {
                store.update_note_text(*id, text).await?;
                edited += 1;
            }
        }
        store.delete_notes(&deleted).await?;

        if let Some(v) = &mut *self.notes_view.lock().unwrap() {
            v.written = modified;
            v.ids.retain(|id| !deleted.contains(id));
        }
        if !deleted.is_empty() || edited > 0 {
            let msg = format!("llm_lsp: notes updated ({} deleted, {edited} edited)", deleted.len());
            info!("{msg}");
            self.client.show_message(MessageType::INFO, msg).await;
        }
        Ok(())
    }

    /// `llm.clearNotes`: deletes every note, after asking.
    async fn clear_notes(&self) -> anyhow::Result<()> {
        let store = self.store().context("the database failed to open")?;
        let count = store.all_notes().await?.len();
        if count == 0 {
            self.client.show_message(MessageType::INFO, "llm_lsp: there are no notes").await;
            return Ok(());
        }
        let delete = MessageActionItem {
            title: format!("Delete {}", plural(count, "note")),
            properties: HashMap::new(),
        };
        let choice = self
            .client
            .show_message_request(
                MessageType::WARNING,
                format!("llm_lsp: delete {} for this project?", match count {
                    1 => "the only note".to_string(),
                    n => format!("all {n} notes"),
                }),
                Some(vec![
                    delete.clone(),
                    MessageActionItem {
                        title: "Cancel".into(),
                        properties: HashMap::new(),
                    },
                ]),
            )
            .await?;
        if choice.is_some_and(|c| c.title == delete.title) {
            let n = store.clear_notes().await?;
            *self.notes_view.lock().unwrap() = None;
            self.client
                .show_message(MessageType::INFO, format!("llm_lsp: deleted {}", plural(n as usize, "note")))
                .await;
        }
        Ok(())
    }

    async fn dismiss(&self, uri: &str, qualified_name: &str, fingerprint: &str) -> anyhow::Result<()> {
        self.dismissed
            .lock()
            .unwrap()
            .insert((qualified_name.to_string(), fingerprint.to_string()));
        if let Some(store) = self.store() {
            store.dismiss(qualified_name, fingerprint).await?;
        }
        self.publish(&uri.parse()?).await;
        Ok(())
    }
}

/// Deletes notes about files that no longer exist.
async fn prune_missing_files(store: &Store, root: &Path) -> anyhow::Result<()> {
    for path in store.note_paths().await? {
        if !root.join(&path).exists() {
            let n = store.delete_notes_for_path(&path).await?;
            info!(path, n, "deleted notes about a file that no longer exists");
        }
    }
    Ok(())
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

/// `1 note`, `3 notes`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

fn review_summary(name: &str, shown: usize, total: usize, cached: bool) -> String {
    let what = match shown {
        0 => "no findings".to_string(),
        n => plural(n, "finding"),
    };
    let mut notes = Vec::new();
    if total > shown {
        notes.push(format!("{} dismissed", total - shown));
    }
    if cached {
        notes.push("unchanged since last review".to_string());
    }
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    };
    format!("llm_lsp: {what} in `{name}`{notes}")
}

/// `llm.reviewSymbol` arguments: `Some(None)` for none (use focus),
/// `Some(Some(id))` for `[uri, qualified_name]`, `None` if malformed.
fn review_target(args: &[LSPAny]) -> Option<Option<SymbolId>> {
    match args {
        [] => Some(None),
        [uri, name] => Some(Some(SymbolId {
            uri: uri.as_str()?.to_string(),
            qualified_name: name.as_str()?.to_string(),
        })),
        _ => None,
    }
}

fn lines_overlap(a: Range, b: Range) -> bool {
    a.start.line <= b.end.line && b.start.line <= a.end.line
}

/// The first sentence of a message, cut to fit a menu.
fn short(message: &str) -> String {
    let first = message.split(". ").next().unwrap_or(message).trim_end_matches('.');
    if first.chars().count() <= 60 {
        first.to_string()
    } else {
        format!("{}…", first.chars().take(59).collect::<String>())
    }
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
        if let Some(store) = &store {
            match store.dismissals().await {
                Ok(d) => self.dismissed.lock().unwrap().extend(d),
                Err(e) => error!("loading dismissals: {e:#}"),
            }
            if let Err(e) = prune_missing_files(store, &root).await {
                error!("pruning notes: {e:#}");
            }
        }
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
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: [
                        DISMISS_COMMAND,
                        REVIEW_COMMAND,
                        INSERT_DOC_COMMAND,
                        SHOW_NOTES_COMMAND,
                        CLEAR_NOTES_COMMAND,
                    ]
                        .map(String::from)
                        .into(),
                    ..Default::default()
                }),
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
        self.0.spawn_ticker();
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let td = params.text_document;
        let lang = Lang::from_language_id(&td.language_id);
        debug!(uri = %td.uri.as_str(), ?lang, "open");
        let doc = Document::new(lang, td.version, td.text);
        self.docs.lock().unwrap().insert(td.uri.clone(), doc);
        if self.has_findings(&td.uri) {
            self.publish(&td.uri).await;
        }
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
        // Move findings with the text, and drop ones whose function changed.
        if self.has_findings(&uri) {
            self.publish(&uri).await;
        }
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

    /// Generated docs for the function, method, type or trait under the
    /// cursor. Other symbols get no hover from us.
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

    /// For each of our findings on the requested lines: apply its fix (if
    /// it has one) or dismiss it.
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let mut actions = Vec::new();
        for live in self.live_findings(&uri) {
            if !lines_overlap(live.diagnostic.range, params.range) {
                continue;
            }
            let label = short(&live.diagnostic.message);
            if let Some(fix) = &live.fix {
                let edit = TextEdit {
                    range: live.diagnostic.range,
                    new_text: fix.clone(),
                };
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("llm_lsp: fix: {label}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![live.diagnostic.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
                        ..Default::default()
                    }),
                    is_preferred: Some(true),
                    ..Default::default()
                }));
            }
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("llm_lsp: dismiss: {label}"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![live.diagnostic]),
                command: Some(Command {
                    title: "Dismiss".into(),
                    command: DISMISS_COMMAND.into(),
                    arguments: Some(vec![
                        json!(uri.as_str()),
                        json!(live.qualified_name),
                        json!(live.fingerprint),
                    ]),
                }),
                ..Default::default()
            }));
        }
        if let Some(symbol) = self.reviewable_at(&uri, params.range.start) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("llm_lsp: review `{}`", symbol.qualified_name),
                kind: Some(CodeActionKind::SOURCE),
                command: Some(Command {
                    title: "Review".into(),
                    command: REVIEW_COMMAND.into(),
                    arguments: Some(vec![json!(uri.as_str()), json!(symbol.qualified_name)]),
                }),
                ..Default::default()
            }));
            if let Some(action) = self.doc_comment_action(&uri, &symbol).await {
                actions.push(action);
            }
        }
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<LSPAny>> {
        if params.command == SHOW_NOTES_COMMAND || params.command == CLEAR_NOTES_COMMAND {
            let result = if params.command == SHOW_NOTES_COMMAND {
                self.show_notes().await
            } else {
                self.clear_notes().await
            };
            if let Err(e) = result {
                self.client
                    .show_message(MessageType::ERROR, format!("llm_lsp: {}: {e:#}", params.command))
                    .await;
            }
        } else if params.command == INSERT_DOC_COMMAND {
            match review_target(&params.arguments) {
                Some(Some(id)) => self.0.insert_doc_later(id),
                _ => warn!("bad {INSERT_DOC_COMMAND} arguments: {:?}", params.arguments),
            }
        } else if params.command == REVIEW_COMMAND {
            let id = match review_target(&params.arguments) {
                Some(Some(id)) => Some(id),
                Some(None) => self.tracker.lock().unwrap().focus().cloned(),
                None => {
                    warn!("bad {REVIEW_COMMAND} arguments: {:?}", params.arguments);
                    None
                }
            };
            match id {
                Some(id) => self.0.review_now(id),
                None => {
                    self.client
                        .show_message(
                            MessageType::WARNING,
                            "llm_lsp: nothing in focus. Edit or hover a function or type first, or use the code action (space a).",
                        )
                        .await
                }
            }
        } else if params.command == DISMISS_COMMAND {
            let args: Vec<String> = params
                .arguments
                .iter()
                .filter_map(|a| a.as_str().map(String::from))
                .collect();
            match args.as_slice() {
                [uri, name, fingerprint] => {
                    if let Err(e) = self.dismiss(uri, name, fingerprint).await {
                        error!("dismissing: {e:#}");
                    }
                }
                _ => warn!("bad {DISMISS_COMMAND} arguments: {:?}", params.arguments),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_takes_the_first_sentence_and_fits_a_menu() {
        assert_eq!(short("b may be zero. Then it panics."), "b may be zero");
        assert_eq!(short("Uses `x.y`. Then"), "Uses `x.y`");
        let long = "a".repeat(80);
        assert_eq!(short(&long).chars().count(), 60);
    }

    #[test]
    fn review_summaries() {
        assert_eq!(review_summary("f", 0, 0, false), "llm_lsp: no findings in `f`");
        assert_eq!(review_summary("f", 2, 2, false), "llm_lsp: 2 findings in `f`");
        assert_eq!(
            review_summary("f", 0, 1, true),
            "llm_lsp: no findings in `f` (1 dismissed, unchanged since last review)"
        );
    }

    #[test]
    fn review_command_arguments() {
        assert_eq!(review_target(&[]), Some(None));
        assert_eq!(
            review_target(&[json!("file:///a.rs"), json!("P::f")]),
            Some(Some(SymbolId { uri: "file:///a.rs".into(), qualified_name: "P::f".into() }))
        );
        assert_eq!(review_target(&[json!("file:///a.rs")]), None);
        assert_eq!(review_target(&[json!(1), json!("f")]), None);
    }

    #[test]
    fn overlap_is_by_line() {
        let r = |a, b| Range::new(Position::new(a, 5), Position::new(b, 1));
        assert!(lines_overlap(r(3, 3), r(3, 3)));
        assert!(lines_overlap(r(1, 4), r(4, 9)));
        assert!(!lines_overlap(r(1, 2), r(3, 3)));
    }
}
