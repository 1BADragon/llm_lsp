//! Code structure: which symbol (function, method, class, …) a position is in.
//!
//! Everything downstream keys off this: the activity tracker uses it to tell
//! "same function" from "moved on", hover docs use it to choose what to
//! document, and the cache keys on the symbol's text.
//!
//! # How it works
//!
//! tree-sitter gives a syntax tree where every node has a `kind()` (a string
//! like `"function_item"`) and a parent. To find what a byte is inside:
//!
//! 1. Take the smallest node covering the byte
//!    (`descendant_for_byte_range`).
//! 2. Walk up through its parents. The first one that is a function or method
//!    is the answer. Remember the first other definition (struct, class, …)
//!    passed on the way, in case no function is found.
//! 3. Name it: most definitions have a `name` field. C/C++ functions hide the
//!    name inside a chain of declarators (`*f(int)` → `f`).
//! 4. Qualify it: keep walking up and prepend each container's name (module,
//!    `impl` type, class, namespace, enclosing function).
//!
//! Each language's node kinds are in the `match`es below. To look at real
//! trees, select some code in Helix and run `:tree-sitter-subtree`.

use std::ops::Range;

use tree_sitter::{Node, Point, Tree};

use crate::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    /// Free function.
    Function,
    /// Function inside an impl / class / trait.
    Method,
    /// A plain data type: struct / enum / union / type alias without member
    /// functions in its body.
    Type,
    /// A class with methods in its body: C++ `class` (or a struct with member
    /// functions), Python `class`. Its methods are symbols of their own.
    Class,
    /// Rust trait.
    Interface,
    Module,
    Macro,
}

impl SymbolKind {
    pub fn is_callable(self) -> bool {
        matches!(self, SymbolKind::Function | SymbolKind::Method)
    }

    /// What gets reviewed and documented: functions, methods, plain types
    /// and traits. Classes are left out for now (their methods are covered
    /// one by one), and so are modules and macros.
    pub fn is_reviewable(self) -> bool {
        matches!(
            self,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Type | SymbolKind::Interface
        )
    }

    /// How prompts and messages name this kind.
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Type => "type definition",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "trait",
            SymbolKind::Module => "module",
            SymbolKind::Macro => "macro",
        }
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
    /// The whole definition, including the body and any decorators or
    /// `template <…>` header.
    pub byte_range: Range<usize>,
    pub start: Point,
    pub end: Point,
}

/// Returns the innermost symbol enclosing `byte`.
///
/// - Prefers the innermost **callable** (function/method) containing `byte`.
///   Closures and lambdas aren't symbols. They belong to their enclosing
///   function.
/// - Otherwise returns the innermost other definition (struct, class, …),
///   else `None`.
/// - The whole definition counts, signature included.
pub fn enclosing_symbol(lang: Lang, tree: &Tree, source: &str, byte: usize) -> Option<Symbol> {
    let start = tree.root_node().descendant_for_byte_range(byte, byte)?;

    let mut found = None;
    let mut fallback = None;
    let mut node = Some(start);
    while let Some(n) = node {
        if let Some(kind) = symbol_kind(lang, n) {
            if kind.is_callable() {
                found = Some((n, kind));
                break;
            }
            fallback.get_or_insert((n, kind));
        }
        node = n.parent();
    }
    let (node, kind) = found.or(fallback)?;
    Some(make_symbol(lang, node, kind, source))
}

/// Finds the reviewable symbol whose qualified name is `qualified_name`,
/// wherever it is now. Used to re-find a symbol after time has passed and
/// the code may have moved. Returns the first match if names collide (e.g.
/// overloads, or a C struct tag and a function sharing a name).
pub fn find_reviewable(lang: Lang, tree: &Tree, source: &str, qualified_name: &str) -> Option<Symbol> {
    // Cheap pre-filter: the bare name must appear in the file.
    let bare = qualified_name.rsplit(separator(lang)).next()?;
    if !source.contains(bare) {
        return None;
    }
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if let Some(kind) = symbol_kind(lang, node)
            && kind.is_reviewable()
            && symbol_name(lang, node, source).ends_with(bare)
        {
            let sym = make_symbol(lang, node, kind, source);
            if sym.qualified_name == qualified_name {
                return Some(sym);
            }
        }
        stack.extend(node.named_children(&mut cursor));
    }
    None
}

