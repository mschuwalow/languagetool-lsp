use crate::language::{DocumentLanguage, SupportedLanguage};
use crate::languagetool::AnnotatedText;
use crate::masking::Masker;
use crate::text_index::TextIndex;
use tower_lsp::lsp_types::{TextDocumentItem, Url};

#[derive(Debug, Clone)]
pub struct Document {
    kind: DocumentKind,
}

#[derive(Debug, Clone)]
enum DocumentKind {
    Supported(Box<SupportedDocument>),
    Unsupported(UnsupportedDocument),
    OutOfSync(OutOfSyncDocument),
}

#[derive(Debug, Clone)]
struct SupportedDocument {
    uri: Url,
    version: i32,
    language: SupportedLanguage,
    text: String,
    index: TextIndex,
    mask: Masker,
}

#[derive(Debug, Clone)]
struct UnsupportedDocument {
    uri: Url,
    version: i32,
}

#[derive(Debug, Clone)]
struct OutOfSyncDocument {
    uri: Url,
    version: i32,
    language: Option<SupportedLanguage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Applied,
    OutOfSync,
}

pub struct CheckableDocument<'a> {
    document: &'a SupportedDocument,
    annotated: AnnotatedText,
    ignored_ranges: Vec<(usize, usize)>,
}

impl CheckableDocument<'_> {
    pub fn uri(&self) -> &Url {
        &self.document.uri
    }

    pub fn version(&self) -> i32 {
        self.document.version
    }

    pub fn annotated(&self) -> &AnnotatedText {
        &self.annotated
    }

    pub fn ignored_ranges(&self) -> &[(usize, usize)] {
        &self.ignored_ranges
    }

    pub fn text(&self) -> &str {
        &self.document.text
    }

    pub fn index(&self) -> &TextIndex {
        &self.document.index
    }
}

impl Document {
    pub fn new(uri: Url, version: i32, language_id: Option<String>, text: String) -> Self {
        let text_len = text.len();
        let kind = match DocumentLanguage::from_lsp_or_uri(language_id.as_deref(), &uri) {
            DocumentLanguage::Supported(language) => {
                log::debug!(
                    "Created supported document {uri} version={version:?} language={language:?} bytes={text_len}"
                );
                DocumentKind::Supported(Box::new(SupportedDocument::new(
                    uri, version, language, text,
                )))
            }
            DocumentLanguage::Unsupported => {
                log::debug!(
                    "Created unsupported document {uri} version={version:?}; text will not be cached"
                );
                DocumentKind::Unsupported(UnsupportedDocument { uri, version })
            }
        };
        Self { kind }
    }

    pub(crate) fn from_text_document(document: &TextDocumentItem) -> Self {
        Self::new(
            document.uri.clone(),
            document.version,
            Some(document.language_id.clone()),
            document.text.clone(),
        )
    }

    pub(crate) fn out_of_sync(uri: Url, version: i32) -> Self {
        Self {
            kind: DocumentKind::OutOfSync(OutOfSyncDocument {
                uri,
                version,
                language: None,
            }),
        }
    }

    pub fn uri(&self) -> &Url {
        match &self.kind {
            DocumentKind::Supported(document) => &document.uri,
            DocumentKind::Unsupported(document) => &document.uri,
            DocumentKind::OutOfSync(document) => &document.uri,
        }
    }

    pub fn version(&self) -> i32 {
        match &self.kind {
            DocumentKind::Supported(document) => document.version,
            DocumentKind::Unsupported(document) => document.version,
            DocumentKind::OutOfSync(document) => document.version,
        }
    }

    pub fn checkable(
        &self,
        language_enabled: impl FnOnce(SupportedLanguage) -> bool,
    ) -> Option<CheckableDocument<'_>> {
        let Some(document) = self.supported() else {
            log::debug!(
                "Skipping {} because the document is not checkable",
                self.uri()
            );
            return None;
        };
        if !language_enabled(document.language) {
            log::debug!(
                "Skipping {} because language {:?} is disabled",
                document.uri,
                document.language
            );
            return None;
        }

        let annotated = document.mask.annotated(&document.text);
        if !annotated.has_text() {
            log::debug!(
                "Skipping {} because language {:?} produced no checkable text",
                document.uri,
                document.language
            );
            return None;
        }

