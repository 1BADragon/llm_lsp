//! Passive review: what to ask the model about a function or type I just
//! finished, and how to turn its answer into findings we trust enough to
//! show.

use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::docs;
use crate::lang::Lang;
use crate::notes::{self, NewNote, Note};
use crate::symbols::Symbol;

/// Bump whenever the prompt or schema changes meaningfully, so cached
/// reviews from the old prompt aren't reused.
pub const PROMPT_VERSION: i64 = 3;

/// More than this many findings on one function is noise.
const MAX_FINDINGS: usize = 5;

pub const SYSTEM_PROMPT: &str = "\
You are a senior engineer pair-programming with the author. They just \
finished editing one function, method, type definition or trait, and you \
glance over it for real problems. They write the code; you only point \
things out.

Report only issues that matter:
- bugs: wrong results, crashes, panics, undefined behavior, off-by-one, \
unhandled error or edge cases that will happen in practice
- resource leaks, races, security holes
- serious performance traps (e.g. accidentally quadratic on real inputs)
- in JavaScript/TypeScript: a missing `await`, unhandled promise \
rejections, stale closures or missing React hook dependencies, and `any` or \
`as` casts that hide a real type error
- for type definitions and traits: a field or variant type that can't \
hold the values it will need to, ownership or lifetime mistakes, a missing \
`Drop`/`Clone`/`Copy`/derive that callers in the provided code rely on, \
layout or size problems that matter (e.g. `#[repr]`, padding in C structs \
shared with other code), and invariants the definition can't uphold
Do NOT report style, formatting, naming, missing docs or comments, \
\"consider adding tests\", or speculative concerns. Most code has nothing \
worth saying: an empty list is the expected, good answer.

For each finding:
- severity: \"error\" for something that is very likely a real bug, \
\"warning\" for a probable problem in realistic cases, \"info\" for a \
notable performance trap or hazard.
- confidence: \"high\" or \"medium\" only if you'd defend it in review; \
otherwise use \"low\" (low findings are discarded).
- category: one word, e.g. bug, leak, race, security, performance.
- anchor: an exact, verbatim substring of the code under review (copied \
character for character) marking where the problem is: the smallest \
expression or single line that shows it, unique within that code.
- message: one or two plain sentences: what goes wrong and when.
- fix: optional. Replacement text for exactly the anchor, only when the fix \
is small, local and certain. Omit it otherwise.

The surrounding file and project context are provided. You may use the \
read-only tools to check how something used here behaves, but only when a \
finding depends on it.

You also keep notes for future reviews of this project, which may be of \
other code. Add a note only for a durable, non-obvious fact you learned \
that will help then: a project convention, an invariant callers rely on, \
what a helper guarantees, how errors are handled. This is especially \
useful for things you confirmed by reading other files. A note is not a \
finding, a summary of this code, or a guess. Keep each to one sentence. \
scope: \"symbol\" (about the code under review), \"file\" (its file) or \
\"project\". Usually there is nothing new to note.

Existing notes are listed with ids. If the code shows one is wrong or \
outdated, put its id in obsolete_notes.";

/// The reply must match this (passed as `--json-schema`).
pub const SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "severity": { "enum": ["error", "warning", "info"] },
          "confidence": { "enum": ["high", "medium", "low"] },
          "category": { "type": "string" },
          "anchor": { "type": "string" },
          "message": { "type": "string" },
          "fix": { "type": "string" }
        },
        "required": ["severity", "confidence", "category", "anchor", "message"]
      }
    },
    "notes": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "scope": { "enum": ["project", "file", "symbol"] },
          "text": { "type": "string" }
        },
        "required": ["scope", "text"]
      }
    },
    "obsolete_notes": { "type": "array", "items": { "type": "integer" } }
  },
  "required": ["findings"]
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub confidence: Confidence,
    pub category: String,
    /// Verbatim text from the function marking where the problem is.
    pub anchor: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Finding {
    /// Stable identity for dismissals: survives rewording of the message
    /// and reformatting of the code, but not a change to the flagged code.
    pub fn fingerprint(&self) -> String {
        let anchor: Vec<&str> = self.anchor.split_whitespace().collect();
        let key = format!("{}\0{}", self.category.to_lowercase(), anchor.join(" "));
        blake3::hash(key.as_bytes()).to_hex()[..16].to_string()
    }

    /// Where the anchor is within the function's text.
    pub fn locate(&self, symbol_text: &str) -> Option<Range<usize>> {
        let start = symbol_text.find(&self.anchor)?;
        Some(start..start + self.anchor.len())
    }
}

