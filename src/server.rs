//! The LSP front end: document sync, activity signals, and a debug hover.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};
use tracing::{debug, info};

use crate::activity::{self, ActivityTracker, Signal, SymbolId};
use crate::document::{Document, Encoding};
use crate::lang::Lang;
use crate::symbols::{self, Symbol};

const TICK: Duration = Duration::from_secs(1);

pub struct Backend {
    client: Client,
    docs: Mutex<HashMap<Uri, Document>>,
    tracker: Arc<Mutex<ActivityTracker>>,
    encoding: OnceLock<Encoding>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            docs: Mutex::new(HashMap::new()),
            tracker: Arc::new(Mutex::new(ActivityTracker::new(activity::Config::default()))),
            encoding: OnceLock::new(),
        }
    }

    fn encoding(&self) -> Encoding {
        *self.encoding.get().unwrap_or(&Encoding::Utf16)
    }

    fn record(&self, signal: Signal) {
        debug!(?signal, "activity");
        self.tracker.lock().unwrap().record(signal);
    }

    /// The symbol at an LSP position, with the document's language.
    fn symbol_at(&self, uri: &Uri, pos: Position) -> Option<Symbol> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(uri)?;
        let offset = doc.offset_at(pos, self.encoding());
        symbol_in(doc, offset)
    }

    fn request_signal(&self, uri: &Uri, pos: Position) -> Option<Symbol> {
        let symbol = self.symbol_at(uri, pos);
        self.record(Signal::Request {
            uri: uri.to_string(),
            symbol: symbol.as_ref().map(|s| symbol_id(uri, s)),
            at: Instant::now(),
        });
        symbol
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
        info!(?enc, "initialize");

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
        self.request_signal(&p.text_document.uri, p.position);
        Ok(None)
    }

    /// Skeleton hover: shows what the symbol lookup and tracker think. This
    /// becomes the generated-doc hover in build step 3.
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let p = params.text_document_position_params;
        let symbol = self.request_signal(&p.text_document.uri, p.position);

        let symbol_line = match &symbol {
            Some(s) => format!(
                "`{}` ({:?}, lines {}–{})",
                s.qualified_name,
                s.kind,
                s.start.row + 1,
                s.end.row + 1
            ),
            None => "none (`symbols::enclosing_symbol` returned None)".into(),
        };
        let focus = self
            .tracker
            .lock()
            .unwrap()
            .focus()
            .map(|f| format!("`{}`", f.qualified_name))
            .unwrap_or_else(|| "none".into());

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!(
                    "**llm_lsp** (skeleton)\n\n- enclosing symbol: {symbol_line}\n- tracker focus: {focus}"
                ),
            }),
            range: None,
        }))
    }
}