        let ignored_ranges = document
            .mask
            .ignored_ranges(&document.text, &document.index);
        Some(CheckableDocument {
            document,
            ignored_ranges,
            annotated,
        })
    }

    pub(crate) fn full_update(&mut self, version: i32, text: String) {
        match &mut self.kind {
            DocumentKind::Supported(document) => {
                document.version = version;
                document.set_text(text);
            }
            DocumentKind::OutOfSync(document) => {
                let uri = document.uri.clone();
                *self = if let Some(language) = document.language {
                    Self {
                        kind: DocumentKind::Supported(Box::new(SupportedDocument::new(
                            uri, version, language, text,
                        ))),
                    }
                } else {
                    Self::new(uri, version, None, text)
                };
            }
            DocumentKind::Unsupported(document) => {
                document.version = version;
            }
        }
    }

    pub(crate) fn incremental_update(
        &mut self,
        version: i32,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> ChangeStatus {
        match &mut self.kind {
            DocumentKind::Supported(document) => {
                match document.incremental_update(version, range, new_text) {
                    ChangeStatus::Applied => ChangeStatus::Applied,
                    ChangeStatus::OutOfSync => {
                        let uri = document.uri.clone();
                        let language = document.language;
                        self.kind = DocumentKind::OutOfSync(OutOfSyncDocument {
                            uri,
                            version,
                            language: Some(language),
                        });
                        ChangeStatus::OutOfSync
                    }
                }
            }
            DocumentKind::Unsupported(document) => {
                document.version = version;
                ChangeStatus::Applied
            }
            DocumentKind::OutOfSync(document) => {
                document.version = version;
                ChangeStatus::OutOfSync
            }
        }
    }

    fn supported(&self) -> Option<&SupportedDocument> {
        match &self.kind {
            DocumentKind::Supported(document) => Some(document.as_ref()),
            DocumentKind::Unsupported(_) | DocumentKind::OutOfSync(_) => None,
        }
    }
}

impl SupportedDocument {
    fn new(uri: Url, version: i32, language: SupportedLanguage, text: String) -> Self {
        let index = TextIndex::new(&text);
        let mask = Masker::new(&text, language);
        Self {
            uri,
            version,
            language,
            text,
            index,
            mask,
        }
    }

    fn set_text(&mut self, text: String) {
        self.index = TextIndex::new(&text);
        self.mask = Masker::new(&text, self.language);
        self.text = text;
    }

