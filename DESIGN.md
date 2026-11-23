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
- Languages: Rust, C, C++, Python required. Also go, JavaScript, TypeScript/TSX,
  Java, C#, Ruby, Lua, Bash, Zig, where a tree-sitter grammar crate is
  available. Any other language falls back to line-window mode (see
  [Code structure](#code-structure)).

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
server. See [spike findings](spikes/helix-log/FINDINGS.md). The server infers
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
definitions, and Rust traits. Plain types are Rust struct/enum/union/type
alias, and C/C++ struct/union/enum without member functions. An anonymous C
struct is named by its `typedef`. Not yet: classes (C++ `class` or structs
with member functions, Python `class`), whose methods are covered one by one.
The plan is to review a class as a "header" with method bodies left out.
Modules and macros are out.

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
- Code action "Insert doc comment" writes the cached doc in the language's
  doc-comment style (`///`, `/** */`, docstring, …).
- Helix shows hovers from every server and lets you switch between them, so
  the plain `textDocument/hover` is enough.

### Notes

- The model may return `notes` with any response, e.g. "error handling in
  this crate uses `anyhow` at the boundary, `thiserror` inside". The server
  decides what to store: deduplicated, scoped to project / path / symbol, and
  capped in number.
- Relevant notes go into later prompts. Notes on deleted symbols are pruned.
  `llm.showNotes` / `llm.clearNotes` commands let me check what it believes.

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
| `notes` | id, scope | Model's project notes |
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

1. ~~What Helix actually sends~~: answered by the spike (edits and explicit
   requests only).
2. ~~How Helix shows hovers from multiple servers~~: all of them, switchable.
3. ~~`claude -p` cold-start latency~~: ≈1.4s, ≈2.7s for a whole doc. Fine
   on demand. Pre-generating docs for opened files isn't needed for now.
4. ~~Fingerprinting findings~~: hash of (category, normalized anchor).

## Build order

1. ~~Spike: logging LSP in Helix~~: done, see `spikes/helix-log/FINDINGS.md`.
2. LSP skeleton, document sync, tree-sitter symbol lookup for the 4 core
   languages.
3. ~~Hover docs + SQLite cache + `claude -p` runner~~: done.
4. Activity tracker, leave-event trigger, scheduler.
5. ~~Passive review → diagnostics → code actions → dismissals~~: done.
6. Notes.
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
