//! Spike: a do-nothing language server that logs everything Helix sends.
//!
//! It advertises every capability that could give us a "where is the user"
//! signal, answers with empty/neutral results, and records each message to:
//!
//! - `<dir>/helix-log.log`   — one readable summary line per message
//! - `<dir>/helix-log.jsonl` — full message (long strings truncated)
//!
//! `<dir>` is `$LLM_LSP_SPIKE_DIR`, else `$XDG_STATE_HOME/llm_lsp`, else
//! `~/.local/state/llm_lsp`.
//!
//! It also answers hover with a visible marker (to see how Helix merges hovers
//! from several servers) and offers one code action that runs a workspace
//! command (to check the `:lsp-workspace-command` / executeCommand path).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// A gap this long between messages gets a blank line in the readable log,
/// so separate "bursts" of activity stand out.
const GAP_SECS: f64 = 2.0;

struct Logger {
    summary: File,
    full: File,
    start: Instant,
    last: Option<Instant>,
    pid: u32,
}

impl Logger {
    fn open() -> io::Result<Self> {
        let dir = log_dir();
        fs::create_dir_all(&dir)?;
        let open = |name: &str| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(name))
        };
        let mut logger = Logger {
            summary: open("helix-log.log")?,
            full: open("helix-log.jsonl")?,
            start: Instant::now(),
            last: None,
            pid: std::process::id(),
        };
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cwd = std::env::current_dir().unwrap_or_default();
        writeln!(
            logger.summary,
            "\n=== start pid={} unix={} cwd={} ===",
            logger.pid,
            epoch,
            cwd.display()
        )?;
        eprintln!("helix-log: logging to {}", dir.display());
        Ok(logger)
    }

    /// `dir` is "<-" for client→server, "->" for server→client.
    fn log(&mut self, dir: &str, msg: &Value) {
        let now = Instant::now();
        if let Some(last) = self.last
            && now.duration_since(last).as_secs_f64() > GAP_SECS
        {
            let _ = writeln!(self.summary);
        }
        self.last = Some(now);
        let t = now.duration_since(self.start).as_secs_f64();

        let _ = writeln!(
            self.summary,
            "{t:9.3} {pid} {dir} {}",
            summarize(msg),
            pid = self.pid
        );
        let entry = json!({ "t": t, "pid": self.pid, "dir": dir, "msg": truncate(msg) });
        let _ = writeln!(self.full, "{entry}");
        let _ = self.summary.flush();
        let _ = self.full.flush();
    }
}

fn log_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LLM_LSP_SPIKE_DIR") {
        return PathBuf::from(d);
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    state.join("llm_lsp")
}

// ---------------------------------------------------------------- summaries

fn summarize(msg: &Value) -> String {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").map(|i| format!("#{i} ")).unwrap_or_default();
    let Some(method) = method else {
        // A response (to us) or something malformed.
        return format!("{id}response {}", short(&truncate(msg), 160));
    };
    let p = msg.get("params").cloned().unwrap_or(Value::Null);
    let file = p
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .map(file_name)
        .unwrap_or_default();

    let detail = match method {
        "textDocument/didOpen" => format!(
            "{file} v{} lang={} len={}",
            p.pointer("/textDocument/version").unwrap_or(&Value::Null),
            p.pointer("/textDocument/languageId")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            p.pointer("/textDocument/text")
                .and_then(Value::as_str)
                .map_or(0, str::len)
        ),
        "textDocument/didChange" => {
            let changes = p
                .get("contentChanges")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            let text = c.get("text").and_then(Value::as_str).unwrap_or("");
                            match c.get("range") {
                                Some(r) => format!("{} {:?}", range(r), preview(text)),
                                None => format!("FULL len={}", text.len()),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .unwrap_or_default();
            format!(
                "{file} v{} [{changes}]",
                p.pointer("/textDocument/version").unwrap_or(&Value::Null)
            )
        }
        _ => {
            let mut parts = vec![file];
            if let Some(pos) = p.get("position") {
                parts.push(format!("@{}", position(pos)));
            }
            if let Some(r) = p.get("range") {
                parts.push(range(r));
            }
            if let Some(ctx) = p.get("context") {
                parts.push(format!("ctx={}", short(&truncate(ctx), 120)));
            }
            if let Some(cmd) = p.get("command") {
                parts.push(format!("cmd={cmd} args={}", p.get("arguments").unwrap_or(&Value::Null)));
            }
            if method == "initialize" {
                parts.push(format!(
                    "client={} root={}",
                    p.get("clientInfo").unwrap_or(&Value::Null),
                    p.get("rootUri").unwrap_or(&Value::Null)
                ));
            }
            parts.retain(|s| !s.is_empty());
            parts.join(" ")
        }
    };
    format!("{id}{method} {detail}")
}

fn file_name(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or(uri).to_string()
}

/// Positions are shown 1-based (line:col) to match what Helix displays.
fn position(p: &Value) -> String {
    let n = |k| p.get(k).and_then(Value::as_u64).unwrap_or(0) + 1;
    format!("{}:{}", n("line"), n("character"))
}

fn range(r: &Value) -> String {
    format!(
        "{}-{}",
        position(r.get("start").unwrap_or(&Value::Null)),
        position(r.get("end").unwrap_or(&Value::Null))
    )
}

fn preview(s: &str) -> String {
    const MAX: usize = 40;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…(+{})", s.chars().take(MAX).collect::<String>(), s.len())
    }
}