    fn incremental_update(
        &mut self,
        version: i32,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> ChangeStatus {
        let Some((byte_start, byte_end, utf16_start, utf16_end)) = self.index.edit_offsets(range)
        else {
            log::error!(
                "Rejected incremental change for {} because range {:?} is outside valid UTF-16 boundaries",
                self.uri,
                range
            );
            self.version = version;
            return ChangeStatus::OutOfSync;
        };

        let old_text = self.text.clone();
        self.text.replace_range(byte_start..byte_end, new_text);
        self.index.apply_edit(
            &self.text,
            byte_start,
            byte_end,
            utf16_start,
            utf16_end,
            new_text,
        );
        self.mask
            .apply_edit(&old_text, &self.text, byte_start, byte_end, new_text);
        self.version = version;
        ChangeStatus::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use tower_lsp::lsp_types::{Position, Range};

    fn supported_document(document: &Document) -> SupportedDocument {
        document
            .supported()
            .expect("document should be supported")
            .clone()
    }

    fn plaintext_document(text: &str) -> Document {
        Document::new(
            Url::parse("file:///tmp/test.txt").unwrap(),
            1,
            Some("plaintext".to_string()),
            text.to_string(),
        )
    }

    #[test]
    fn applies_all_incremental_changes() {
        let mut document = plaintext_document("hello world");
        document.incremental_update(
            2,
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "zed",
        );
        document.incremental_update(
            3,
            Range::new(Position::new(0, 0), Position::new(0, 5)),
            "hi",
        );

        let document = supported_document(&document);
        assert_eq!(document.text, "hi zed");
        assert_eq!(document.version, 3);
    }

    #[test]
    fn applies_utf16_incremental_change() {
        let mut document = plaintext_document("a😀b");
        document.incremental_update(2, Range::new(Position::new(0, 1), Position::new(0, 3)), "x");
        assert_eq!(supported_document(&document).text, "axb");
    }

    #[test]
    fn malformed_incremental_change_marks_document_out_of_sync() {
        let mut document = plaintext_document("a😀b");
        let status = document.incremental_update(
            2,
            Range::new(Position::new(0, 2), Position::new(0, 2)),
            "x",
        );

        assert_eq!(status, ChangeStatus::OutOfSync);
        assert!(matches!(document.kind, DocumentKind::OutOfSync(_)));
        assert_eq!(document.version(), 2);
        assert!(document.checkable(|_| true).is_none());
    }

    #[test]
    fn full_change_resynchronizes_out_of_sync_document() {
        let mut document = plaintext_document("a😀b");
        document.incremental_update(2, Range::new(Position::new(0, 2), Position::new(0, 2)), "x");
        document.full_update(3, "axb".to_string());

        let document = supported_document(&document);
        assert_eq!(document.text, "axb");
        assert_eq!(document.version, 3);
    }

    #[test]
    fn full_change_resynchronizes_out_of_sync_document_with_original_language() {
        let mut document = Document::new(
            Url::parse("file:///tmp/test.rs").unwrap(),
            1,
            Some("rust".to_string()),
            "let code = 1; // This are old docs.".to_string(),
        );
        document.incremental_update(
            2,
            Range::new(Position::new(0, 100), Position::new(0, 100)),
            "x",
        );
        document.full_update(3, "let code = 1; // This are new docs.".to_string());

        let document = supported_document(&document);
        let data = document.mask.annotated(&document.text);
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();

        assert_eq!(checked, "This are new docs.");
    }

    fn assert_index_consistent(document: &Document) {
        let doc = supported_document(document);
        let expected = TextIndex::new(&doc.text);
        assert_eq!(
            doc.index, expected,
            "index is inconsistent with text {:?}",
            doc.text
        );
    }

    #[test]
    fn incremental_changes_keep_index_consistent() {
        let mut document = plaintext_document("hello world\nfoo bar");
        assert_index_consistent(&document);

        document.incremental_update(
            2,
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "zed",
        );
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text, "hello zed\nfoo bar");

        document.incremental_update(
            3,
            Range::new(Position::new(1, 3), Position::new(1, 3)),
            "\n",
        );
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text, "hello zed\nfoo\n bar");

        document.incremental_update(4, Range::new(Position::new(1, 3), Position::new(2, 0)), "");
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text, "hello zed\nfoo bar");
    }

    #[test]
    fn incremental_changes_with_emoji_keep_index_consistent() {
        let mut document = plaintext_document("hi 😀 there");
        assert_index_consistent(&document);

        document.incremental_update(2, Range::new(Position::new(0, 3), Position::new(0, 5)), "x");
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text, "hi x there");

        document.incremental_update(
            3,
            Range::new(Position::new(0, 4), Position::new(0, 4)),
            "😂",
        );
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text, "hi x😂 there");
    }

    #[test]
    fn incremental_changes_update_mask_tree() {
        let mut document = Document::new(
            Url::parse("file:///tmp/test.rs").unwrap(),
            1,
            Some("rust".to_string()),
            indoc! {r#"
                let code = "// This are code";
                // This are old docs.
            "#}
            .to_string(),
        );

        document.incremental_update(
            2,
            Range::new(Position::new(1, 12), Position::new(1, 15)),
            "new",
        );

        let document = supported_document(&document);
        let data = document.mask.annotated(&document.text);
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();

        assert_eq!(checked, "This are new docs.");
        assert!(!checked.contains("This are code"));
    }

    #[test]
    fn incremental_changes_with_crlf_keep_index_consistent() {
        let mut document = plaintext_document("line one\r\nline two");
        assert_index_consistent(&document);

        document.incremental_update(
            2,
            Range::new(Position::new(1, 5), Position::new(1, 8)),
            "three",
        );
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text, "line one\r\nline three");
    }

    #[test]
    fn unsupported_document_is_noop_without_cached_text() {
        let mut document = Document::new(
            Url::parse("file:///tmp/test.rb").unwrap(),
            1,
            Some("ruby".to_string()),
            "This are a tset.".to_string(),
        );

        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
        assert!(document.checkable(|_| true).is_none());
        assert_eq!(document.version(), 1);

        document.incremental_update(
            2,
            Range::new(Position::new(0, 0), Position::new(0, 4)),
            "That",
        );

        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
        assert!(document.checkable(|_| true).is_none());
        assert_eq!(document.version(), 2);
    }
}
