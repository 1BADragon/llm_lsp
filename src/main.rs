//! llm_lsp: an LLM-backed pair-programmer language server. See DESIGN.md.

// The `symbols` and `activity` stubs leave much of the plumbing unused.
// Remove once they're implemented.
#![allow(dead_code)]

mod activity;
mod document;
mod lang;
mod server;
mod symbols;

use tower_lsp_server::{LspService, Server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // stdout is the LSP channel; logs go to stderr, which Helix copies into
    // its own log (`:log-open`). Override with RUST_LOG, e.g. llm_lsp=debug.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("llm_lsp=info")),
        )
        .init();

    let (service, socket) = LspService::new(server::Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
