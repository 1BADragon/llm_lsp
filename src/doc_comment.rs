//! Turning a generated doc into a doc comment in the source: where it goes,
//! what it looks like in each language, and whether one is already there.

use std::ops::Range;

use tree_sitter::{Node, Tree};

use crate::lang::Lang;
use crate::symbols::Symbol;

/// Doc comments are wrapped to fit this many columns, counting indentation
/// and the comment prefix.
const WIDTH: usize = 80;
/// Never wrap narrower than this, however deep the indentation.
const MIN_TEXT_WIDTH: usize = 40;

/// A text edit (byte range in the current source, replacement text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEdit {
    pub range: Range<usize>,
    pub text: String,
    /// True when an existing doc comment is being replaced.
    pub replaces: bool,
}

/// The edit that puts `doc` on `symbol` as a doc comment, replacing the
/// existing one if there is one. `None` when there's nowhere sensible to put
/// it (e.g. a one-line Python `def f(): return 1`).
pub fn doc_edit(lang: Lang, tree: &Tree, source: &str, symbol: &Symbol, doc: &str) -> Option<DocEdit> {
    let doc = doc.trim();
    if doc.is_empty() {
        return None;
    }
    match lang {
        Lang::Python => python_edit(tree, source, symbol, doc),
        Lang::Rust | Lang::C | Lang::Cpp | Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            above_edit(lang, source, symbol, doc)
        }
    }
}

/// Rust / C / C++: the comment goes on the lines above the definition.
fn above_edit(lang: Lang, source: &str, symbol: &Symbol, doc: &str) -> Option<DocEdit> {
    let lines = line_spans(source);
    let def_line = symbol.start.row;
    let indent = leading_ws(&source[lines[def_line].clone()]);
    let text = match lang {
        Lang::Rust => rust_comment(&wrap(doc, text_width(indent, 4)), indent),
        _ => c_comment(&wrap(doc, text_width(indent, 3)), indent),
    };

    // Walk up over attributes (Rust `#[…]`) or decorators (JS/TS `@…`)
    // to find where docs are, or would go.
    let attribute = match lang {
        Lang::Rust => Some("#["),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => Some("@"),
        _ => None,
    };
    let mut top = def_line;
    while let Some(prefix) = attribute
        && top > 0
        && line(source, &lines, top - 1).starts_with(prefix)
    {
        top -= 1;
    }
    // An existing doc comment ends right above `top` (Rust docs usually sit
    // above the attributes) or right above the definition itself.
    for anchor in [top, def_line] {
        if let Some(existing) = existing_doc_above(lang, source, &lines, anchor) {
            let range = lines[existing.start].start..lines[existing.end - 1].end;
            return Some(DocEdit { range, text, replaces: true });
        }
    }
    let at = lines[top].start;
    Some(DocEdit { range: at..at, text, replaces: false })
}

/// The lines (`start..end`, exclusive) of a doc comment ending just above
/// line `anchor`.
fn existing_doc_above(lang: Lang, source: &str, lines: &[Range<usize>], anchor: usize) -> Option<Range<usize>> {
    let end = anchor;
    let mut start = anchor;
    // `///` lines (Rust and C/C++ alike).
    while start > 0 && is_line_doc(line(source, lines, start - 1)) {
        start -= 1;
    }
    if start < end {
        return Some(start..end);
    }
    // A `/** … */` block (C / C++).
    if lang != Lang::Rust && end > 0 && line(source, lines, end - 1).ends_with("*/") {
        let mut s = end - 1;
        loop {
            let l = line(source, lines, s);
            if l.starts_with("/**") || l.starts_with("/*!") {
                return Some(s..end);
            }
            if l.starts_with("/*") || s == 0 || end - s > 200 {
                return None; // a plain comment, or no opener in reach
            }
            s -= 1;
        }
    }
    None
}

fn is_line_doc(trimmed: &str) -> bool {
    // `/// <reference … />` is a TypeScript directive, not documentation.
    trimmed.starts_with("///") && !trimmed.starts_with("////") && !trimmed.starts_with("/// <")
}

fn rust_comment(doc: &str, indent: &str) -> String {
    doc.lines()
        .map(|l| {
            if l.trim().is_empty() {
                format!("{indent}///\n")
            } else {
                format!("{indent}/// {l}\n")
            }
        })
        .collect()
}

