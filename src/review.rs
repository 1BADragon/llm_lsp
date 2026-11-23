//! Passive review: what to ask the model about a function I just finished,
//! and how to turn its answer into findings we trust enough to show.

use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::docs;
use crate::lang::Lang;
use crate::symbols::Symbol;

/// Bump whenever the prompt or schema changes meaningfully, so cached
/// reviews from the old prompt aren't reused.
pub const PROMPT_VERSION: i64 = 1;

/// More than this many findings on one function is noise.
const MAX_FINDINGS: usize = 5;

pub const SYSTEM_PROMPT: &str = "\
You are a senior engineer pair-programming with the author. They just \
finished editing one function and you glance over it for real problems. \
They write the code; you only point things out.

Report only issues that matter:
- bugs: wrong results, crashes, panics, undefined behavior, off-by-one, \
unhandled error or edge cases that will happen in practice
- resource leaks, races, security holes
- serious performance traps (e.g. accidentally quadratic on real inputs)
Do NOT report style, formatting, naming, missing docs or comments, \
\"consider adding tests\", or speculative concerns. Most functions have \
nothing worth saying: an empty list is the expected, good answer.

For each finding:
- severity: \"error\" for something that is very likely a real bug, \
\"warning\" for a probable problem in realistic cases, \"info\" for a \
notable performance trap or hazard.
- confidence: \"high\" or \"medium\" only if you'd defend it in review; \
otherwise use \"low\" (low findings are discarded).
- category: one word, e.g. bug, leak, race, security, performance.
- anchor: an exact, verbatim substring of the function (copied character \
for character) marking where the problem is: the smallest expression or \
single line that shows it, unique within the function.
- message: one or two plain sentences: what goes wrong and when.
- fix: optional. Replacement text for exactly the anchor, only when the fix \
is small, local and certain. Omit it otherwise.

The surrounding file and project context are provided. You may use the \
read-only tools to check how something used here behaves, but only when a \
finding depends on it.";

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
    }
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
}

pub struct ReviewRequest<'a> {
    pub lang: Lang,
    pub symbol: &'a Symbol,
    /// The file's path relative to the project root.
    pub path: &'a Path,
    pub file_text: &'a str,
    pub context: &'a [(String, String)],
}

pub fn prompt(req: &ReviewRequest) -> String {
    let sym = req.symbol;
    let mut out = docs::context_section(req.context);
    out.push_str(&docs::file_section(req.path, req.lang, req.file_text, sym));
    out.push_str(&format!(
        "Review this {lang} {kind} that was just edited: `{name}` (lines {start}-{end} of the file):\n<function>\n{body}\n</function>\n",
        lang = req.lang.name(),
        kind = format!("{:?}", sym.kind).to_lowercase(),
        name = sym.qualified_name,
        start = sym.start.row + 1,
        end = sym.end.row + 1,
        body = &req.file_text[sym.byte_range.clone()],
    ));
    out
}

/// Turns the model's structured reply into findings worth showing: drops
/// low-confidence ones, ones whose anchor isn't really in the function (the
/// model misquoted the code), and duplicates, then caps the count.
pub fn parse(structured: serde_json::Value, symbol_text: &str) -> Result<Vec<Finding>> {
    let out: ReviewOutput =
        serde_json::from_value(structured).context("review output doesn't match the schema")?;
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
    Ok(kept)
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
        let kept = parse(out, BODY).unwrap();
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
        let kept = parse(json!({ "findings": items }), BODY).unwrap();
        assert_eq!(kept.len(), MAX_FINDINGS);
        assert_eq!(kept[0].severity, Severity::Error);
    }

    #[test]
    fn a_fix_identical_to_the_anchor_is_no_fix() {
        let mut f = finding("a / b", "high");
        f["fix"] = json!("a / b");
        let kept = parse(json!({ "findings": [f] }), BODY).unwrap();
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
    fn rejects_malformed_output() {
        assert!(parse(json!({ "nope": [] }), BODY).is_err());
    }
}