fn short(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.len() <= max {
        s
    } else {
        let cut = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        format!("{}…", &s[..cut])
    }
}

/// Replace long strings (document text) so the jsonl log stays readable.
fn truncate(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > 200 => Value::String(format!(
            "<{} bytes> {}",
            s.len(),
            preview(s)
        )),
        Value::Array(a) => Value::Array(a.iter().map(truncate).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), truncate(v))).collect()),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------- transport

fn read_message(r: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let len = len.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no Content-Length"))?;
    let mut buf = vec![0; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn send(out: &mut impl Write, log: &mut Logger, msg: Value) -> io::Result<()> {
    log.log("->", &msg);
    let body = msg.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

// ---------------------------------------------------------------- server

const PING: &str = "spike.ping";

fn capabilities() -> Value {
    json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": {
            "openClose": true,
            "change": 2, // incremental
            "save": { "includeText": false },
            "willSave": true,
        },
        "hoverProvider": true,
        "completionProvider": { "triggerCharacters": [".", ":", ">", "("] },
        "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
        "documentHighlightProvider": true,
        "documentSymbolProvider": true,
        "codeActionProvider": true,
        "inlayHintProvider": true,
        "selectionRangeProvider": true,
        "diagnosticProvider": { "interFileDependencies": false, "workspaceDiagnostics": false },
        "executeCommandProvider": { "commands": [PING] },
        "workspace": {
            "workspaceFolders": { "supported": true, "changeNotifications": true }
        },
    })
}

/// Result for a request, or `None` for "method not found".
fn handle_request(method: &str, params: &Value) -> Option<Value> {
    Some(match method {
        "initialize" => json!({
            "capabilities": capabilities(),
            "serverInfo": { "name": "helix-log-spike" },
        }),
        "shutdown" => Value::Null,
        "textDocument/hover" => {
            let pos = params.get("position").map(position).unwrap_or_default();
            json!({ "contents": { "kind": "markdown", "value": format!("**helix-log spike** hover at `{pos}`") } })
        }
        "textDocument/codeAction" => json!([{
            "title": "spike: ping (executeCommand)",
            "kind": "quickfix",
            "command": { "title": "ping", "command": PING, "arguments": [params.get("range")] },
        }]),
        "workspace/executeCommand" => Value::Null,
        "textDocument/completion" => json!([]),
        "textDocument/signatureHelp" => Value::Null,
        "textDocument/documentHighlight" => json!([]),
        "textDocument/documentSymbol" => json!([]),
        "textDocument/inlayHint" => json!([]),
        "textDocument/selectionRange" => json!([]),
        "textDocument/diagnostic" => json!({ "kind": "full", "items": [] }),
        _ => return None,
    })
}

fn main() -> io::Result<()> {
    let mut log = Logger::open()?;
    let mut input = BufReader::new(io::stdin().lock());
    let mut out = io::stdout().lock();

    while let Some(msg) = read_message(&mut input)? {
        log.log("<-", &msg);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        if method == "exit" {
            break;
        }
        if method == "textDocument/didOpen" {
            // One hint per file, to see how Helix shows a second server's diagnostics.
            let uri = params.pointer("/textDocument/uri").cloned().unwrap_or(Value::Null);
            send(
                &mut out,
                &mut log,
                json!({
                    "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "diagnostics": [{
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                        "severity": 4, "source": "helix-log",
                        "message": "helix-log spike: test diagnostic",
                    }]},
                }),
            )?;
        }
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification, or a response to one of our requests
        };
        if method.is_empty() {
            continue; // response from the client
        }

        let reply = match handle_request(method, &params) {
            Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            None => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("unhandled: {method}") },
            }),
        };
        send(&mut out, &mut log, reply)?;

        if method == "workspace/executeCommand" {
            send(
                &mut out,
                &mut log,
                json!({
                    "jsonrpc": "2.0", "method": "window/showMessage",
                    "params": { "type": 3, "message": format!("helix-log spike: pong {}", params.get("arguments").unwrap_or(&Value::Null)) },
                }),
            )?;
        }
    }
    Ok(())
}