fn make_symbol(lang: Lang, node: Node, kind: SymbolKind, source: &str) -> Symbol {
    // `full` may already be qualified, e.g. C++'s out-of-line `Lru::size`.
    let full = symbol_name(lang, node, source);
    let name = full.rsplit(separator(lang)).next().unwrap_or(&full).to_string();

    let mut parts = vec![full];
    let mut ancestor = node.parent();
    while let Some(a) = ancestor {
        if let Some(container) = container_name(lang, a, source) {
            parts.push(container);
        }
        ancestor = a.parent();
    }
    parts.reverse();

    let outer = with_wrapper(node);
    Symbol {
        kind,
        name,
        qualified_name: parts.join(separator(lang)),
        byte_range: outer.byte_range(),
        start: outer.start_position(),
        end: outer.end_position(),
    }
}

fn separator(lang: Lang) -> &'static str {
    match lang {
        Lang::Python => ".",
        Lang::Rust | Lang::C | Lang::Cpp => "::",
    }
}

/// Is `node` a definition, and of what kind?
fn symbol_kind(lang: Lang, node: Node) -> Option<SymbolKind> {
    use SymbolKind::*;
    let kind = match (lang, node.kind()) {
        // `function_signature_item` is a trait method without a body.
        (Lang::Rust, "function_item" | "function_signature_item") => {
            if parent_body_of(node, &["impl_item", "trait_item"]) {
                Method
            } else {
                Function
            }
        }
        (Lang::Rust, "struct_item" | "enum_item" | "union_item" | "type_item") => Type,
        (Lang::Rust, "trait_item") => Interface,
        (Lang::Rust, "mod_item") => Module,
        (Lang::Rust, "macro_definition") => Macro,

        (Lang::C, "function_definition") => Function,

        (Lang::Cpp, "function_definition") => {
            let in_class = parent_body_of(
                node,
                &["class_specifier", "struct_specifier", "union_specifier"],
            );
            // `int Lru::size() { … }` defined outside the class. (A
            // namespaced free function written `ns::f() { … }` also lands
            // here. That's rare enough not to matter.)
            let out_of_line = declarator_name_node(node)
                .is_some_and(|n| n.kind() == "qualified_identifier");
            if in_class || out_of_line {
                Method
            } else {
                Function
            }
        }
        (Lang::Cpp, "class_specifier") => Class,

        // A struct/union/enum only defines something when it has a body:
        // `struct foo *p;` merely mentions one. In C++ a struct with member
        // functions is really a class.
        (Lang::C | Lang::Cpp, "struct_specifier" | "union_specifier" | "enum_specifier") => {
            let body = node.child_by_field_name("body")?;
            if lang == Lang::Cpp && has_member_functions(body) {
                Class
            } else {
                Type
            }
        }

        (Lang::Python, "function_definition") => {
            if parent_body_of(node, &["class_definition"]) {
                Method
            } else {
                Function
            }
        }
        (Lang::Python, "class_definition") => Class,

        _ => return None,
    };
    Some(kind)
}

/// Name of an ancestor that contributes to qualified names: any definition,
/// plus scopes that aren't symbols themselves (Rust `impl`, C++ namespaces).
fn container_name(lang: Lang, node: Node, source: &str) -> Option<String> {
    match (lang, node.kind()) {
        // `impl<'a> Parser<'a>` and `impl fmt::Display for Token` both
        // qualify by the type being implemented: `Parser`, `Token`.
        (Lang::Rust, "impl_item") => node
            .child_by_field_name("type")
            .map(|t| rust_type_name(t, source)),
        // Anonymous namespaces add nothing.
        (Lang::Cpp, "namespace_definition") => node
            .child_by_field_name("name")
            .map(|n| text(n, source).to_string()),
        _ => symbol_kind(lang, node).map(|_| symbol_name(lang, node, source)),
    }
}

