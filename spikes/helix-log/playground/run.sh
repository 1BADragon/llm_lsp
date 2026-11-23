#!/usr/bin/env bash
# Build the spike server and open the playground in Helix.
# Logs go to ./logs/ (see .helix/languages.toml).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cargo build --release --manifest-path "$here/../Cargo.toml"
mkdir -p "$here/logs"
cd "$here"
exec hx "${@:-src/main.rs}"
