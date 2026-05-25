use crate::language::{DocumentLanguage, SupportedLanguage};
use crate::languagetool::AnnotatedText;
use crate::masking::{MaskError, Masker};
use crate::text_index::TextIndex;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Url};

#[derive(Debug, Clone)]
pub struct Document {
    kind: DocumentKind,
}

#[derive(Debug, Clone)]
enum DocumentKind {
    Supported(Box<SupportedDocument>),
    Unsupported(UnsupportedDocument),
}

#[derive(Debug, Clone)]
struct SupportedDocument {
    uri: Url,
    version: Option<i32>,
    language: SupportedLanguage,
    text: String,
    index: TextIndex,
    mask: Masker,
}

#[derive(Debug, Clone)]
struct UnsupportedDocument {
    uri: Url,
    version: Option<i32>,
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

    pub fn version(&self) -> Option<i32> {
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
    pub fn new(uri: Url, version: Option<i32>, language_id: Option<String>, text: String) -> Self {
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

    pub fn uri(&self) -> &Url {
        match &self.kind {
            DocumentKind::Supported(document) => &document.uri,
            DocumentKind::Unsupported(document) => &document.uri,
        }
    }

    pub fn version(&self) -> Option<i32> {
        match &self.kind {
            DocumentKind::Supported(document) => document.version,
            DocumentKind::Unsupported(document) => document.version,
        }
    }

    pub fn checkable(
        &self,
        language_enabled: impl FnOnce(SupportedLanguage) -> bool,
    ) -> Option<CheckableDocument<'_>> {
        let Some(document) = self.supported() else {
            log::debug!(
                "Skipping {} because the document language is unsupported",
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
        log::debug!(
            "Prepared checkable document {} language={:?} annotations={} ignored_ranges={}",
            document.uri,
            document.language,
            annotated.annotation.len(),
            ignored_ranges.len()
        );

        Some(CheckableDocument {
            document,
            ignored_ranges,
            annotated,
        })
    }

    fn supported(&self) -> Option<&SupportedDocument> {
        match &self.kind {
            DocumentKind::Supported(document) => Some(document.as_ref()),
            DocumentKind::Unsupported(_) => None,
        }
    }

    fn set_version(&mut self, version: Option<i32>) {
        let uri = self.uri().clone();
        match &mut self.kind {
            DocumentKind::Supported(document) => {
                let previous = document.version;
                document.version = version.or(document.version);
                log::debug!(
                    "Updated supported document {uri} version {previous:?} -> {:?}",
                    document.version
                );
            }
            DocumentKind::Unsupported(document) => {
                let previous = document.version;
                document.version = version.or(document.version);
                log::debug!(
                    "Updated unsupported document {uri} version {previous:?} -> {:?}",
                    document.version
                );
            }
        }
    }

    fn set_text(&mut self, text: String) {
        if let DocumentKind::Supported(document) = &mut self.kind {
            log::debug!(
                "Replacing full text for supported document {} bytes={} -> {}",
                document.uri,
                document.text.len(),
                text.len()
            );
            document.set_text(text);
        } else {
            log::debug!(
                "Ignoring full text update for unsupported document {}",
                self.uri()
            );
        }
    }

    fn apply_range_change(
        &mut self,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> Result<bool, MaskError> {
        match &mut self.kind {
            DocumentKind::Supported(document) => document.apply_range_change(range, new_text),
            DocumentKind::Unsupported(document) => {
                log::debug!(
                    "Ignoring incremental text change for unsupported document {} version={:?}",
                    document.uri,
                    document.version
                );
                Ok(true)
            }
        }
    }

    fn from_text_document(document: &TextDocumentItem) -> Self {
        Self::new(
            document.uri.clone(),
            Some(document.version),
            Some(document.language_id.clone()),
            document.text.clone(),
        )
    }
}

impl SupportedDocument {
    fn new(uri: Url, version: Option<i32>, language: SupportedLanguage, text: String) -> Self {
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

    fn apply_range_change(
        &mut self,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> Result<bool, MaskError> {
        let Some((byte_start, byte_end, utf16_start, utf16_end)) = self.index.edit_offsets(range)
        else {
            log::debug!(
                "Rejected incremental change for {} because range {:?} is outside valid UTF-16 boundaries",
                self.uri,
                range
            );
            return Ok(false);
        };
        log::debug!(
            "Applying incremental change to {} bytes={byte_start}..{byte_end} utf16={utf16_start}..{utf16_end} replacement_bytes={}",
            self.uri,
            new_text.len()
        );
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
            .apply_edit(&old_text, &self.text, byte_start, byte_end, new_text)?;
        Ok(true)
    }
}

#[derive(Debug, Default, Clone)]
pub struct DocumentCache {
    documents: Arc<RwLock<HashMap<String, DocumentEntry>>>,
}

#[derive(Debug, Clone)]
struct DocumentEntry {
    document: Document,
    generation: u64,
}

impl DocumentCache {
    pub fn insert(&self, document: &TextDocumentItem) {
        let document = Document::from_text_document(document);
        log::debug!(
            "Inserting document {} version={:?}",
            document.uri(),
            document.version()
        );
        self.documents
            .write()
            .expect("document cache poisoned")
            .entry(document.uri().to_string())
            .and_modify(|entry| entry.document = document.clone())
            .or_insert(DocumentEntry {
                document,
                generation: 0,
            });
    }

    pub fn update(&self, uri: &Url, version: Option<i32>, text: String) {
        log::debug!(
            "Applying full document update for {uri} version={version:?} bytes={}",
            text.len()
        );
        let key = uri.to_string();
        let mut documents = self.documents.write().expect("document cache poisoned");
        if let Some(entry) = documents.get_mut(&key) {
            entry.document.set_version(version);
            entry.document.set_text(text);
        } else {
            documents.insert(
                key,
                DocumentEntry {
                    document: Document::new(uri.clone(), version, None, text),
                    generation: 0,
                },
            );
        }
    }

    pub fn apply_change(
        &self,
        uri: &Url,
        version: Option<i32>,
        change: TextDocumentContentChangeEvent,
    ) -> Result<(), MaskError> {
        if let Some(range) = change.range {
            log::debug!(
                "Applying ranged document change for {uri} version={version:?} range={range:?} replacement_bytes={}",
                change.text.len()
            );
            let key = uri.to_string();
            let mut documents = self.documents.write().expect("document cache poisoned");
            if let Some(entry) = documents.get_mut(&key) {
                if entry.document.apply_range_change(range, &change.text)? {
                    entry.document.set_version(version);
                } else {
                    log::debug!("Ranged document change for {uri} was ignored");
                }
            } else {
                log::debug!("Document {uri} was not cached; treating ranged change as full text");
                documents.insert(
                    key,
                    DocumentEntry {
                        document: Document::new(uri.clone(), version, None, change.text),
                        generation: 0,
                    },
                );
            }
        } else {
            log::debug!(
                "Applying full document change for {uri} version={version:?} bytes={}",
                change.text.len()
            );
            self.update(uri, version, change.text);
        }
        Ok(())
    }

    pub fn remove(&self, uri: &Url) {
        log::debug!("Removing document {uri} from cache");
        self.documents
            .write()
            .expect("document cache poisoned")
            .remove(uri.as_str());
    }

    pub fn get(&self, uri: &Url) -> Option<Document> {
        self.documents
            .read()
            .expect("document cache poisoned")
            .get(uri.as_str())
            .map(|entry| entry.document.clone())
    }

    pub fn bump_generation(&self, uri: &Url) -> u64 {
        let mut documents = self.documents.write().expect("document cache poisoned");
        let Some(entry) = documents.get_mut(uri.as_str()) else {
            return 0;
        };
        entry.generation += 1;
        entry.generation
    }

    pub fn generation(&self, uri: &Url) -> u64 {
        self.documents
            .read()
            .expect("document cache poisoned")
            .get(uri.as_str())
            .map_or(0, |entry| entry.generation)
    }

    pub fn urls(&self) -> Vec<Url> {
        self.documents
            .read()
            .expect("document cache poisoned")
            .values()
            .map(|entry| entry.document.uri().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use tower_lsp::lsp_types::{Position, Range};

    fn supported_document(cache: &DocumentCache, uri: &Url) -> SupportedDocument {
        cache
            .get(uri)
            .unwrap()
            .supported()
            .expect("document should be supported")
            .clone()
    }

    #[test]
    fn applies_all_incremental_changes() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        cache.update(&uri, Some(1), "hello world".to_string());
        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 6), Position::new(0, 11))),
                    range_length: None,
                    text: "zed".to_string(),
                },
            )
            .unwrap();
        cache
            .apply_change(
                &uri,
                Some(3),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 0), Position::new(0, 5))),
                    range_length: None,
                    text: "hi".to_string(),
                },
            )
            .unwrap();

        let document = supported_document(&cache, &uri);
        assert_eq!(document.text, "hi zed");
        assert_eq!(document.version, Some(3));
    }

    #[test]
    fn applies_utf16_incremental_change() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        cache.update(&uri, Some(1), "a😀b".to_string());
        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 1), Position::new(0, 3))),
                    range_length: None,
                    text: "x".to_string(),
                },
            )
            .unwrap();
        assert_eq!(supported_document(&cache, &uri).text, "axb");
    }

    #[test]
    fn ignores_incremental_change_inside_surrogate_pair() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        cache.update(&uri, Some(1), "a😀b".to_string());
        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 2), Position::new(0, 2))),
                    range_length: None,
                    text: "x".to_string(),
                },
            )
            .unwrap();
        let document = supported_document(&cache, &uri);
        assert_eq!(document.text, "a😀b");
        assert_eq!(document.version, Some(1));
    }

    // Verify that document.index always matches TextIndex::new(&document.text) after
    // a sequence of incremental changes.
    fn assert_index_consistent(cache: &DocumentCache, uri: &Url) {
        let doc = supported_document(cache, uri);
        let expected = TextIndex::new(&doc.text);
        assert_eq!(
            doc.index, expected,
            "index is inconsistent with text {:?}",
            doc.text
        );
    }

    #[test]
    fn incremental_changes_keep_index_consistent() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        cache.update(&uri, Some(1), "hello world\nfoo bar".to_string());
        assert_index_consistent(&cache, &uri);

        // Replace word on first line.
        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 6), Position::new(0, 11))),
                    range_length: None,
                    text: "zed".to_string(),
                },
            )
            .unwrap();
        assert_index_consistent(&cache, &uri);
        assert_eq!(supported_document(&cache, &uri).text, "hello zed\nfoo bar");

        // Insert a newline in the middle of the second line.
        cache
            .apply_change(
                &uri,
                Some(3),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 3), Position::new(1, 3))),
                    range_length: None,
                    text: "\n".to_string(),
                },
            )
            .unwrap();
        assert_index_consistent(&cache, &uri);
        assert_eq!(
            supported_document(&cache, &uri).text,
            "hello zed\nfoo\n bar"
        );

        // Delete the second newline, merging lines 1 and 2.
        cache
            .apply_change(
                &uri,
                Some(4),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 3), Position::new(2, 0))),
                    range_length: None,
                    text: String::new(),
                },
            )
            .unwrap();
        assert_index_consistent(&cache, &uri);
        assert_eq!(supported_document(&cache, &uri).text, "hello zed\nfoo bar");
    }

    #[test]
    fn incremental_changes_with_emoji_keep_index_consistent() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/emoji.txt").unwrap();
        cache.update(&uri, Some(1), "hi 😀 there".to_string());
        assert_index_consistent(&cache, &uri);

        // Replace the emoji with ASCII — removes the checkpoint.
        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 3), Position::new(0, 5))),
                    range_length: None,
                    text: "x".to_string(),
                },
            )
            .unwrap();
        assert_index_consistent(&cache, &uri);
        assert_eq!(supported_document(&cache, &uri).text, "hi x there");

        // Insert an emoji after existing ASCII — adds a new checkpoint at the right offset.
        cache
            .apply_change(
                &uri,
                Some(3),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 4), Position::new(0, 4))),
                    range_length: None,
                    text: "😂".to_string(),
                },
            )
            .unwrap();
        assert_index_consistent(&cache, &uri);
        assert_eq!(supported_document(&cache, &uri).text, "hi x😂 there");
    }

    #[test]
    fn incremental_changes_update_mask_tree() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.rs").unwrap();
        cache.insert(&TextDocumentItem {
            uri: uri.clone(),
            language_id: "rust".to_string(),
            version: 1,
            text: indoc! {r#"
                let code = "// This are code";
                // This are old docs.
            "#}
            .to_string(),
        });

        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 12), Position::new(1, 15))),
                    range_length: None,
                    text: "new".to_string(),
                },
            )
            .unwrap();

        let document = supported_document(&cache, &uri);
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
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/crlf.txt").unwrap();
        cache.update(&uri, Some(1), "line one\r\nline two".to_string());
        assert_index_consistent(&cache, &uri);

        // Edit on the second line using positions from the CRLF-aware index.
        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 5), Position::new(1, 8))),
                    range_length: None,
                    text: "three".to_string(),
                },
            )
            .unwrap();
        assert_index_consistent(&cache, &uri);
        assert_eq!(
            supported_document(&cache, &uri).text,
            "line one\r\nline three"
        );
    }

    #[test]
    fn unsupported_document_is_noop_without_cached_text() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.rb").unwrap();
        cache.insert(&TextDocumentItem {
            uri: uri.clone(),
            language_id: "ruby".to_string(),
            version: 1,
            text: "This are a tset.".to_string(),
        });

        let document = cache.get(&uri).unwrap();
        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
        assert!(document.checkable(|_| true).is_none());
        assert_eq!(document.version(), Some(1));

        cache
            .apply_change(
                &uri,
                Some(2),
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 0), Position::new(0, 4))),
                    range_length: None,
                    text: "That".to_string(),
                },
            )
            .unwrap();

        let document = cache.get(&uri).unwrap();
        assert!(matches!(&document.kind, DocumentKind::Unsupported(_)));
        assert!(document.checkable(|_| true).is_none());
        assert_eq!(document.version(), Some(2));
    }

    #[test]
    fn tracks_generation_with_document_entry() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        assert_eq!(cache.generation(&uri), 0);
        assert_eq!(cache.bump_generation(&uri), 0);

        cache.update(&uri, Some(1), "hello".to_string());
        assert_eq!(cache.generation(&uri), 0);
        assert_eq!(cache.bump_generation(&uri), 1);
        assert_eq!(cache.generation(&uri), 1);
        assert_eq!(cache.bump_generation(&uri), 2);

        cache.remove(&uri);
        assert_eq!(cache.generation(&uri), 0);
    }
}