fn symbol_name(lang: Lang, node: Node, source: &str) -> String {
    let name_node = match (lang, node.kind()) {
        (Lang::C | Lang::Cpp, "function_definition") => declarator_name_node(node),
        _ => node.child_by_field_name("name"),
    };
    match name_node {
        Some(n) => strip_template_args(text(n, source)),
        // `typedef struct { … } ring_t;` is known by its typedef name.
        None => typedef_name(node, source).unwrap_or_else(|| "<anonymous>".to_string()),
    }
}

fn typedef_name(node: Node, source: &str) -> Option<String> {
    let parent = node.parent().filter(|p| p.kind() == "type_definition")?;
    let declarator = parent.child_by_field_name("declarator")?;
    (declarator.kind() == "type_identifier").then(|| text(declarator, source).to_string())
}

/// Does a C++ struct/union body define or declare member functions?
fn has_member_functions(body: Node) -> bool {
    let mut cursor = body.walk();
    body.named_children(&mut cursor).any(|member| match member.kind() {
        "function_definition" | "template_declaration" => true,
        "field_declaration" | "declaration" => member
            .child_by_field_name("declarator")
            .is_some_and(|d| d.kind() == "function_declarator"),
        _ => false,
    })
}

/// C/C++ bury a function's name in nested declarators:
/// `static void *alloc(size_t n)` is
/// `pointer_declarator → function_declarator → identifier`.
fn declarator_name_node(function: Node) -> Option<Node> {
    let mut node = function.child_by_field_name("declarator")?;
    loop {
        match node.kind() {
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator" => {
                // reference_declarator has no `declarator` field; its target
                // is the last named child.
                let last = node.named_child_count().checked_sub(1)?;
                node = node
                    .child_by_field_name("declarator")
                    .or_else(|| node.named_child(last as _))?;
            }
            _ => return Some(node),
        }
    }
}

/// `Parser<'a>` → `Parser`, `fmt::Display` → `fmt::Display`.
fn rust_type_name(node: Node, source: &str) -> String {
    match node.kind() {
        "generic_type" => node
            .child_by_field_name("type")
            .map(|t| rust_type_name(t, source))
            .unwrap_or_else(|| text(node, source).to_string()),
        _ => text(node, source).to_string(),
    }
}

/// Is `node` directly in the body of one of `containers`? Looks through
/// wrappers (decorators, `template <…>`) and the body node itself.
fn parent_body_of(node: Node, containers: &[&str]) -> bool {
    let Some(body) = with_wrapper(node).parent() else {
        return false;
    };
    // Body nodes: Rust `declaration_list`, C++ `field_declaration_list`,
    // Python `block`.
    body.parent().is_some_and(|c| containers.contains(&c.kind()))
}

/// The definition including wrapper nodes that belong to it: decorators,
/// `template <…>`, and a `typedef` around a struct.
fn with_wrapper(node: Node) -> Node {
    match node.parent() {
        Some(p) if matches!(p.kind(), "decorated_definition" | "template_declaration") => p,
        Some(p) if p.kind() == "type_definition" && p.child_by_field_name("type") == Some(node) => p,
        _ => node,
    }
}