#[derive(Deserialize)]
struct ReviewOutput {
    findings: Vec<Finding>,
    #[serde(default)]
    notes: Vec<NewNote>,
    #[serde(default)]
    obsolete_notes: Vec<i64>,
}

/// A parsed, filtered review.
#[derive(Debug, Default, PartialEq)]
pub struct Review {
    pub findings: Vec<Finding>,
    pub notes: Vec<NewNote>,
    /// Ids of notes to delete, limited to the ones shown in the prompt.
    pub obsolete_notes: Vec<i64>,
}

pub struct ReviewRequest<'a> {
    pub lang: Lang,
    pub symbol: &'a Symbol,
    /// The file's path relative to the project root.
    pub path: &'a Path,
    pub file_text: &'a str,
    pub context: &'a [(String, String)],
    pub notes: &'a [Note],
}

pub fn prompt(req: &ReviewRequest) -> String {
    let sym = req.symbol;
    let mut out = docs::context_section(req.context);
    out.push_str(&notes::prompt_section(req.notes));
    out.push_str(&docs::file_section(req.path, req.lang, req.file_text, sym));
    out.push_str(&format!(
        "Review this {lang} {kind} that was just edited: `{name}` (lines {start}-{end} of the file):\n<code>\n{body}\n</code>\n",
        lang = req.lang.name(),
        kind = sym.kind.label(req.lang),
        name = sym.qualified_name,
        start = sym.start.row + 1,
        end = sym.end.row + 1,
        body = &req.file_text[sym.byte_range.clone()],
    ));
    out
}

