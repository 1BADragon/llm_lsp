//! Notes: durable facts the reviewer learned about the project (a
//! convention, an invariant, what a helper guarantees). Reviews add and
//! retire them; relevant ones go into later review and doc prompts.
//!
//! I can see and edit them as a Markdown file (`llm.showNotes`): deleting a
//! line or editing its text and saving updates the notes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Longest note kept; longer ones are dropped, not truncated.
const MAX_LEN: usize = 300;
/// New notes accepted from one review.
const MAX_PER_REVIEW: usize = 3;
/// Project-wide notes included in one prompt (most recent first).
pub const MAX_PROJECT_IN_PROMPT: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteScope {
    Project,
    File,
    Symbol,
}

impl NoteScope {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteScope::Project => "project",
            NoteScope::File => "file",
            NoteScope::Symbol => "symbol",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "project" => Some(NoteScope::Project),
            "file" => Some(NoteScope::File),
            "symbol" => Some(NoteScope::Symbol),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: i64,
    pub scope: NoteScope,
    /// File path relative to the project root; empty for project notes.
    pub path: String,
    /// Qualified name; empty unless `scope` is `Symbol`.
    pub symbol: String,
    pub text: String,
}

/// A note as the model writes it. Where it applies (path, symbol) comes from
/// the review it was written in, never from the model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewNote {
    pub scope: NoteScope,
    pub text: String,
}

/// Trims, drops empty or overlong notes, and caps how many one review adds.
pub fn validate(notes: Vec<NewNote>) -> Vec<NewNote> {
    notes
        .into_iter()
        .map(|n| NewNote {
            text: n.text.split_whitespace().collect::<Vec<_>>().join(" "),
            ..n
        })
        .filter(|n| !n.text.is_empty() && n.text.chars().count() <= MAX_LEN)
        .take(MAX_PER_REVIEW)
        .collect()
}

/// De-duplication key: case, whitespace and a trailing period don't matter.
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches('.')
        .to_lowercase()
}

/// The notes as a prompt section, with ids so a review can retire them.
/// Empty when there are none.
pub fn prompt_section(notes: &[Note]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Notes from earlier reviews of this project (they may be outdated):\n<notes>\n",
    );
    for n in notes {
        let scope = match n.scope {
            NoteScope::Project => "project".to_string(),
            NoteScope::File => format!("file {}", n.path),
            NoteScope::Symbol => format!("`{}` in {}", n.symbol, n.path),
        };
        out.push_str(&format!("[{}] ({scope}) {}\n", n.id, n.text));
    }
    out.push_str("</notes>\n\n");
    out
}

/// The editable Markdown view of all notes.
pub fn render(project: &str, notes: &[Note]) -> String {
    let mut out = format!(
        "# llm_lsp notes: {project}\n\n\
         What the reviewer has learned about this project. Relevant notes are\n\
         added to review and doc prompts.\n\n\
         To change them, delete a line or edit its text, then save. Lines\n\
         without an `[n…]` id are ignored. Notes added after this file was\n\
         opened aren't shown; run `llm.showNotes` again to refresh.\n"
    );
    if notes.is_empty() {
        out.push_str("\n_No notes yet._\n");
        return out;
    }
    let mut sections: BTreeMap<(NoteScope, &str, &str), Vec<&Note>> = BTreeMap::new();
    for n in notes {
        sections.entry((n.scope, &n.path, &n.symbol)).or_default().push(n);
    }
    for ((scope, path, symbol), list) in sections {
        let heading = match scope {
            NoteScope::Project => "Project".to_string(),
            NoteScope::File => format!("`{path}`"),
            NoteScope::Symbol => format!("`{path}` › `{symbol}`"),
        };
        out.push_str(&format!("\n## {heading}\n\n"));
        for n in list {
            out.push_str(&format!("- [n{}] {}\n", n.id, n.text));
        }
    }
    out
}

/// Reads `(id, text)` back from an edited view. Lines that don't look like
/// `- [n12] text` are ignored.
pub fn parse_view(markdown: &str) -> Vec<(i64, String)> {
    markdown
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- [n")?;
            let (id, text) = rest.split_once(']')?;
            let id = id.parse().ok()?;
            let text = text.trim();
            (!text.is_empty()).then(|| (id, text.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, scope: NoteScope, path: &str, symbol: &str, text: &str) -> Note {
        Note {
            id,
            scope,
            path: path.into(),
            symbol: symbol.into(),
            text: text.into(),
        }
    }

    #[test]
    fn validate_cleans_and_caps() {
        let n = |text: &str| NewNote { scope: NoteScope::Project, text: text.into() };
        let kept = validate(vec![
            n("  Uses   anyhow\nat the boundary. "),
            n(""),
            n(&"x".repeat(301)),
            n("b"),
            n("c"),
            n("d"),
        ]);
        let texts: Vec<_> = kept.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts, ["Uses anyhow at the boundary.", "b", "c"]);
    }

    #[test]
    fn normalize_ignores_case_space_and_final_period() {
        assert_eq!(normalize("Uses  anyhow."), normalize("uses anyhow"));
        assert_ne!(normalize("uses anyhow"), normalize("uses thiserror"));
    }

    #[test]
    fn prompt_section_lists_ids_and_scopes() {
        assert_eq!(prompt_section(&[]), "");
        let s = prompt_section(&[
            note(3, NoteScope::Project, "", "", "anyhow at boundaries"),
            note(7, NoteScope::Symbol, "src/p.rs", "P::f", "never called with 0"),
        ]);
        assert!(s.contains("[3] (project) anyhow at boundaries\n"));
        assert!(s.contains("[7] (`P::f` in src/p.rs) never called with 0\n"));
    }

    #[test]
    fn render_and_parse_round_trip() {
        let notes = [
            note(7, NoteScope::Symbol, "src/p.rs", "P::f", "never called with 0"),
            note(3, NoteScope::Project, "", "", "anyhow at boundaries"),
            note(5, NoteScope::File, "src/p.rs", "", "all indices are byte offsets"),
        ];
        let md = render("/repo", &notes);
        // Grouped: project first, then files, then symbols.
        let project = md.find("## Project").unwrap();
        let file = md.find("## `src/p.rs`\n").unwrap();
        let symbol = md.find("## `src/p.rs` › `P::f`").unwrap();
        assert!(project < file && file < symbol);

        let mut parsed = parse_view(&md);
        parsed.sort();
        assert_eq!(
            parsed,
            [
                (3, "anyhow at boundaries".into()),
                (5, "all indices are byte offsets".into()),
                (7, "never called with 0".into()),
            ]
        );
    }

    #[test]
    fn parse_view_ignores_other_lines() {
        let md = "# title\n- [n4] kept\n- a new idea\n- [nx] bad id\n  - [n9]   indented  \n- [n2]\n";
        assert_eq!(parse_view(md), [(4, "kept".into()), (9, "indented".into())]);
    }

    #[test]
    fn empty_view_says_so() {
        assert!(render("/repo", &[]).contains("_No notes yet._"));
    }
}
