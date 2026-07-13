use std::path::Path;
use tower_lsp_server::ls_types::Uri;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
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

impl SupportedLanguage {
    pub fn from_language_id(language_id: &str) -> Option<Self> {
        match language_id {
            "plaintext" => Some(Self::PlainText),
            "rust" | "rs" => Some(Self::Rust),
            "scala" | "scala3" | "sc" => Some(Self::Scala),
            "nix" => Some(Self::Nix),
            "html" => Some(Self::Html),
            "java" => Some(Self::Java),
            "python" => Some(Self::Python),
            "javascript" | "javascriptreact" | "jsx" => Some(Self::Javascript),
            "typescript" => Some(Self::Typescript),
            "typescriptreact" | "tsx" => Some(Self::Tsx),
            "markdown" | "md" | "mdx" => Some(Self::Markdown),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("java") => Some(Self::Java),
            Some("js") | Some("jsx") => Some(Self::Javascript),
            Some("md") | Some("markdown") | Some("mdx") => Some(Self::Markdown),
            Some("nix") => Some(Self::Nix),
            Some("html") | Some("htm") => Some(Self::Html),
            Some("py") => Some(Self::Python),
            Some("rs") => Some(Self::Rust),
            Some("scala") | Some("sc") => Some(Self::Scala),
            Some("ts") => Some(Self::Typescript),
            Some("tsx") => Some(Self::Tsx),
            Some("txt") | Some("text") => Some(Self::PlainText),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentLanguage {
    Unsupported,
    Supported(SupportedLanguage),
}

impl DocumentLanguage {
    #[cfg(test)]
    pub fn from_language_id(language_id: Option<&str>) -> Self {
        let Some(language_id) = language_id.filter(|value| !value.trim().is_empty()) else {
            return Self::Unsupported;
        };
        SupportedLanguage::from_language_id(language_id)
            .map(Self::Supported)
            .unwrap_or(Self::Unsupported)
    }

    pub fn from_path(path: &Path) -> Self {
        SupportedLanguage::from_path(path)
            .map(Self::Supported)
            .unwrap_or(Self::Unsupported)
    }

    pub fn from_lsp_or_uri(language_id: Option<&str>, uri: &Uri) -> Self {
        if let Some(language_id) = language_id.filter(|value| !value.trim().is_empty())
            && let Some(language) = SupportedLanguage::from_language_id(language_id)
        {
            return Self::Supported(language);
        }

        uri.to_file_path()
            .map(|path| Self::from_path(path.as_ref()))
            .unwrap_or(Self::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_language_ids_are_unsupported() {
        assert_eq!(
            DocumentLanguage::from_language_id(Some("ruby")),
            DocumentLanguage::Unsupported
        );
        assert_eq!(
            DocumentLanguage::from_language_id(None),
            DocumentLanguage::Unsupported
        );
    }

    #[test]
    fn plaintext_is_explicitly_supported() {
        assert_eq!(
            DocumentLanguage::from_language_id(Some("plaintext")),
            DocumentLanguage::Supported(SupportedLanguage::PlainText)
        );
        assert_eq!(
            DocumentLanguage::from_path(Path::new("notes.txt")),
            DocumentLanguage::Supported(SupportedLanguage::PlainText)
        );
    }

    #[test]
    fn unknown_lsp_language_ids_fall_back_to_uri() {
        let uri = "file:///tmp/notes.txt".parse::<Uri>().unwrap();
        assert_eq!(
            DocumentLanguage::from_lsp_or_uri(Some("unknown-client-language"), &uri),
            DocumentLanguage::Supported(SupportedLanguage::PlainText)
        );
    }
}
