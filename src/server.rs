//! The LSP front end: document sync, activity signals, hover docs, and
//! passive review published as diagnostics.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::json;
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

    /// The symbol at an LSP position, and a doc target if it's a callable.
    fn symbol_at(&self, uri: &Uri, pos: Position) -> Option<(Symbol, Option<Target>)> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(uri)?;
        let offset = doc.offset_at(pos, self.encoding());
        let symbol = symbol_in(doc, offset)?;
        let target = symbol
            .kind
            .is_callable()
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
                .and_then(|(lang, tree)| symbols::find_callable(lang, tree, doc.text(), &id.qualified_name));
            let Some(symbol) = found else {
                return skip("it isn't a function or method in the open file");
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

                let context = read_context_files(&self.root());
                let prompt = review::prompt(&ReviewRequest {
                    lang: target.key.lang,
                    symbol: &target.symbol,
                    path: &target.path,
                    file_text: &target.file_text,
                    context: &context,
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
                let findings = review::parse(structured, target.body())?;
                store
                    .put_review(&target.key, name, &serde_json::to_string(&findings)?, &config.review_model)
                    .await?;
                findings
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

    /// The function or method at a position, if any.
    fn callable_at(&self, uri: &Uri, pos: Position) -> Option<Symbol> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(uri)?;
        symbol_in(doc, doc.offset_at(pos, self.encoding())).filter(|s| s.kind.is_callable())
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
                .or_insert_with(|| symbols::find_callable(lang, tree, doc.text(), &p.qualified_name));
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

    /// The hover for a callable: the cached doc, or a placeholder while one
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
        let context = read_context_files(&self.root());
        let prompt = docs::prompt(&DocRequest {
            lang: target.key.lang,
            symbol: &target.symbol,
            path: &target.path,
            file_text: &target.file_text,
            context: &context,
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
        self.store()
            .expect("only called with a store")
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

fn review_summary(name: &str, shown: usize, total: usize, cached: bool) -> String {
    let what = match shown {
        0 => "no findings".to_string(),
        1 => "1 finding".to_string(),
        n => format!("{n} findings"),
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
                    commands: vec![DISMISS_COMMAND.into(), REVIEW_COMMAND.into()],
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
        if let Some(symbol) = self.callable_at(&uri, params.range.start) {
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
        }
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<LSPAny>> {
        if params.command == REVIEW_COMMAND {
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
                            "llm_lsp: no function in focus. Edit or hover one first, or use the code action (space a).",
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
