use crate::diagnostics_cache::{CachedDiagnostic, DiagnosticsCache};
use crate::language::{DocumentLanguage, SupportedLanguage};
use crate::masking::{CheckBlock, Masker};
use crate::text_index::{ByteRange, TextIndex};
use std::sync::Arc;
use tower_lsp::lsp_types::{Diagnostic, TextDocumentItem, Url};

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
    text: Arc<String>,
    index: Arc<TextIndex>,
    mask: Masker,
    diagnostics_cache: DiagnosticsCache,
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
    language: SupportedLanguage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocumentChangeStatus {
    Incremental,
    FullReplace,
    OutOfSync,
}

#[derive(Debug)]
pub enum PreparedCheck {
    Check(PreparedCheckData),
    ReuseCached { uri: Url, version: i32 },
    Clear { uri: Url, version: i32 },
}

#[derive(Debug)]
pub struct PreparedCheckData {
    pub uri: Url,
    pub version: i32,
    pub text: Arc<String>,
    pub index: Arc<TextIndex>,
    pub blocks: Vec<PreparedCheckBlock>,
}

#[derive(Debug)]
pub struct PreparedCheckBlock {
    pub block: CheckBlock,
}

#[derive(Debug)]
pub struct CompletedCheckBlock {
    pub byte_range: ByteRange,
    pub diagnostics: Vec<CachedDiagnostic>,
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

    pub(crate) fn full_update(&mut self, version: i32, text: String) {
        match &mut self.kind {
            DocumentKind::Supported(document) => {
                document.version = version;
                document.set_text(text);
            }
            DocumentKind::OutOfSync(document) => {
                let uri = document.uri.clone();
                *self = Self {
                    kind: DocumentKind::Supported(Box::new(SupportedDocument::new(
                        uri,
                        version,
                        document.language,
                        text,
                    ))),
                };
            }
            DocumentKind::Unsupported(document) => {
                let uri = document.uri.clone();
                *self = Self::new(uri, version, None, text);
            }
        }
    }

    pub(crate) fn incremental_update(
        &mut self,
        version: i32,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> Option<DocumentChangeStatus> {
        match &mut self.kind {
            DocumentKind::Supported(document) => {
                let status = document.incremental_update(version, range, new_text);
                if status == DocumentChangeStatus::OutOfSync {
                    let uri = document.uri.clone();
                    let language = document.language;
                    self.kind = DocumentKind::OutOfSync(OutOfSyncDocument {
                        uri,
                        version,
                        language,
                    });
                }
                Some(status)
            }
            DocumentKind::Unsupported(document) => {
                document.version = version;
                None
            }
            DocumentKind::OutOfSync(document) => {
                document.version = version;
                Some(DocumentChangeStatus::OutOfSync)
            }
        }
    }

    pub(crate) fn prepare_check(&mut self, options_key: String) -> PreparedCheck {
        let Some(document) = self.supported_mut() else {
            return PreparedCheck::Clear {
                uri: self.uri().clone(),
                version: self.version(),
            };
        };
        document.prepare_check(options_key)
    }

    pub(crate) fn complete_check(
        &mut self,
        checked_blocks: Vec<CompletedCheckBlock>,
    ) -> Vec<Diagnostic> {
        let Some(document) = self.supported_mut() else {
            return Vec::new();
        };
        document.complete_check(checked_blocks)
    }

    #[cfg(test)]
    fn supported(&self) -> Option<&SupportedDocument> {
        match &self.kind {
            DocumentKind::Supported(document) => Some(document.as_ref()),
            DocumentKind::Unsupported(_) | DocumentKind::OutOfSync(_) => None,
        }
    }

    fn supported_mut(&mut self) -> Option<&mut SupportedDocument> {
        match &mut self.kind {
            DocumentKind::Supported(document) => Some(document.as_mut()),
            DocumentKind::Unsupported(_) | DocumentKind::OutOfSync(_) => None,
        }
    }
}

impl SupportedDocument {
    fn new(uri: Url, version: i32, language: SupportedLanguage, text: String) -> Self {
        let text = Arc::new(text);
        let index = Arc::new(TextIndex::new(&text));
        let mask = Masker::new(&text, language);
        Self {
            uri,
            version,
            language,
            text,
            index,
            mask,
            diagnostics_cache: DiagnosticsCache::default(),
        }
    }

