# Findings: what Helix 25.07.1 sends a second language server

Method: the `helix-log` server was listed *after* clangd / rust-analyzer in
`playground/.helix/languages.toml`, then used over four short sessions (open,
type, save, hover, code actions, switching buffers, moving the cursor).
Logs: `playground/logs/` (not committed).

## Setup

- Helix's workspace trust ignores `.helix/` until `:workspace-trust`. Servers
  that are already running don't pick up the config, so restart `hx` after
  trusting.
- Helix runs **one server process per workspace root**, shared by all
  languages (ring.c and main.rs went to the same pid).

## Signals a second-listed server receives

| Signal | Received | Notes |
|---|---|---|
| `didOpen` | ✅ | Only the first time a buffer opens. |
| `didChange` | ✅ | Incremental, **one per keystroke**, exact range + text. |
| `didSave` | ✅ | |
| `completion` | ✅ | Automatic after a short typing pause, with position. |
| `hover` | ✅ | Only on `space k`, with position. |
| `codeAction` | ✅ | Only on `space a`, with the cursor/selection range. Never automatic. |
| `workspace/executeCommand` | ✅ | Code action → command → our `showMessage` popup worked. |
| `textDocument/diagnostic` (pull) | ✅ | ~250ms after edits, and in **bursts** (15 in 2.6s while rust-analyzer started). |
| `documentHighlight` | ⚠️ | Only right at file open, before the primary server is ready. |
| Cursor movement | ❌ | Nothing. Single-server features (documentHighlight, probably inlayHint and signatureHelp) go to the **first** capable server only. |
| `inlayHint`, `signatureHelp` | ❌ | Never received (same reason). |
| `didClose`, buffer switch | ❌ | Switching to an already-open buffer sends nothing. |

When our server is listed second, it sees **edits and explicit requests, not
cursor movement**. Being listed first would give us `documentHighlight` on
every cursor move (the primary server gets one about every 40ms during
movement). But Helix would then show only our (empty) highlights instead of
clangd's / rust-analyzer's. Not worth it.

## Display

- **Hover**: Helix shows hovers from both servers and lets you switch between
  them. So hover docs can just use `textDocument/hover`.
- **Diagnostics**: a pushed Hint from the second server shows up alongside the
  others.

## Other details

- **Format on save** produces one `didChange` touching many lines (here,
  clangd re-indenting), ~20ms before `didSave`. It is not user activity and
  must not count as "worked here".
- **Client capabilities**:
  - position encodings utf-8 / utf-32 / utf-16. Use **utf-8**.
  - `workDoneProgress` (progress spinner)
  - `workspace/applyEdit`
  - `showDocument`
  - hover is markdown only
  - pull diagnostics with `relatedDocumentSupport`
  - `publishDiagnostics` with `versionSupport`
