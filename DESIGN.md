# llm_lsp — Design

> Status: **Draft**. This document is the source of truth for the project's
> direction. Change goals, non-goals, or architecture here first (see
> [Decision log](#decision-log)).

## Problem

LLM coding tools mostly write code *for* you. I want to keep writing my own
code, but still get what a good pair programmer gives you: someone who spots the
real bug in a function you just finished, and who can explain or document a
function on demand.

## Goal

A language server that runs next to the normal language servers in the editor.
It uses an LLM to:

1. **Passively review** code I just finished working on and report real problems
   as diagnostics, with code-action fixes where possible.
2. **Document on demand**: hover over a function to get a generated doc
   comment, and use a code action to insert it.

### Goals

- Stay quiet unless something matters. A false positive costs more than a
  missed nitpick.
- Run only when I'm actually working (editing or making requests), and never
  while I'm in the middle of typing a function.
- Spend tokens carefully: cache everything that's keyed by content, and use
  cheap models by default.
- Keep the model informed by project context (`CLAUDE.md`, `AGENTS.md`, its own
  notes).
- Never modify files on its own. Every edit goes through a code action that I
  choose to apply.
- Use my locally installed, signed-in `claude` CLI, like vision does.

### Non-goals

- Completion or inline code generation.
- Replacing the real language server (types, go-to-definition, compiler
  diagnostics).
- Supporting every editor equally. **Helix comes first**; other editors only
  where standard LSP covers them for free.

## Target environment

- Editor: Helix (25.07+). It runs as an extra language server next to
  rust-analyzer, clangd, etc.
- Languages: Rust, C, C++, Python, JavaScript (incl. JSX), TypeScript and TSX
  are supported. Helix language ids `javascript`/`javascriptreact` share the
  JavaScript grammar, and `typescriptreact` is TSX. Candidates for later:
  Go, Java, C#, Ruby, Lua, Bash, Zig. HTML/CSS have no symbols to review;
  Vue/Svelte single-file components embed several languages and would need
  separate handling. Unsupported languages are simply ignored (the planned
  line-window fallback isn't built).

## Architecture overview

```
┌────────────┐ stdio JSON-RPC ┌──────────────────────────────────────────┐
│   Helix    │ ◄────────────► │  llm_lsp (Rust, tokio)                   │
└────────────┘                │                                          │
                              │  LSP layer ─► Workspace (docs + trees)   │
                              │      │              │                    │
                              │      ▼              ▼                    │
                              │  Activity tracker ─► Scheduler ─► Store  │
                              │                         │      (SQLite)  │
                              │                    ┌────▼─────┐          │
                              │                    │ LLM      │          │
                              │                    │ runner   │          │
                              │                    └────┬─────┘          │
                              └─────────────────────────┼────────────────┘
                                                        │ spawn
                                              `claude -p` (read-only tools)
```

### Components

- **LSP layer** (`tower-lsp-server`). Handles document sync (incremental,
  **utf-8** positions, which Helix supports), hover, code actions,
  execute-command, and diagnostics. Helix pulls diagnostics in bursts (15
  requests in under 3s at startup), so diagnostics are pushed and any pull
  request is answered from memory, never by starting LLM work. Uses
  `workDoneProgress` for a spinner while generating.
- **Workspace**. Keeps the open documents' text plus one tree-sitter tree per
  document, re-parsed incrementally on every change.
- **Code structure**. Maps a position to its enclosing *symbol* (function,
  method, impl/class) by walking up the syntax tree from the cursor and
  matching node kinds from a small per-language table. (The grammars' bundled
  tags queries were tried and dropped: their quirks needed as much special
  casing as the table.) Produces a
  `SymbolKey { lang, path, qualified_name, body_hash }`.
  Fallback for languages without a grammar: a line window around the edit.
- **Activity tracker**. Infers where I'm working (see below).
- **Scheduler**. Decides when to run a review, and enforces the debounce, the
  concurrency limit, and the budget.
- **LLM runner**. Spawns `claude -p`, builds prompts, and parses structured
  output.
- **Store** (`rusqlite`). Cache, findings, dismissals, notes, usage
  accounting.

## Knowing where I'm working

LSP doesn't report the cursor position. In Helix, a server listed after the
primary one (rust-analyzer, clangd, …) never sees cursor movement either:
single-server features like `documentHighlight` go only to the first capable
server. See the [appendix](#appendix-what-helix-sends-a-second-language-server). The server infers
focus from what it does get:

| Signal | Meaning |
|---|---|
| `didChange` ranges (one per keystroke) | Strongest signal: I'm editing *this symbol* |
| `completion` positions (after a typing pause) | Same, a little coarser |
| `hover`, `codeAction` positions | Explicit attention on this spot (on demand only) |
| `didOpen` / `didSave` | Which files are in play; save is a "checkpoint" |

Not available: cursor movement, buffer switches, `didClose`, viewport.
Moving somewhere without typing is invisible, so a leave event in that case
fires from the timeout alone.

**Ignore formatter edits.** Format on save arrives as one multi-range
`didChange` right before `didSave`. Edits like that don't move focus and don't
mark symbols as touched by me. (They still change body hashes, so cached
results for the reformatted code are looked up again.)

From these signals the tracker keeps a **focus symbol** and a list of
recently-touched symbols. The main trigger is:

> **Leave event**: a symbol I edited loses focus, meaning an edit or request
> lands in a different symbol or file, *or* there has been no signal inside it
> for *N* seconds (default ~10s, the only option when I move without typing).
> It is then queued for review.

**Explicit review**: `llm.reviewSymbol` runs immediately, skipping the idle
wait and the hourly budget (the cache still applies), and always reports
the outcome in a message. With no arguments
(`:lsp-workspace-command llm.reviewSymbol`, bindable to a key) it reviews the
tracker's focus. The code action "llm_lsp: review `name`" passes the
function under the cursor.

Not yet built: `didSave` queuing all dirty symbols in that file, and
`llm.reviewFile`.

**Idle gating.** Leave events only *queue* reviews. A review starts once
there has been no activity at all for `review-idle-secs` (default 10s). Reviews
run one at a time, and the next queued one starts only while I'm still idle.
So moving on to another function and continuing to type delays reviews of
the first until I pause. Only edited symbols are ever queued.

## Features

**What's reviewable (and documentable):** functions, methods, plain type
definitions, and traits/interfaces.
- Plain types are Rust struct/enum/union/type alias, C/C++ struct/union/enum
  without member functions, TS type aliases and enums, and JS/TS classes
  without methods. An anonymous C struct is named by its `typedef`.
- JS/TS functions include `const f = () => …` / `= function …`, and
  `export default function` (named `default` when anonymous). Methods
  include object-literal methods (`api.get`) and arrow-function class fields.
  Inline callbacks belong to their enclosing function. Definitions include
  their `export` / `const` / `declare` wrapper. A TS overload signature
  defers to the implementation.
- Not yet: classes with methods (C++ `class` or structs with member
  functions, Python and JS/TS classes with methods), whose methods are
  covered one by one. The plan is to review a class as a "header" with
  method bodies left out. Modules, namespaces and macros are out.

### Passive review → diagnostics

- Input: the symbol's text, the file around it (trimmed), project context
  (see below), and relevant notes. The model may read other files with
  read-only tools to check callers and types.
- Output: schema-checked JSON (`claude --json-schema`), a list of findings
  `{ severity: error|warning|info, confidence: high|medium|low, category,
  anchor, message, fix? }`. (`notes` not yet.)
- The prompt asks for real problems only (bugs, leaks, races, security,
  serious performance traps) and says an empty list is the expected answer.
  `error` is reserved for very likely real bugs.
- We then drop: `low` confidence, anchors that aren't a verbatim substring of
  the function (the model misquoted the code), and duplicates. We keep at most
  5, most severe first.
- Reviews are cached like docs: by (language, whitespace-insensitive body
  hash, prompt version). Re-reviewing unchanged code is free.
- Findings are anchored to the function plus the `anchor` text, not to line
  numbers. They are re-located after every edit, and hidden once the
  function's body hash changes, until it is reviewed again.
- `fix` (a replacement for exactly the anchor) becomes a quick-fix code
  action.
- A "Dismiss" code action (`llm.dismiss`) stores
  `(qualified_name, fingerprint)`, where fingerprint = hash(category,
  whitespace-normalized anchor). It survives rewording and reformatting, so
  the same finding isn't raised again.
- Budget: `max-reviews-per-hour` (default 20) counts actual model runs.
  Cache hits are free.
- Measured (Sonnet): ≈4s and ≈$0.03 per review.

### Hover docs

- `hover` on a symbol checks `docs` by `body_hash`. On a hit it returns
  immediately.
- On a miss it returns a short "generating…" hover at once and starts
  generation in the background. The next hover hits the cache.
- Code action "insert doc comment" writes the doc into the source:
  - Formats: Rust `///` lines placed above any `#[…]` attributes, C/C++ a
    `/** … */` block, Python a docstring as the first statement.
  - Indented to match, and wrapped to 80 columns. Lists get a hanging
    indent; code blocks, headings and tables are left alone.
  - An existing doc comment (`///` lines, `/** */`, a docstring) is replaced
    rather than duplicated, and the action says "replace". Plain comments
    are left alone.
  - Cached doc: the action carries the edit and applies instantly.
    Otherwise it's "generate and insert", which runs `llm.insertDoc`: it
    generates the doc (with progress), then sends `workspace/applyEdit`,
    with positions computed from the text at that moment.
- Helix shows hovers from every server and lets you switch between them, so
  the plain `textDocument/hover` is enough.

### Notes

- Reviews may return `notes` (scope `project` / `file` / `symbol` plus one
  sentence of text), e.g. "error handling in this crate uses `anyhow` at the
  boundary, `thiserror` inside". The prompt asks only for durable,
  non-obvious facts useful for reviewing *other* code later, and says
  usually there are none. The server attaches the file path and symbol from
  the review itself, so the model can't invent locations.
- Existing notes are shown with ids, and a review can retire wrong or stale
  ones via `obsolete_notes`. Only ids it was shown are accepted. Tested: a
  seeded false note was retired.
- Stored deduplicated by normalized text per place, with at most 3 new notes
  per review. Kept per place, newest first: 50 project-wide, 10 per file,
  5 per symbol. Ids are never reused.
- Relevant notes go into review *and* doc prompts: the 30 most recent
  project notes, plus the file's and the symbol's. Docs can't add notes.
- Pruned automatically: notes on symbols that no longer exist (checked
  before each review of that file), and on files that no longer exist
  (checked at startup).
- `llm.showNotes` writes all notes to `<db>.notes.md` and opens it in the
  editor. Deleting a line or editing its text, then saving, updates the notes
  (the server checks the file once a second). Only notes that were in the
  file can be deleted that way. `llm.clearNotes` deletes all notes after a
  confirmation prompt.

## Project context

Each prompt gets:
- `CLAUDE.md` (loaded by Claude Code automatically when `cwd` is the project
  root).
- `AGENTS.md` and other configured context files, injected by us.
- Relevant notes.

The model is told it is a *reviewer*. It is not writing the code.

## LLM runner

- `claude -p` with `cwd` = workspace root, `--output-format json`, our own
  short `--system-prompt` (replacing Claude Code's large default, which is
  faster and cheaper), and read-only tools only (Read, Grep, Glob). No Edit,
  Write, or Bash. Isolated from the user's Claude Code setup: no settings
  sources, MCP servers, slash commands or auto-memory. So `CLAUDE.md` /
  `AGENTS.md` are read by us and put in the prompt. The prompt goes on stdin.
- Measured (Claude Code 2.1, Haiku): CLI startup ≈1.4s; a hover doc ≈2.7s
  end to end at ≈$0.001.
- Per-task model: review → sonnet, hover docs → haiku. Both configurable.
- One process per task. At most *K* concurrent, default 1 for passive work
  and 1 reserved for on-demand work. On-demand always pre-empts passive.
- Cancellation: if a symbol changes while its review is running, kill the
  process and requeue.
- Budget: per-hour cap on passive runs. Usage is recorded in SQLite.

## Store (SQLite)

Database file at `$XDG_DATA_HOME/llm_lsp/<project-hash>.db`.

| Table | Key | Purpose |
|---|---|---|
| `docs` | `(lang, body_hash)` | Cached hover docs |
| `reviews` | `(lang, body_hash)` | Last review result for exactly this body |
| `dismissals` | `(qualified_name, fingerprint)` | Findings I rejected |
| `notes` | id; unique (scope, path, symbol, normalized text) | Model's project notes |
| `runs` | id | Model, duration, cost/tokens, outcome |

Because everything is keyed by content hash, an unchanged function is never
re-reviewed or re-documented, even across restarts and branch switches.

## Helix integration

```toml
# ~/.config/helix/languages.toml
[language-server.llm-lsp]
command = "llm_lsp"

# Optional; these are the defaults.
config = { claude = "claude", doc-model = "haiku", doc-tools = ["Read", "Grep", "Glob"],
           reviews = true, review-model = "sonnet", review-tools = ["Read", "Grep", "Glob"],
           review-idle-secs = 10, max-reviews-per-hour = 20 }

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "llm-lsp"]
# …repeat for each language
```

`config` arrives as `initializationOptions`.

Commands can be bound to keys with `:lsp-workspace-command`, e.g.
`"C-r" = ":lsp-workspace-command llm.reviewSymbol"`.

## Open questions

1. ~~What Helix actually sends~~: answered by measuring it (edits and explicit
   requests only).
2. ~~How Helix shows hovers from multiple servers~~: all of them, switchable.
3. ~~`claude -p` cold-start latency~~: ≈1.4s, ≈2.7s for a whole doc. Fine
   on demand. Pre-generating docs for opened files isn't needed for now.
4. ~~Fingerprinting findings~~: hash of (category, normalized anchor).

## Build order

1. ~~Spike: logging LSP in Helix~~: done; results in the appendix.
2. LSP skeleton, document sync, tree-sitter symbol lookup for the 4 core
   languages.
3. ~~Hover docs + SQLite cache + `claude -p` runner~~: done.
4. Activity tracker, leave-event trigger, scheduler.
5. ~~Passive review → diagnostics → code actions → dismissals~~: done.
6. ~~Notes~~: done.
7. Extra languages.

## Decision log

- 2026-11-22: Helix is the primary editor. Rust/C/C++/Python required, other
  common languages best-effort.
- 2026-11-22: Use the local `claude` CLI (subscription, automatic `CLAUDE.md`,
  agentic file reads) rather than calling the API directly.
- 2026-11-22: Review on *leaving* a symbol, not while typing in it.
- 2026-11-22: Run as a second-listed server in Helix and accept that there
  is no cursor tracking. Listing it first would hide clangd's / rust-analyzer's
  document highlights. Focus comes from edits and explicit requests.

## Appendix: what Helix sends a second language server

Measured on Helix 25.07.1 with a logging-only server listed *after*
clangd / rust-analyzer, over a few editing sessions (open, type, save,
hover, code actions, switching buffers, moving the cursor). The logging
server has since been removed. These results shaped
[Knowing where I'm working](#knowing-where-im-working).

### Setup

- Helix's workspace trust ignores `.helix/` until `:workspace-trust`. Servers
  that are already running don't pick up the config, so restart `hx` after
  trusting.
- Helix runs **one server process per workspace root**, shared by all
  languages (a C and a Rust file went to the same process).

### Signals a second-listed server receives

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

### Display

- **Hover**: Helix shows hovers from both servers and lets you switch between
  them. So hover docs can just use `textDocument/hover`.
- **Diagnostics**: a pushed Hint from the second server shows up alongside the
  others.

### Other details

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
