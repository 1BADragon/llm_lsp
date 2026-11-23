#!/usr/bin/env bash
# Build llm_lsp and open the playground in Helix.
# Logs go to Helix's log (:log-open).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cargo build --manifest-path "$here/../Cargo.toml"
cd "$here"
exec hx "${@:-src/main.rs}"
