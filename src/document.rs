//! An open document: its text, and (for supported languages) an
//! incrementally maintained tree-sitter tree.

use std::ops::Range;

use tower_lsp_server::ls_types::{Position, TextDocumentContentChangeEvent};
use tree_sitter::{InputEdit, Parser, Point, Tree};

use crate::lang::Lang;

/// How LSP `Position::character` is counted. Negotiated in `initialize`;
/// Helix supports UTF-8, which matches tree-sitter's byte columns exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

pub struct Document {
    pub lang: Option<Lang>,
    pub version: i32,
    text: String,
    /// Byte offset of the start of each line. Always non-empty.
    line_starts: Vec<usize>,
    parser: Option<Parser>,
    tree: Option<Tree>,
}

impl Document {
    pub fn new(lang: Option<Lang>, version: i32, text: String) -> Self {
        let parser = lang.map(|l| {
            let mut p = Parser::new();
            p.set_language(&l.ts_language())
                .expect("grammar ABI is compatible with tree-sitter");
            p
        });
        let mut doc = Document {
            lang,
            version,
            line_starts: line_starts(&text),
            text,
            parser,
            tree: None,
        };
        doc.reparse();
        doc
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Applies one `didChange` notification's changes, in order, then
    /// reparses incrementally.
    ///
    /// Returns the byte ranges the changes produced in the *new* text (one per
    /// change, adjusted for later changes in the same batch). A deletion is an
    /// empty range at the deletion point.
    pub fn apply_changes(
        &mut self,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
        enc: Encoding,
    ) -> Vec<Range<usize>> {
        let mut spans: Vec<Range<usize>> = Vec::with_capacity(changes.len());
        for change in changes {
            let Some(range) = change.range else {
                // Full-document sync.
                self.text = change.text.clone();
                self.line_starts = line_starts(&self.text);
                self.tree = None;
                spans.clear();
                spans.push(0..self.text.len());
                continue;
            };
            let start = self.offset_at(range.start, enc);
            let old_end = self.offset_at(range.end, enc).max(start);
            let start_position = self.point_at(start);
            let old_end_position = self.point_at(old_end);

            self.text.replace_range(start..old_end, &change.text);
            self.line_starts = line_starts(&self.text);
            let new_end = start + change.text.len();
            let new_end_position = self.point_at(new_end);

            if let Some(tree) = &mut self.tree {
                tree.edit(&InputEdit {
                    start_byte: start,
                    old_end_byte: old_end,
                    new_end_byte: new_end,
                    start_position,
                    old_end_position,
                    new_end_position,
                });
            }

            // Shift earlier spans that sit after this edit.
            let delta = new_end as isize - old_end as isize;
            for s in &mut spans {
                if s.start >= old_end {
                    s.start = s.start.saturating_add_signed(delta);
                    s.end = s.end.saturating_add_signed(delta);
                } else if s.end > start {
                    // Overlaps this edit: clamp into the new text.
                    s.start = s.start.min(start);
                    s.end = new_end.max(s.start);
                }
            }
            spans.push(start..new_end);
        }
        self.version = version;
        self.reparse();
        spans
    }

    fn reparse(&mut self) {
        let Some(parser) = &mut self.parser else {
            return;
        };
        self.tree = parser.parse(&self.text, self.tree.as_ref());
    }

    /// LSP position → byte offset, clamped to the line / document.
    pub fn offset_at(&self, pos: Position, enc: Encoding) -> usize {
        let line = pos.line as usize;
        let Some(&line_start) = self.line_starts.get(line) else {
            return self.text.len();
        };
        let next_line = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        // Past-the-end columns clamp to the end of the line, not into the
        // line terminator.
        let line_text = self.text[line_start..next_line].trim_end_matches(['\n', '\r']);
        let col = pos.character as usize;
        let in_line = match enc {
            Encoding::Utf8 => {
                let mut c = col.min(line_text.len());
                while !line_text.is_char_boundary(c) {
                    c -= 1;
                }
                c
            }
            Encoding::Utf16 => {
                let mut units = 0;
                let mut bytes = 0;
                for ch in line_text.chars() {
                    if units >= col {
                        break;
                    }
                    units += ch.len_utf16();
                    bytes += ch.len_utf8();
                }
                bytes
            }
        };
        line_start + in_line
    }

    /// Byte offset → LSP position.
    #[allow(dead_code)] // for publishing diagnostics (build step 5)
    pub fn position_at(&self, offset: usize, enc: Encoding) -> Position {
        let p = self.point_at(offset);
        let character = match enc {
            Encoding::Utf8 => p.column,
            Encoding::Utf16 => {
                let line_start = self.line_starts[p.row];
                self.text[line_start..line_start + p.column]
                    .chars()
                    .map(char::len_utf16)
                    .sum()
            }
        };
        Position::new(p.row as u32, character as u32)
    }

    /// Byte offset → tree-sitter point (row, byte column).
    pub fn point_at(&self, offset: usize) -> Point {
        let offset = offset.min(self.text.len());
        let row = self.line_starts.partition_point(|&s| s <= offset) - 1;
        Point::new(row, offset - self.line_starts[row])
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Range as LspRange;

    fn change(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(LspRange::new(Position::new(sl, sc), Position::new(el, ec))),
            range_length: None,
            text: text.into(),
        }
    }

    fn fresh_sexp(doc: &Document) -> String {
        Document::new(doc.lang, 0, doc.text().to_string())
            .tree()
            .unwrap()
            .root_node()
            .to_sexp()
    }

    #[test]
    fn incremental_edits_match_a_fresh_parse() {
        let mut doc = Document::new(Some(Lang::Rust), 0, "fn a() {}\nfn b() {}\n".into());
        // Like Helix: one keystroke per notification.
        for (i, ch) in "let x = 1;".chars().enumerate() {
            let col = 8 + i as u32;
            doc.apply_changes(i as i32 + 1, &[change(0, col, 0, col, &ch.to_string())], Encoding::Utf8);
        }
        assert_eq!(doc.text(), "fn a() {let x = 1;}\nfn b() {}\n");
        assert_eq!(doc.tree().unwrap().root_node().to_sexp(), fresh_sexp(&doc));
    }

    #[test]
    fn multi_range_change_applies_in_order_and_reports_shifted_spans() {
        // A formatter-style batch: re-indent two lines.
        let mut doc = Document::new(None, 0, "a\n    b\n    c\n".into());
        let spans = doc.apply_changes(
            1,
            &[change(1, 0, 1, 4, "  "), change(2, 0, 2, 4, "  ")],
            Encoding::Utf8,
        );
        assert_eq!(doc.text(), "a\n  b\n  c\n");
        assert_eq!(spans, vec![2..4, 6..8]);
    }

    #[test]
    fn utf16_positions_count_code_units() {
        let doc = Document::new(None, 0, "x = \"é😀\"; y\n".into());
        // 'y' is at UTF-16 column 11 (😀 is two units) but byte offset 14.
        let byte = doc.text().find('y').unwrap();
        assert_eq!(byte, 14);
        assert_eq!(doc.offset_at(Position::new(0, 11), Encoding::Utf16), byte);
        assert_eq!(doc.position_at(byte, Encoding::Utf16), Position::new(0, 11));
        assert_eq!(doc.position_at(byte, Encoding::Utf8), Position::new(0, byte as u32));
    }

    #[test]
    fn out_of_range_positions_clamp() {
        let doc = Document::new(None, 0, "ab\ncd".into());
        assert_eq!(doc.offset_at(Position::new(0, 99), Encoding::Utf8), 2);
        assert_eq!(doc.offset_at(Position::new(9, 0), Encoding::Utf8), 5);
    }
}
