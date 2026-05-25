use std::path::Path;

use tower_lsp::lsp_types::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    PlainText,
    Rust,
    Scala,
    Nix,
    Html,
    Java,
    Python,
    Javascript,
    Typescript,
    Tsx,
    Markdown,
}

impl Language {
    pub fn from_language_id(language_id: Option<&str>) -> Self {
        match language_id.unwrap_or_default() {
            "rust" | "rs" => Self::Rust,
            "scala" | "scala3" | "sc" => Self::Scala,
            "nix" => Self::Nix,
            "html" => Self::Html,
            "java" => Self::Java,
            "python" => Self::Python,
            "javascript" | "javascriptreact" | "jsx" => Self::Javascript,
            "typescript" => Self::Typescript,
            "typescriptreact" | "tsx" => Self::Tsx,
            "markdown" | "md" | "mdx" => Self::Markdown,
            _ => Self::PlainText,
        }
    }

    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("java") => Self::Java,
            Some("js") | Some("jsx") => Self::Javascript,
            Some("md") | Some("markdown") | Some("mdx") => Self::Markdown,
            Some("nix") => Self::Nix,
            Some("html") | Some("htm") => Self::Html,
            Some("py") => Self::Python,
            Some("rs") => Self::Rust,
            Some("scala") | Some("sc") => Self::Scala,
            Some("ts") => Self::Typescript,
            Some("tsx") => Self::Tsx,
            _ => Self::PlainText,
        }
    }

    pub fn from_lsp_or_uri(language_id: Option<&str>, uri: &Url) -> Self {
        let from_lsp = Self::from_language_id(language_id);
        if from_lsp != Self::PlainText {
            return from_lsp;
        }
        uri.to_file_path()
            .ok()
            .map(|path| Self::from_path(&path))
            .filter(|language| *language != Self::PlainText)
            .unwrap_or(from_lsp)
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::PlainText => "plaintext",
            Self::Rust => "rust",
            Self::Scala => "scala",
            Self::Nix => "nix",
            Self::Html => "html",
            Self::Java => "java",
            Self::Python => "python",
            Self::Javascript => "javascript",
            Self::Typescript => "typescript",
            Self::Tsx => "tsx",
            Self::Markdown => "markdown",
        }
    }
}
