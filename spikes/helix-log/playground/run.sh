#!/usr/bin/env bash
# Build llm_lsp (and the spike server) and open the playground in Helix.
# llm_lsp logs to Helix's log (:log-open); spike logs go to ./logs/.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cargo build --manifest-path "$here/../../../Cargo.toml"
cargo build --release --manifest-path "$here/../Cargo.toml"
mkdir -p "$here/logs"
cd "$here"
exec hx "${@:-src/main.rs}"
