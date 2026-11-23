//! Supported languages and their tree-sitter grammars.

use std::sync::OnceLock;

use tree_sitter::{Language, Query};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    C,
    Cpp,
    Python,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::Rust, Lang::C, Lang::Cpp, Lang::Python];

    /// Maps an LSP `languageId` (as Helix sends it) to a supported language.
    pub fn from_language_id(id: &str) -> Option<Self> {
        match id {
            "rust" => Some(Lang::Rust),
            "c" => Some(Lang::C),
            "cpp" => Some(Lang::Cpp),
            "python" => Some(Lang::Python),
            _ => None,
        }
    }

    pub fn ts_language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    /// The grammar's bundled tags query (`@definition.*` / `@name` captures),
    /// compiled once and shared.
    pub fn tags_query(self) -> &'static Query {
        static QUERIES: [OnceLock<Query>; 4] = [const { OnceLock::new() }; 4];
        let (slot, src) = match self {
            Lang::Rust => (&QUERIES[0], tree_sitter_rust::TAGS_QUERY),
            Lang::C => (&QUERIES[1], tree_sitter_c::TAGS_QUERY),
            Lang::Cpp => (&QUERIES[2], tree_sitter_cpp::TAGS_QUERY),
            Lang::Python => (&QUERIES[3], tree_sitter_python::TAGS_QUERY),
        };
        slot.get_or_init(|| {
            Query::new(&self.ts_language(), src)
                .unwrap_or_else(|e| panic!("bundled tags query for {self:?} is invalid: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_tags_queries_compile() {
        for lang in Lang::ALL {
            let q = lang.tags_query();
            assert!(
                q.capture_names().iter().any(|n| n.starts_with("definition.")),
                "{lang:?} tags query has no definition captures"
            );
        }
    }

    #[test]
    fn helix_language_ids() {
        assert_eq!(Lang::from_language_id("cpp"), Some(Lang::Cpp));
        assert_eq!(Lang::from_language_id("toml"), None);
    }
}
