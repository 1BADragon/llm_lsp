# Spike: what does Helix send?

A language server that does nothing except log every message it receives from
Helix. It answers DESIGN.md open questions 1–2:

1. Which requests and notifications carry "where is the user" information, and
   when are they sent? (didChange ranges, hover, codeAction, completion,
   signatureHelp, inlayHint, documentHighlight, pull diagnostics, …)
2. How does Helix show hovers, diagnostics, and code actions from a *second*
   server next to rust-analyzer / clangd / pyright?

It advertises every relevant capability and returns empty results, with these
exceptions, so the UI side is visible:

- **hover** returns `**helix-log spike** hover at L:C`
- **codeAction** returns `spike: ping (executeCommand)`. Running it calls
  `workspace/executeCommand`, which pops up a `pong` message.
- **didOpen** publishes one Hint diagnostic on line 1 of the file

## Build

```sh
cargo build --release   # → target/release/helix-log
```

## Playground

`playground/` is a throwaway workspace that's already set up: a small Rust
crate (`src/main.rs`), `c/ring.c`, `c/lru.cpp`, `py/inventory.py`, and a
`.helix/` folder with its own `languages.toml` (rust-analyzer / clangd /
pyright + ruff, plus `helix-log`) and `config.toml` (inlay hints and auto
document highlight on). Each source file has one deliberate bug, saved for
later testing of the real reviewer.

```sh
spikes/helix-log/playground/run.sh            # builds the spike, opens src/main.rs
spikes/helix-log/playground/run.sh c/ring.c   # or any file
```

**First time only:** this Helix build has workspace trust and ignores
`.helix/` until you allow it. In the playground, run `:workspace-trust`,
then restart `hx`. If you later edit `.helix/`, trust it again.

Logs go to `playground/logs/` (git-ignored).

To check that it's running: `:log-open` should mention `helix-log`, and line 1
of each file should have a `helix-log spike: test diagnostic` hint. Also try
`:lsp-workspace-command spike.ping`.

## Logs

In `playground/logs/` when run from the playground (set by
`LLM_LSP_SPIKE_DIR` in `.helix/languages.toml`). Otherwise
`${XDG_STATE_HOME:-~/.local/state}/llm_lsp/`:

- `helix-log.log`: one readable line per message. `<-` = from Helix,
  `->` = from us. Positions are 1-based like Helix's statusline. A blank line
  marks a gap of more than 2s.
- `helix-log.jsonl`: full messages, with document text truncated. Read it with
  `jq`, e.g. the client capabilities:
  `jq 'select(.msg.method=="initialize") | .msg.params.capabilities' helix-log.jsonl`

Watch it live, in another terminal:
`tail -f spikes/helix-log/playground/logs/helix-log.log`

## Session script

Do each step, note the timestamp, then check the log:

1. Open a file and move the cursor around without editing. Does anything arrive?
2. Type inside one function, then move to another and type there.
3. Stay in insert mode, pause, then press `Esc`.
4. Hover (`space k`) in a few places.
5. Open code actions (`space a`) and run the spike one.
6. Scroll a long file (inlay hints on).
7. Save. Switch buffers. Open a split. Close a buffer.
8. Undo/redo, a multi-cursor edit, and paste a big block.
9. Repeat 1–2 with `auto-document-highlight` and inlay hints turned off in
   `.helix/config.toml`, to see what we get without them.

Write results in `FINDINGS.md`.