fn c_comment(doc: &str, indent: &str) -> String {
    let mut out = format!("{indent}/**\n");
    for l in doc.lines() {
        let l = l.replace("*/", "* /");
        if l.trim().is_empty() {
            out.push_str(&format!("{indent} *\n"));
        } else {
            out.push_str(&format!("{indent} * {l}\n"));
        }
    }
    out.push_str(&format!("{indent} */\n"));
    out
}

/// Python: a docstring as the first statement of the body.
fn python_edit(tree: &Tree, source: &str, symbol: &Symbol, doc: &str) -> Option<DocEdit> {
    let def = definition_node(tree, symbol)?;
    let body = def.child_by_field_name("body")?;
    let first = body.named_child(0)?;
    if first.start_position().row == def.start_position().row {
        return None; // `def f(): return 1` — no room for a docstring
    }
    let lines = line_spans(source);
    let first_line = first.start_position().row;
    let indent = leading_ws(&source[lines[first_line].clone()]);
    let docstring = python_docstring(&wrap(doc, text_width(indent, 3)), indent);

    if is_docstring(first) {
        // Replace the whole existing statement line(s), keeping what follows.
        let range = lines[first_line].start..lines[first.end_position().row].end;
        return Some(DocEdit { range, text: docstring, replaces: true });
    }
    let at = lines[first_line].start;
    Some(DocEdit { range: at..at, text: docstring, replaces: false })
}

/// The `function_definition` / `class_definition` node of `symbol`, looking
/// through a `decorated_definition` wrapper.
fn definition_node<'t>(tree: &'t Tree, symbol: &Symbol) -> Option<Node<'t>> {
    let mut node = tree
        .root_node()
        .descendant_for_byte_range(symbol.byte_range.start, symbol.byte_range.end)?;
    if node.kind() == "decorated_definition" {
        node = node.child_by_field_name("definition")?;
    }
    matches!(node.kind(), "function_definition" | "class_definition").then_some(node)
}

fn is_docstring(stmt: Node) -> bool {
    stmt.kind() == "expression_statement"
        && stmt.named_child_count() == 1
        && stmt.named_child(0).is_some_and(|c| c.kind() == "string")
}

fn python_docstring(doc: &str, indent: &str) -> String {
    let doc = doc.replace("\"\"\"", "\\\"\\\"\\\"");
    let mut lines = doc.lines();
    let first = lines.next().unwrap_or_default();
    let rest: Vec<&str> = lines.collect();
    if rest.is_empty() {
        return format!("{indent}\"\"\"{first}\"\"\"\n");
    }
    let mut out = format!("{indent}\"\"\"{first}\n");
    for l in rest {
        if l.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str(&format!("{indent}{l}\n"));
        }
    }
    out.push_str(&format!("{indent}\"\"\"\n"));
    out
}

fn text_width(indent: &str, prefix: usize) -> usize {
    WIDTH.saturating_sub(indent.len() + prefix).max(MIN_TEXT_WIDTH)
}

/// Wraps Markdown prose to `width`: paragraphs and list items are filled
/// (list items with a hanging indent); code blocks, indented lines, headings
/// and tables are left as they are.
fn wrap(doc: &str, width: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in doc.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            continue;
        }
        let verbatim = in_fence
            || line.starts_with("    ")
            || line.starts_with('\t')
            || trimmed.starts_with('#')
            || trimmed.starts_with('|')
            || trimmed.is_empty();
        if verbatim || line.len() <= width {
            out.push(line.to_string());
            continue;
        }
        // Hanging indent for list items: "- ", "* ", "1. ".
        let lead = line.len() - trimmed.len();
        let marker = trimmed.split_once(' ').map(|(m, _)| m).filter(|m| is_list_marker(m));
        let hang = lead + marker.map_or(0, |m| m.len() + 1);

        let mut current = String::new();
        for word in line.split_whitespace() {
            if current.is_empty() {
                current = format!("{}{word}", " ".repeat(lead));
            } else if current.len() + 1 + word.len() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                out.push(std::mem::take(&mut current));
                current = format!("{}{word}", " ".repeat(hang));
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out.join("\n")
}

fn is_list_marker(m: &str) -> bool {
    matches!(m, "-" | "*" | "+")
        || (m.len() > 1 && m.ends_with('.') && m[..m.len() - 1].chars().all(|c| c.is_ascii_digit()))
}

/// Byte range of each line, including its `\n`.
fn line_spans(source: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, _) in source.match_indices('\n') {
        spans.push(start..i + 1);
        start = i + 1;
    }
    spans.push(start..source.len());
    spans
}