    fn set_text(&mut self, text: String) {
        self.index = Arc::new(TextIndex::new(&text));
        self.mask = Masker::new(&text, self.language);
        self.diagnostics_cache.clear();
        self.text = Arc::new(text);
    }

    fn incremental_update(
        &mut self,
        version: i32,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> DocumentChangeStatus {
        let Some((bytes, utf16)) = self.index.edit_offsets(range) else {
            log::error!(
                "Rejected incremental change for {} because range {:?} is outside valid UTF-16 boundaries",
                self.uri,
                range
            );
            self.version = version;
            return DocumentChangeStatus::OutOfSync;
        };

        let mask_edit = Masker::input_edit(&self.text, &bytes, new_text);

        let text = Arc::make_mut(&mut self.text);
        text.replace_range(bytes.start.0..bytes.end.0, new_text);

        Arc::make_mut(&mut self.index).apply_edit(text, &bytes, &utf16, new_text);
        self.mask.apply_edit(&mask_edit, text);
        self.diagnostics_cache
            .apply_edit(&bytes, new_text.len(), &self.index, version);
        self.version = version;
        DocumentChangeStatus::Incremental
    }

    fn prepare_check(&mut self, options_key: String) -> PreparedCheck {
        let check_blocks = self.mask.check_blocks(&self.text);
        if check_blocks.is_empty() {
            log::debug!(
                "Skipping {} because language {:?} produced no checkable blocks",
                self.uri,
                self.language
            );
            self.diagnostics_cache.clear();
            return PreparedCheck::Clear {
                uri: self.uri.clone(),
                version: self.version,
            };
        }

        // Any cached block should also be returned by the masker, as we should have invalidated
        // the cache blocks otherwise.
        debug_assert!(
            self.diagnostics_cache.byte_ranges().all(|cached_range| {
                check_blocks
                    .iter()
                    .any(|block| block.byte_range == *cached_range)
            }),
            "Not all cached blocks were returned by the masker"
        );

        self.diagnostics_cache.reset_if_options_changed(options_key);

        let blocks: Vec<PreparedCheckBlock> = check_blocks
            .into_iter()
            .filter_map(|block| {
                (!self.diagnostics_cache.contains_block(&block.byte_range))
                    .then_some(PreparedCheckBlock { block })
            })
            .collect();

        if blocks.is_empty() {
            return PreparedCheck::ReuseCached {
                uri: self.uri.clone(),
                version: self.version,
            };
        }

        PreparedCheck::Check(PreparedCheckData {
            uri: self.uri.clone(),
            version: self.version,
            text: Arc::clone(&self.text),
            index: Arc::clone(&self.index),
            blocks,
        })
    }

    fn complete_check(&mut self, checked_blocks: Vec<CompletedCheckBlock>) -> Vec<Diagnostic> {
        for checked in checked_blocks {
            self.diagnostics_cache
                .store_checked_block(checked.byte_range, checked.diagnostics);
        }

        self.diagnostics_cache.diagnostics()
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
        assert_eq!(document.text.as_str(), "hi zed");
        assert_eq!(document.version, 3);
    }

    #[test]
    fn applies_utf16_incremental_change() {
        let mut document = plaintext_document("a😀b");
        document.incremental_update(2, Range::new(Position::new(0, 1), Position::new(0, 3)), "x");
        assert_eq!(supported_document(&document).text.as_str(), "axb");
    }

    #[test]
    fn malformed_incremental_change_marks_document_out_of_sync() {
        let mut document = plaintext_document("a😀b");
        let status = document.incremental_update(
            2,
            Range::new(Position::new(0, 2), Position::new(0, 2)),
            "x",
        );

        assert_eq!(status, Some(DocumentChangeStatus::OutOfSync));
        assert!(matches!(document.kind, DocumentKind::OutOfSync(_)));
        assert_eq!(document.version(), 2);
        assert!(matches!(
            document.prepare_check("test".to_string()),
            PreparedCheck::Clear { .. }
        ));
    }

    #[test]
    fn reversed_incremental_change_marks_document_out_of_sync() {
        let mut document = plaintext_document("abc");
        let status = document.incremental_update(
            2,
            Range::new(Position::new(0, 2), Position::new(0, 1)),
            "x",
        );

        assert_eq!(status, Some(DocumentChangeStatus::OutOfSync));
        assert!(matches!(document.kind, DocumentKind::OutOfSync(_)));
        assert_eq!(document.version(), 2);
    }

    #[test]
    fn full_change_resynchronizes_out_of_sync_document() {
        let mut document = plaintext_document("a😀b");
        document.incremental_update(2, Range::new(Position::new(0, 2), Position::new(0, 2)), "x");
        document.full_update(3, "axb".to_string());

        let document = supported_document(&document);
        assert_eq!(document.text.as_str(), "axb");
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
        let blocks = document.mask.check_blocks(&document.text);
        let checked: String = blocks
            .iter()
            .flat_map(|b| b.annotated.annotation.iter())
            .filter_map(|annotation| annotation.as_text())
            .collect();

        assert_eq!(checked, "This are new docs.");
    }

    fn assert_index_consistent(document: &Document) {
        let doc = supported_document(document);
        let expected = TextIndex::new(&doc.text);
        assert_eq!(
            doc.index.as_ref(),
            &expected,
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
        assert_eq!(
            supported_document(&document).text.as_str(),
            "hello zed\nfoo bar"
        );

        document.incremental_update(
            3,
            Range::new(Position::new(1, 3), Position::new(1, 3)),
            "\n",
        );
        assert_index_consistent(&document);
        assert_eq!(
            supported_document(&document).text.as_str(),
            "hello zed\nfoo\n bar"
        );

        document.incremental_update(4, Range::new(Position::new(1, 3), Position::new(2, 0)), "");
        assert_index_consistent(&document);
        assert_eq!(
            supported_document(&document).text.as_str(),
            "hello zed\nfoo bar"
        );
    }

    #[test]
    fn incremental_changes_with_emoji_keep_index_consistent() {
        let mut document = plaintext_document("hi 😀 there");
        assert_index_consistent(&document);

        document.incremental_update(2, Range::new(Position::new(0, 3), Position::new(0, 5)), "x");
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text.as_str(), "hi x there");

        document.incremental_update(
            3,
            Range::new(Position::new(0, 4), Position::new(0, 4)),
            "😂",
        );
        assert_index_consistent(&document);
        assert_eq!(supported_document(&document).text.as_str(), "hi x😂 there");
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
        let blocks = document.mask.check_blocks(&document.text);
        let checked: String = blocks
            .iter()
            .flat_map(|b| b.annotated.annotation.iter())
            .filter_map(|annotation| annotation.as_text())
            .collect();

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
        assert_eq!(
            supported_document(&document).text.as_str(),
            "line one\r\nline three"
        );
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
        assert!(matches!(
            document.prepare_check("test".to_string()),
            PreparedCheck::Clear { .. }
        ));
        assert_eq!(document.version(), 1);

        let status = document.incremental_update(
            2,
            Range::new(Position::new(0, 0), Position::new(0, 4)),
            "That",
        );

        assert_eq!(status, None);
        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
        assert!(matches!(
            document.prepare_check("test".to_string()),
            PreparedCheck::Clear { .. }
        ));
        assert_eq!(document.version(), 2);
    }

    #[test]
    fn unsupported_full_update_remains_unsupported_without_supported_uri() {
        let mut document = Document::new(
            Url::parse("untitled:notes").unwrap(),
            1,
            Some("unknown".to_string()),
            "This are ignored.".to_string(),
        );

        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
        document.full_update(2, "This are checked.".to_string());
        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
    }

    #[test]
    fn unknown_lsp_language_id_falls_back_to_supported_uri() {
        let document = Document::new(
            Url::parse("file:///tmp/notes.txt").unwrap(),
            1,
            Some("unknown".to_string()),
            "This are checked.".to_string(),
        );

        let document = supported_document(&document);
        assert_eq!(document.text.as_str(), "This are checked.");
        assert_eq!(document.version, 1);
    }
}