/// `LruCache<K, V>::get` → `LruCache::get`.
fn strip_template_args(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn text<'s>(node: Node, source: &'s str) -> &'s str {
    &source[node.byte_range()]
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
            sym.name == "<anonymous>" || source[sym.byte_range.clone()].contains(&sym.name),
            "byte_range should cover the definition"
        );
    }

    #[test]
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
        expect(l, "struct S { a: |u8 }", Type, "S");
        expect(l, "enum E { A, |B }", Type, "E");
        expect(l, "type Id = |u64;", Type, "Id");
        expect(l, "trait T { fn f(&self)|; }", Method, "T::f");
        expect(l, "trait T {\n    |\n}", Interface, "T");
        assert_eq!(at_cursor(l, "use std::fmt;|\n"), None);
    }

    #[test]
    fn c() {
        use SymbolKind::*;
        let l = Lang::C;
        expect(l, "int ring_push(int *r) {\n    return |0;\n}", Function, "ring_push");
        expect(l, "static void *alloc(size_t n) { | }", Function, "alloc");
        expect(l, "typedef struct { int |x; } ring_t;", Type, "ring_t");
        expect(l, "struct node { struct node *|next; };", Type, "node");
        expect(l, "enum color { RED, |GREEN };", Type, "color");
        let anon = at_cursor(l, "struct { int |x; } v;").unwrap();
        assert_eq!((anon.kind, anon.qualified_name.as_str()), (Type, "<anonymous>"));
        assert_eq!(at_cursor(l, "#include <stdio.h>|\n"), None);
    }

    #[test]
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
        expect(l, "struct Pt { int |x, y; };", Type, "Pt");
        expect(l, "struct Pt { int |x; int len() const; };", Class, "Pt");
        expect(l, "struct Pt { int |x; int len() { return x; } };", Class, "Pt");
        expect(l, "class Lru { int |n; };", Class, "Lru");
        expect(l, "int Lru::size() const { return |0; }", Method, "Lru::size");
        expect(l, "int main() { auto f = [] { | }; }", Function, "main");
        expect(l, "int &pick(int &a) { return |a; }", Function, "pick");
        expect(
            l,
            "template <class K, class V> V Lru<K, V>::get(K k) { | }",
            Method,
            "Lru::get",
        );
    }

    #[test]
    fn python() {
        use SymbolKind::*;
        let l = Lang::Python;
        expect(l, "def main():\n    x = |1\n", Function, "main");
        expect(l, "class Inventory:\n    def add(self):\n        |pass\n", Method, "Inventory.add");
        expect(
            l,
            "class I:\n    @property\n    def v(self):\n        |pass\n",
            Method,
            "I.v",
        );
        expect(l, "class A:\n    x = |1\n", Class, "A");
        expect(l, "def f():\n    g = lambda: |1\n", Function, "f");
        assert_eq!(at_cursor(l, "import os|\n"), None);
    }

    /// Every function in the playground files resolves to itself.
    #[test]
    fn playground_files() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/spikes/helix-log/playground/");
        let cases: &[(Lang, &str, &str, &str)] = &[
            (Lang::Rust, "src/main.rs", "value / rhs", "Parser::term"),
            (Lang::Rust, "src/main.rs", "Token::LParen => write!", "Token::fmt"),
            (Lang::C, "c/ring.c", "r->len--", "ring_pop"),
            (Lang::Cpp, "c/lru.cpp", "order_.emplace_front", "LruCache::put"),
            (Lang::Python, "py/inventory.py", "item.qty -= qty", "Inventory.remove"),
        ];
        for &(lang, file, needle, qualified) in cases {
            let source = std::fs::read_to_string(format!("{root}{file}")).unwrap();
            let byte = source.find(needle).unwrap_or_else(|| panic!("{needle} in {file}"));
            let mut parser = Parser::new();
            parser.set_language(&lang.ts_language()).unwrap();
            let tree = parser.parse(&source, None).unwrap();
            let sym = enclosing_symbol(lang, &tree, &source, byte).unwrap();
            assert_eq!(sym.qualified_name, qualified, "{file}: {needle}");
        }
    }

    #[test]
    fn finds_reviewables_by_qualified_name() {
        let source = "struct P;\nimpl P {\n    fn term(&self) {}\n}\nfn term() {}\nmod m { fn term() {} }\n";
        let mut parser = Parser::new();
        parser.set_language(&Lang::Rust.ts_language()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        for (name, line) in [("P::term", 2), ("term", 4), ("m::term", 5)] {
            let sym = find_reviewable(Lang::Rust, &tree, source, name).unwrap();
            assert_eq!((sym.qualified_name.as_str(), sym.start.row), (name, line));
        }
        assert_eq!(find_reviewable(Lang::Rust, &tree, source, "P").unwrap().kind, SymbolKind::Type);
        assert_eq!(find_reviewable(Lang::Rust, &tree, source, "Q::term"), None);
        assert_eq!(find_reviewable(Lang::Rust, &tree, source, "missing"), None);
        assert_eq!(find_reviewable(Lang::Rust, &tree, source, "m"), None, "modules aren't reviewable");
    }
}
