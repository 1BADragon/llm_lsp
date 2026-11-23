//! Hover docs: what to ask the model, and how to clean up its answer.

use std::path::Path;

use crate::lang::Lang;
use crate::symbols::Symbol;

/// Bump whenever `SYSTEM_PROMPT` or `prompt` changes meaningfully, so cached
/// docs from the old prompt are regenerated rather than served.
pub const PROMPT_VERSION: i64 = 1;

pub const SYSTEM_PROMPT: &str = "\
You write documentation for source code. It is shown in an editor hover and \
may later be inserted as the symbol's doc comment.

Write the documentation for the one symbol you are given:
- Start with a one-sentence summary of what it does and why a caller would \
use it, not a line-by-line account of how.
- Add more only when it helps a caller: non-obvious behavior, parameters, \
return value, errors or exceptions, panics, safety requirements, side \
effects. Follow the language's documentation conventions (e.g. Rust \
`# Errors` / `# Panics` / `# Safety` sections, Python Args/Returns/Raises).
- Be brief: usually 1-6 lines. Don't restate the signature or types.
- Don't guess. The surrounding file is provided. If behavior depends on code \
elsewhere, you may look it up with the read-only tools, but only when it \
matters.
- Reply with only the documentation text, in Markdown. No comment markers \
(///, #, /** */), no code fence around the whole reply, no preamble.";

/// Context files read from the project root, if present.
pub const CONTEXT_FILES: [&str; 2] = ["CLAUDE.md", "AGENTS.md"];
const MAX_CONTEXT_FILE: usize = 16 * 1024;
const MAX_FILE: usize = 40 * 1024;
/// When the file is too big, how many lines either side of the symbol to keep.
const EXCERPT_LINES: usize = 150;

pub struct DocRequest<'a> {
    pub lang: Lang,
    pub symbol: &'a Symbol,
    /// The file's path relative to the project root.
    pub path: &'a Path,
    pub file_text: &'a str,
    /// `(file name, contents)` of project context files.
    pub context: &'a [(String, String)],
}

/// Cache key for a symbol's text. Whitespace runs are collapsed first, so
/// reformatting or re-indenting doesn't cause a regeneration.
pub fn body_hash(text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    for word in text.split_whitespace() {
        hasher.update(word.as_bytes());
        hasher.update(b" ");
    }
    hasher.finalize().to_hex().to_string()
}

pub fn prompt(req: &DocRequest) -> String {
    let sym = req.symbol;
    let lang = req.lang.name();
    let mut out = String::new();

    for (name, text) in req.context {
        out.push_str(&format!(
            "Project context from {name}:\n<context file=\"{name}\">\n{}\n</context>\n\n",
            truncate(text, MAX_CONTEXT_FILE)
        ));
    }

    out.push_str(&format!(
        "File: {} ({lang})\n<file>\n{}\n</file>\n\n",
        req.path.display(),
        file_excerpt(req.file_text, sym)
    ));
    out.push_str(&format!(
        "Document this {lang} {kind}: `{name}` (lines {start}-{end} of the file):\n<symbol>\n{body}\n</symbol>\n",
        kind = format!("{:?}", sym.kind).to_lowercase(),
        name = sym.qualified_name,
        start = sym.start.row + 1,
        end = sym.end.row + 1,
        body = &req.file_text[sym.byte_range.clone()],
    ));
    out
}

/// The whole file when it's small, else the lines around the symbol.
fn file_excerpt(text: &str, sym: &Symbol) -> String {
    if text.len() <= MAX_FILE {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let first = sym.start.row.saturating_sub(EXCERPT_LINES);
    let last = (sym.end.row + EXCERPT_LINES).min(lines.len().saturating_sub(1));
    format!(
        "[excerpt: lines {}-{} of {}]\n{}",
        first + 1,
        last + 1,
        lines.len(),
        lines[first..=last].join("\n")
    )
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Strips things models add despite instructions: a code fence around the
/// whole reply, or comment markers on every line.
pub fn clean(reply: &str) -> String {
    let mut text = reply.trim();
    if text.starts_with("```") && text.ends_with("```") && text.len() > 6 {
        let inner = &text[3..text.len() - 3];
        // Drop the fence's language tag line.
        text = inner.split_once('\n').map_or(inner, |(_, rest)| rest).trim();
    }
    let lines: Vec<&str> = text.lines().collect();
    for marker in ["///", "//!", "#"] {
        if !lines.is_empty() && lines.iter().all(|l| l.trim_start().starts_with(marker)) {
            return lines
                .iter()
                .map(|l| {
                    let rest = &l.trim_start()[marker.len()..];
                    rest.strip_prefix(' ').unwrap_or(rest)
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::SymbolKind;
    use tree_sitter::Point;

    #[test]
    fn hash_ignores_formatting_but_not_code() {
        let a = body_hash("fn f() {\n    a + b\n}");
        assert_eq!(a, body_hash("fn f() { a + b }"));
        assert_eq!(a, body_hash("fn f() {\n\ta + b\n}\n"));
        assert_ne!(a, body_hash("fn f() { a - b }"));
    }

    #[test]
    fn prompt_contains_context_file_and_symbol() {
        let file = "use x;\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let start = file.find("fn add").unwrap();
        let sym = Symbol {
            kind: SymbolKind::Function,
            name: "add".into(),
            qualified_name: "add".into(),
            byte_range: start..file.len() - 1,
            start: Point::new(2, 0),
            end: Point::new(4, 1),
        };
        let context = [("CLAUDE.md".to_string(), "Use i32 everywhere.".to_string())];
        let p = prompt(&DocRequest {
            lang: Lang::Rust,
            symbol: &sym,
            path: Path::new("src/math.rs"),
            file_text: file,
            context: &context,
        });
        assert!(p.contains("<context file=\"CLAUDE.md\">\nUse i32 everywhere."));
        assert!(p.contains("File: src/math.rs (rust)\n<file>\nuse x;"));
        assert!(p.contains("Document this rust function: `add` (lines 3-5"));
        assert!(p.contains("<symbol>\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n</symbol>"));
    }

    #[test]
    fn big_files_are_excerpted_around_the_symbol() {
        let file: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let start = file.find("line 2500\n").unwrap();
        let sym = Symbol {
            kind: SymbolKind::Function,
            name: "f".into(),
            qualified_name: "f".into(),
            byte_range: start..start + 9,
            start: Point::new(2500, 0),
            end: Point::new(2500, 9),
        };
        let ex = file_excerpt(&file, &sym);
        assert!(ex.starts_with("[excerpt: lines 2351-2651 of 5000]\nline 2350\n"));
        assert!(ex.ends_with("line 2650"));
    }

    #[test]
    fn clean_strips_fences_and_comment_markers() {
        assert_eq!(clean("  Adds two numbers.\n"), "Adds two numbers.");
        assert_eq!(clean("```markdown\nAdds.\n\nMore.\n```"), "Adds.\n\nMore.");
        assert_eq!(clean("/// Adds.\n///\n/// More."), "Adds.\n\nMore.");
        assert_eq!(clean("# Adds.\n# More."), "Adds.\nMore.");
        // A Markdown heading inside normal text is left alone.
        assert_eq!(clean("Adds.\n\n# Errors\nNever."), "Adds.\n\n# Errors\nNever.");
    }
}