/// Turns the model's structured reply into findings worth showing: drops
/// low-confidence ones, ones whose anchor isn't really in the code (the
/// model misquoted it), and duplicates, then caps the count. Notes are
/// cleaned, and obsolete ids limited to `shown_notes`.
pub fn parse(structured: serde_json::Value, symbol_text: &str, shown_notes: &[Note]) -> Result<Review> {
    let out: ReviewOutput =
        serde_json::from_value(structured).context("review output doesn't match the schema")?;
    let obsolete_notes = out
        .obsolete_notes
        .into_iter()
        .filter(|id| shown_notes.iter().any(|n| n.id == *id))
        .collect();
    let mut kept: Vec<Finding> = Vec::new();
    for mut f in out.findings {
        f.fix = f.fix.filter(|fix| fix != &f.anchor);
        if f.confidence == Confidence::Low {
            debug!(?f, "dropping low-confidence finding");
        } else if f.anchor.trim().is_empty() || f.locate(symbol_text).is_none() {
            debug!(?f, "dropping finding whose anchor isn't in the function");
        } else if kept.iter().any(|k| k.fingerprint() == f.fingerprint()) {
            debug!(?f, "dropping duplicate finding");
        } else {
            kept.push(f);
        }
    }
    // Most severe first, then most confident.
    kept.sort_by_key(|f| (f.severity as u8, f.confidence as u8));
    kept.truncate(MAX_FINDINGS);
    Ok(Review {
        findings: kept,
        notes: notes::validate(out.notes),
        obsolete_notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BODY: &str = "fn div(a: f64, b: f64) -> f64 {\n    a / b\n}";

    fn finding(anchor: &str, confidence: &str) -> serde_json::Value {
        json!({
            "severity": "warning", "confidence": confidence, "category": "bug",
            "anchor": anchor, "message": "b may be zero"
        })
    }

    #[test]
    fn schema_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(v["required"], json!(["findings"]));
    }

    #[test]
    fn keeps_good_findings_and_drops_bad_ones() {
        let out = json!({ "findings": [
            finding("a / b", "high"),
            finding("a / b", "medium"),       // duplicate
            finding("a/b", "high"),           // misquoted anchor
            finding("a / b", "low"),          // low confidence
            finding("", "high"),              // empty anchor
        ]});
        let kept = parse(out, BODY, &[]).unwrap().findings;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].anchor, "a / b");
        assert_eq!(kept[0].locate(BODY), Some(36..41));
    }

    #[test]
    fn sorts_by_severity_and_caps_the_count() {
        let mut items: Vec<_> = (0..8)
            .map(|i| {
                json!({
                    "severity": "info", "confidence": "high", "category": format!("c{i}"),
                    "anchor": "a / b", "message": "m"
                })
            })
            .collect();
        items.push(json!({
            "severity": "error", "confidence": "high", "category": "bug",
            "anchor": "b: f64", "message": "m"
        }));
        let kept = parse(json!({ "findings": items }), BODY, &[]).unwrap().findings;
        assert_eq!(kept.len(), MAX_FINDINGS);
        assert_eq!(kept[0].severity, Severity::Error);
    }

    #[test]
    fn a_fix_identical_to_the_anchor_is_no_fix() {
        let mut f = finding("a / b", "high");
        f["fix"] = json!("a / b");
        let kept = parse(json!({ "findings": [f] }), BODY, &[]).unwrap().findings;
        assert_eq!(kept[0].fix, None);
    }

    #[test]
    fn fingerprint_survives_rewording_and_reformatting() {
        let base = Finding {
            severity: Severity::Warning,
            confidence: Confidence::High,
            category: "bug".into(),
            anchor: "a / b".into(),
            message: "b may be zero".into(),
            fix: None,
        };
        let reworded = Finding { message: "division by zero".into(), ..base.clone() };
        let reformatted = Finding { anchor: "a  /\n b".into(), ..base.clone() };
        let changed = Finding { anchor: "a / c".into(), ..base.clone() };
        assert_eq!(base.fingerprint(), reworded.fingerprint());
        assert_eq!(base.fingerprint(), reformatted.fingerprint());
        assert_ne!(base.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn notes_are_cleaned_and_obsolete_ids_limited_to_shown_notes() {
        use crate::notes::NoteScope;
        let shown = [Note {
            id: 4,
            scope: NoteScope::Project,
            path: String::new(),
            symbol: String::new(),
            text: "old".into(),
        }];
        let out = json!({
            "findings": [],
            "notes": [{ "scope": "file", "text": "  all floats  " }, { "scope": "project", "text": "" }],
            "obsolete_notes": [4, 99]
        });
        let review = parse(out, BODY, &shown).unwrap();
        assert_eq!(review.notes, [NewNote { scope: NoteScope::File, text: "all floats".into() }]);
        assert_eq!(review.obsolete_notes, [4]);
    }

    #[test]
    fn notes_fields_are_optional() {
        let review = parse(json!({ "findings": [] }), BODY, &[]).unwrap();
        assert_eq!(review, Review::default());
    }

    #[test]
    fn prompt_includes_notes() {
        use crate::notes::NoteScope;
        use crate::symbols::SymbolKind;
        use tree_sitter::Point;
        let sym = Symbol {
            kind: SymbolKind::Function,
            name: "div".into(),
            qualified_name: "div".into(),
            byte_range: 0..BODY.len(),
            start: Point::new(0, 0),
            end: Point::new(2, 1),
        };
        let shown = [Note {
            id: 4,
            scope: NoteScope::Project,
            path: String::new(),
            symbol: String::new(),
            text: "b is never zero".into(),
        }];
        let p = prompt(&ReviewRequest {
            lang: Lang::Rust,
            symbol: &sym,
            path: Path::new("src/m.rs"),
            file_text: BODY,
            context: &[],
            notes: &shown,
        });
        assert!(p.contains("<notes>\n[4] (project) b is never zero\n</notes>"));
        assert!(p.contains("<code>\nfn div"));
    }

    #[test]
    fn rejects_malformed_output() {
        assert!(parse(json!({ "nope": [] }), BODY, &[]).is_err());
    }
}
