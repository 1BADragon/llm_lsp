//! Code structure: which symbol (function, method, class, …) a position is in.
//!
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │ STUB: yours to implement. Run the spec with                         │
//! │     cargo test symbols -- --include-ignored                         │
//! │ and see it live in Helix: hover anywhere in the playground and the  │
//! │ skeleton's hover shows what `enclosing_symbol` returned.            │
//! └─────────────────────────────────────────────────────────────────────┘
//!
//! Everything downstream keys off this: the activity tracker uses it to tell
//! "same function" from "moved on", hover docs use it to choose what to
//! document, and the cache keys on the symbol's text.

use std::ops::Range;

use tree_sitter::{Point, Tree};

use crate::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    /// Free function.
    Function,
    /// Function inside an impl / class / trait.
    Method,
    /// struct / enum / union / class / type alias.
    Class,
    /// Rust trait.
    Interface,
    Module,
    Macro,
}

impl SymbolKind {
    /// Functions and methods are what gets reviewed and documented.
    pub fn is_callable(self) -> bool {
        matches!(self, SymbolKind::Function | SymbolKind::Method)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub kind: SymbolKind,
    /// The bare name, e.g. `term`.
    pub name: String,
    /// Name plus its containers, joined the way the language writes it:
    /// `Parser::term` (Rust, C++), `Inventory.add` (Python), `ring_push` (C).
    pub qualified_name: String,
    /// The whole definition node, including the body.
    pub byte_range: Range<usize>,
    pub start: Point,
    pub end: Point,
}

/// Returns the innermost symbol enclosing `byte`.
///
/// Rules (the tests below are the spec):
/// - Prefer the innermost **callable** (function/method) that contains `byte`.
///   A closure or lambda is not a symbol. It belongs to its enclosing function.
/// - If no callable contains `byte`, return the innermost non-callable
///   definition (struct field, class body, …), else `None`.
/// - "Contains" is inclusive of the whole definition node, so the signature
///   and a doc-less header count as inside the function.
///
/// Hints, not requirements:
/// - `lang.tags_query()` is each grammar's bundled tags query. Its
///   `@definition.<kind>` captures mark definition nodes and `@name` marks their
///   names. Run it with a `tree_sitter::QueryCursor`, using `set_byte_range` to
///   limit the work.
/// - The bundled queries have quirks. In Rust, a method matches *both* the
///   `definition.method` and the `definition.function` pattern, and an
///   `impl Foo { … }` block isn't a definition at all, so `Foo::` has to come
///   from walking up the tree (`node.parent()`) to the `impl_item`. Check what
///   the C++ and Python queries do with methods and classes.
/// - `tree-sitter` has `Node::to_sexp()`, and Helix has
///   `:tree-sitter-subtree`, for looking at real trees.
pub fn enclosing_symbol(lang: Lang, tree: &Tree, source: &str, byte: usize) -> Option<Symbol> {
    let _ = (lang, tree, source, byte);
    None // TODO: implement
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    /// Parses `src` with the `|` cursor marker removed, and looks up the
    /// symbol at the marker.
    fn at_cursor(lang: Lang, src: &str) -> Option<Symbol> {
        let byte = src.find('|').expect("test source needs a | cursor");
        let source = src.replacen('|', "", 1);
        let mut parser = Parser::new();
        parser.set_language(&lang.ts_language()).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        enclosing_symbol(lang, &tree, &source, byte)
    }

    fn expect(lang: Lang, src: &str, kind: SymbolKind, qualified: &str) {
        let sym = at_cursor(lang, src).unwrap_or_else(|| panic!("no symbol for:\n{src}"));
        assert_eq!((sym.kind, sym.qualified_name.as_str()), (kind, qualified), "\n{src}");
        let source = src.replacen('|', "", 1);
        assert!(
            source[sym.byte_range.clone()].contains(&sym.name),
            "byte_range should cover the definition"
        );
    }

    #[test]
    #[ignore = "stub: implement symbols::enclosing_symbol"]
    fn rust() {
        use SymbolKind::*;
        let l = Lang::Rust;
        expect(l, "fn main() { let x = |1; }", Function, "main");
        expect(l, "fn ma|in() {}", Function, "main");
        expect(l, "struct P;\nimpl P {\n    fn term(&self) { 1 + |2 }\n}", Method, "P::term");
        expect(l, "impl<'a> Parser<'a> { fn next(&mut self) { | } }", Method, "Parser::next");
        expect(l, "trait T { fn f(&self) { | } }", Method, "T::f");
        expect(l, "mod m { fn f() { | } }", Function, "m::f");
        expect(l, "fn outer() { let c = || { | }; }", Function, "outer");
        expect(l, "fn outer() { fn inner() { | } }", Function, "outer::inner");
        expect(l, "struct S { a: |u8 }", Class, "S");
        assert_eq!(at_cursor(l, "use std::fmt;|\n"), None);
    }

    #[test]
    #[ignore = "stub: implement symbols::enclosing_symbol"]
    fn c() {
        use SymbolKind::*;
        let l = Lang::C;
        expect(l, "int ring_push(int *r) {\n    return |0;\n}", Function, "ring_push");
        expect(l, "static void *alloc(size_t n) { | }", Function, "alloc");
        assert_eq!(at_cursor(l, "#include <stdio.h>|\n"), None);
    }

    #[test]
    #[ignore = "stub: implement symbols::enclosing_symbol"]
    fn cpp() {
        use SymbolKind::*;
        let l = Lang::Cpp;
        expect(l, "class Lru {\n  int get(int k) { return |k; }\n};", Method, "Lru::get");
        expect(
            l,
            "template <typename K> class Lru { void put(K k) { | } };",
            Method,
            "Lru::put",
        );
        expect(l, "namespace ns { int f() { | } }", Function, "ns::f");
        expect(l, "int Lru::size() const { return |0; }", Method, "Lru::size");
        expect(l, "int main() { auto f = [] { | }; }", Function, "main");
    }

    #[test]
    #[ignore = "stub: implement symbols::enclosing_symbol"]
    fn python() {
        use SymbolKind::*;
        let l = Lang::Python;
        expect(l, "def main():\n    x = |1\n", Function, "main");
        expect(l, "class Inventory:\n    def add(self):\n        |pass\n", Method, "Inventory.add");
        expect(l, "class A:\n    x = |1\n", Class, "A");
        expect(l, "def f():\n    g = lambda: |1\n", Function, "f");
        assert_eq!(at_cursor(l, "import os|\n"), None);
    }
}
