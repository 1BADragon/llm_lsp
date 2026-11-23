//! Supported languages and their tree-sitter grammars.

use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    C,
    Cpp,
    Python,
}

impl Lang {
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

    /// Stable name, used as a database key.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Python => "python",
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helix_language_ids() {
        assert_eq!(Lang::from_language_id("cpp"), Some(Lang::Cpp));
        assert_eq!(Lang::from_language_id("toml"), None);
    }
}