fn line<'s>(source: &'s str, lines: &[Range<usize>], i: usize) -> &'s str {
    source[lines[i].clone()].trim()
}

fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::enclosing_symbol;
    use tree_sitter::Parser;

    /// Applies the doc edit for the symbol at `|` and returns the new source.
    fn apply(lang: Lang, src: &str, doc: &str) -> (String, bool) {
        let byte = src.find('|').unwrap();
        let source = src.replacen('|', "", 1);
        let mut parser = Parser::new();
        parser.set_language(&lang.ts_language()).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let sym = enclosing_symbol(lang, &tree, &source, byte).unwrap();
        let edit = doc_edit(lang, &tree, &source, &sym, doc).expect("an edit");
        let mut out = source.clone();
        out.replace_range(edit.range, &edit.text);
        (out, edit.replaces)
    }

    const DOC: &str = "Adds two numbers.\n\nNever overflows in practice.";

    #[test]
    fn rust_inserts_above_attributes_with_indent() {
        let (out, replaces) = apply(
            Lang::Rust,
            "impl P {\n    #[inline]\n    #[must_use]\n    fn add(&self) -> i32 { |1 }\n}\n",
            DOC,
        );
        assert!(!replaces);
        assert_eq!(
            out,
            "impl P {\n    /// Adds two numbers.\n    ///\n    /// Never overflows in practice.\n    #[inline]\n    #[must_use]\n    fn add(&self) -> i32 { 1 }\n}\n"
        );
    }

    #[test]
    fn rust_replaces_existing_docs() {
        let (out, replaces) = apply(
            Lang::Rust,
            "/// Old.\n/// Older.\n#[inline]\nfn f() { | }\n\n/// Other.\nfn g() {}\n",
            "New.",
        );
        assert!(replaces);
        assert_eq!(out, "/// New.\n#[inline]\nfn f() {  }\n\n/// Other.\nfn g() {}\n");
        // Docs between the attributes and the item count too.
        let (out, replaces) = apply(Lang::Rust, "#[inline]\n/// Old.\nfn f() { | }\n", "New.");
        assert!(replaces);
        assert_eq!(out, "#[inline]\n/// New.\nfn f() {  }\n");
    }

    #[test]
    fn rust_ignores_plain_comments_and_documents_types() {
        let (out, replaces) = apply(Lang::Rust, "// a plain comment\nstruct S { a: |u8 }\n", "A thing.");
        assert!(!replaces);
        assert_eq!(out, "// a plain comment\n/// A thing.\nstruct S { a: u8 }\n");
    }

    #[test]
    fn c_uses_a_block_and_replaces_doc_blocks_only() {
        let (out, replaces) = apply(Lang::C, "int f(void) {\n    return |0;\n}\n", "Returns zero.");
        assert!(!replaces);
        assert_eq!(out, "/**\n * Returns zero.\n */\nint f(void) {\n    return 0;\n}\n");

        let (out, replaces) = apply(Lang::C, "/**\n * Old.\n */\nint f(void) { |return 0; }\n", "New.");
        assert!(replaces);
        assert_eq!(out, "/**\n * New.\n */\nint f(void) { return 0; }\n");

        let (_, replaces) = apply(Lang::C, "/* license header */\nint f(void) { |return 0; }\n", "New.");
        assert!(!replaces, "a plain block comment isn't a doc comment");

        let (out, _) = apply(Lang::C, "int f(void) { |return 0; }\n", "Ends with */ oops.");
        assert!(out.contains(" * Ends with * / oops.\n"));
    }

    #[test]
    fn cpp_indents_methods_and_covers_templates() {
        let (out, _) = apply(Lang::Cpp, "class L {\n  int get() { return |1; }\n};\n", "Gets.");
        assert_eq!(out, "class L {\n  /**\n   * Gets.\n   */\n  int get() { return 1; }\n};\n");
        let (out, replaces) = apply(
            Lang::Cpp,
            "/// Old.\ntemplate <class T>\nT id(T t) { return |t; }\n",
            "Returns its argument.",
        );
        assert!(replaces);
        assert!(out.starts_with("/**\n * Returns its argument.\n */\ntemplate <class T>\n"));
    }

    #[test]
    fn python_docstrings() {
        let (out, replaces) = apply(Lang::Python, "def f():\n    return |1\n", "Returns one.");
        assert!(!replaces);
        assert_eq!(out, "def f():\n    \"\"\"Returns one.\"\"\"\n    return 1\n");

        let (out, replaces) = apply(
            Lang::Python,
            "class A:\n    @property\n    def v(self):\n        \"\"\"Old.\n\n        Older.\n        \"\"\"\n        return |1\n",
            DOC,
        );
        assert!(replaces);
        assert_eq!(
            out,
            "class A:\n    @property\n    def v(self):\n        \"\"\"Adds two numbers.\n\n        Never overflows in practice.\n        \"\"\"\n        return 1\n"
        );

        let (out, _) = apply(Lang::Python, "def f():\n    |pass\n", "Has \"\"\" inside.");
        assert!(out.contains(r#""""Has \"\"\" inside.""""#));
    }

    #[test]
    fn python_one_liner_has_no_room() {
        let source = "def f(): return 1\n";
        let mut parser = Parser::new();
        parser.set_language(&Lang::Python.ts_language()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let sym = enclosing_symbol(Lang::Python, &tree, source, 10).unwrap();
        assert_eq!(doc_edit(Lang::Python, &tree, source, &sym, "Doc."), None);
    }

    #[test]
    fn wraps_prose_and_lists_but_not_code() {
        let long = "word ".repeat(30);
        let doc = format!(
            "{long}\n\n- {long}\n\n```\n{long}\n```\n    {long}\n# A heading that is long enough to exceed the width surely yes\nshort line"
        );
        let wrapped = wrap(&doc, 40);
        let lines: Vec<&str> = wrapped.lines().collect();
        // Prose and the list item are filled; the list continues with a hang.
        assert!(lines[0].len() <= 40 && lines[0].starts_with("word"));
        let item = lines.iter().position(|l| l.starts_with("- ")).unwrap();
        assert!(lines[item + 1].starts_with("  word"));
        // Fenced, indented and heading lines are untouched.
        assert!(lines.contains(&long.as_str()));
        assert!(lines.iter().any(|l| l.starts_with("    word") && l.len() > 40));
        assert!(lines.iter().any(|l| l.starts_with("# A heading")));
        assert_eq!(lines.last(), Some(&"short line"));
        // No words lost.
        let count = |t: &str| t.split_whitespace().filter(|w| *w == "word").count();
        assert_eq!(count(&wrapped), count(&doc));
    }

    #[test]
    fn inserted_comments_fit_in_80_columns() {
        let doc = "Parses a single operand: a number, a variable, a unary minus applied to an operand, or a parenthesized expression.";
        let (out, _) = apply(Lang::Rust, "impl P {\n    fn atom(&self) { |1 }\n}\n", doc);
        for l in out.lines() {
            assert!(l.len() <= 80, "{l:?} is {} wide", l.len());
        }
        assert!(out.contains("    /// Parses a single operand"));
        let (out, _) = apply(Lang::Python, "class I:\n    def remove(self):\n        |pass\n", doc);
        assert!(out.lines().all(|l| l.len() <= 80));
        assert!(out.contains("        \"\"\"Parses a single operand"), "{out}");
        assert!(out.contains("\n        \"\"\"\n"), "multi-line docstring closes on its own line: {out}");
    }

    #[test]
    fn js_jsdoc_goes_above_export_and_decorators() {
        let (out, replaces) = apply(Lang::TypeScript, "export const load = async () => { |go(); };\n", "Loads it.");
        assert!(!replaces);
        assert_eq!(out, "/**\n * Loads it.\n */\nexport const load = async () => { go(); };\n");

        let (out, _) = apply(Lang::TypeScript, "class D {\n  @log\n  method() { | }\n}\n", "Does it.");
        assert_eq!(out, "class D {\n  /**\n   * Does it.\n   */\n  @log\n  method() {  }\n}\n");
    }

    #[test]
    fn js_replaces_jsdoc_but_not_reference_directives() {
        let (out, replaces) = apply(
            Lang::JavaScript,
            "/**\n * Old.\n * @param {number} x\n */\nfunction f(x) { return |x; }\n",
            "New.",
        );
        assert!(replaces);
        assert_eq!(out, "/**\n * New.\n */\nfunction f(x) { return x; }\n");

        let (out, replaces) = apply(
            Lang::TypeScript,
            "/// <reference types=\"node\" />\nfunction f() { | }\n",
            "New.",
        );
        assert!(!replaces, "a triple-slash directive isn't a doc comment");
        assert!(out.starts_with("/// <reference types=\"node\" />\n/**\n * New.\n */\nfunction f()"));
    }
}
