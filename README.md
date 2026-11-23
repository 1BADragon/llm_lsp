# llm_lsp

A language server that acts like a pair programmer. You keep writing the
code; it watches what you've just finished and only speaks up about real
problems. It also documents functions on demand.

It runs next to your normal language servers (rust-analyzer, clangd, …) and
drives your locally installed [Claude Code](https://docs.claude.com/en/docs/claude-code)
CLI headlessly. Built for [Helix](https://helix-editor.com); see
[DESIGN.md](DESIGN.md) for the design and the reasoning behind it.

## What it does

- **Passive review.** When you finish editing a function and go idle, it
  reviews that function in the background and shows real problems (bugs,
  leaks, races, security holes, serious performance traps) as diagnostics.
  Style nitpicks are deliberately excluded, and the usual answer is
  "nothing to report". Findings follow the code as you edit, and disappear
  once you change the flagged function.
- **Fixes and dismissals.** Code actions apply a suggested fix (when the
  model is sure of a small, local one) or dismiss a finding for good.
- **Hover docs.** Hover a function, method, type or trait to get generated
  documentation.
- **Caching.** Docs and reviews are stored in SQLite, keyed by the exact
  code (ignoring whitespace). Unchanged code is never sent to the model
  twice, even across restarts.
- **Project context.** Your `CLAUDE.md` and `AGENTS.md` are included in
  every prompt.
- **Notes.** While reviewing, the model can note durable facts about your
  project ("errors use `anyhow` at the boundary") for later reviews, and
  retire notes the code contradicts. You can read and edit them.

Supported languages: Rust, C, C++, Python.

## Requirements

- Rust (edition 2024) to build it.
- Claude Code installed as `claude` and signed in. Runs use your own
  account; nothing is bundled.
- Helix 25.07 or later. Other editors should work for the standard LSP
  parts, but only Helix has been tested.

## Install

```sh
cargo install --path .
```

This puts `llm_lsp` in `~/.cargo/bin`. Rerun it to update, then
`:lsp-restart` in Helix.

### Helix setup

Add the server to `~/.config/helix/languages.toml`, after each language's
usual servers. `language-servers` replaces Helix's default list, so list
the defaults too:

```toml
[language-server.llm-lsp]
command = "llm_lsp"
# Optional; see "Settings" below.
# config = { doc-model = "sonnet", max-reviews-per-hour = 10 }

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "llm-lsp"]

[[language]]
name = "c"
language-servers = ["clangd", "llm-lsp"]

[[language]]
name = "cpp"
language-servers = ["clangd", "llm-lsp"]

[[language]]
name = "python"
language-servers = ["pyright", "ruff", "llm-lsp"]
```

Check with `hx --health rust`.

## Using it

- **Reviews happen on their own.** Edit a function, then stop typing: about
  10s after you leave it, and once you've been idle for 10s, it's reviewed.
  A progress message shows while it runs. If you move on to another
  function and keep typing, reviews wait until you pause.
- **Hover** (`space k`) inside a function, method, type or trait. The first
  time, it says it's documenting; hover again a few seconds later. Helix
  shows every server's hover, so switch to the llm_lsp one if needed.
- **Code actions** (`space a`):
  - `llm_lsp: fix: …` applies a finding's suggested fix.
  - `llm_lsp: dismiss: …` hides a finding permanently. It survives
    rewording and reformatting.
  - ``llm_lsp: review `name` `` reviews the symbol under the cursor right now.
- **Commands** (`:lsp-workspace-command` opens a picker):
  - `llm.reviewSymbol`: review the function you last edited or hovered,
    now. It skips the idle wait and the hourly cap, and always reports the
    result.
  - `llm.showNotes`: open the notes as a Markdown file. Delete a line or
    edit its text and save to change them.
  - `llm.clearNotes`: delete all notes (asks first).

Bind a command to a key in `~/.config/helix/config.toml`:

```toml
[keys.normal.space]
R = ":lsp-workspace-command llm.reviewSymbol"
```

## Settings

Pass them as `config` under `[language-server.llm-lsp]`. All are optional:

| Setting | Default | Meaning |
|---|---|---|
| `claude` | `"claude"` | Path to the Claude Code CLI |
| `doc-model` | `"haiku"` | Model for hover docs |
| `doc-tools` | `["Read", "Grep", "Glob"]` | Read-only tools the doc writer may use |
| `reviews` | `true` | Passive review on/off |
| `review-model` | `"sonnet"` | Model for reviews |
| `review-tools` | `["Read", "Grep", "Glob"]` | Read-only tools the reviewer may use |
| `review-idle-secs` | `10` | Idle time before queued reviews start |
| `max-reviews-per-hour` | `20` | Cap on review runs (cache hits are free) |

Each run is isolated from your interactive Claude Code setup (no settings,
hooks, plugins, MCP servers or memory) and only gets the tools listed. Only
list read-only tools: they run without asking.

## Cost

Measured with Claude Code 2.1: a Haiku doc takes about 3s and costs about
$0.001, and a Sonnet review takes about 4s and costs about $0.01–0.03. Only
code you edit is reviewed, unchanged code comes from the cache, and the
hourly cap bounds the worst case (about $0.60/hour at the defaults). Set
`reviews = false` to turn passive review off.

## Data

One SQLite database per project in `~/.local/share/llm_lsp/` (or
`$XDG_DATA_HOME/llm_lsp/`), named by a hash of the project root. It holds
cached docs and reviews, dismissals, notes, and a log of every model run
(model, duration, cost). Deleting it resets everything for that project.

Logs go to Helix's log (`:log-open`). Set
`environment = { RUST_LOG = "llm_lsp=debug" }` under
`[language-server.llm-lsp]` for more detail.

## Development

```sh
cargo test                       # unit tests
cargo test -- --include-ignored  # also calls the real claude CLI (fractions of a cent)
playground/run.sh
```

`run.sh` builds the server and opens a small playground (Rust, C, C++ and
Python files with deliberate bugs) in Helix, with a workspace config that
uses the debug build. Run `:workspace-trust` in it once.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
