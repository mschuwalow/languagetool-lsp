use crate::text_index::TextIndex;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Url};

#[derive(Debug, Clone)]
pub struct Document {
    pub uri: Url,
    pub version: Option<i32>,
    pub language_id: Option<String>,
    pub text: String,
    pub index: TextIndex,
}

impl Document {
    pub fn new(uri: Url, version: Option<i32>, language_id: Option<String>, text: String) -> Self {
        let index = TextIndex::new(&text);
        Self {
            uri,
            version,
            language_id,
            text,
            index,
        }
    }

    fn set_text(&mut self, text: String) {
        self.index = TextIndex::new(&text);
        self.text = text;
    }

    fn apply_range_change(&mut self, range: tower_lsp::lsp_types::Range, new_text: &str) -> bool {
        let Some((byte_start, byte_end, utf16_start, utf16_end)) = self.index.edit_offsets(range)
        else {
            return false;
        };
        self.text.replace_range(byte_start..byte_end, new_text);
        self.index
            .apply_edit(byte_start, byte_end, utf16_start, utf16_end, new_text);
        true
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
        self.documents
            .write()
            .expect("document cache poisoned")
            .entry(document.uri.to_string())
            .and_modify(|entry| entry.document = document.clone())
            .or_insert(DocumentEntry {
                document,
                generation: 0,
            });
    }

    pub fn update(&self, uri: &Url, version: Option<i32>, text: String) {
        let key = uri.to_string();
        let mut documents = self.documents.write().expect("document cache poisoned");
        if let Some(entry) = documents.get_mut(&key) {
            entry.document.version = version.or(entry.document.version);
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
    ) {
        if let Some(range) = change.range {
            let key = uri.to_string();
            let mut documents = self.documents.write().expect("document cache poisoned");
            if let Some(entry) = documents.get_mut(&key) {
                if entry.document.apply_range_change(range, &change.text) {
                    entry.document.version = version.or(entry.document.version);
                }
            } else {
                documents.insert(
                    key,
                    DocumentEntry {
                        document: Document::new(uri.clone(), version, None, change.text),
                        generation: 0,
                    },
                );
            }
        } else {
            self.update(uri, version, change.text);
        }
    }

    pub fn remove(&self, uri: &Url) {
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
            .map(|entry| entry.document.uri.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, Range};

    #[test]
    fn applies_all_incremental_changes() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        cache.update(&uri, Some(1), "hello world".to_string());
        cache.apply_change(
            &uri,
            Some(2),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 6), Position::new(0, 11))),
                range_length: None,
                text: "zed".to_string(),
            },
        );
        cache.apply_change(
            &uri,
            Some(3),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 5))),
                range_length: None,
                text: "hi".to_string(),
            },
        );

        let document = cache.get(&uri).unwrap();
        assert_eq!(document.text, "hi zed");
        assert_eq!(document.version, Some(3));
    }

    #[test]
    fn applies_utf16_incremental_change() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        cache.update(&uri, Some(1), "a😀b".to_string());
        cache.apply_change(
            &uri,
            Some(2),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 1), Position::new(0, 3))),
                range_length: None,
                text: "x".to_string(),
            },
        );
        assert_eq!(cache.get(&uri).unwrap().text, "axb");
    }

    // Verify that document.index always matches TextIndex::new(&document.text) after
    // a sequence of incremental changes.
    fn assert_index_consistent(cache: &DocumentCache, uri: &Url) {
        let doc = cache.get(uri).unwrap();
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
        cache.apply_change(
            &uri,
            Some(2),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 6), Position::new(0, 11))),
                range_length: None,
                text: "zed".to_string(),
            },
        );
        assert_index_consistent(&cache, &uri);
        assert_eq!(cache.get(&uri).unwrap().text, "hello zed\nfoo bar");

        // Insert a newline in the middle of the second line.
        cache.apply_change(
            &uri,
            Some(3),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 3), Position::new(1, 3))),
                range_length: None,
                text: "\n".to_string(),
            },
        );
        assert_index_consistent(&cache, &uri);
        assert_eq!(cache.get(&uri).unwrap().text, "hello zed\nfoo\n bar");

        // Delete the second newline, merging lines 1 and 2.
        cache.apply_change(
            &uri,
            Some(4),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 3), Position::new(2, 0))),
                range_length: None,
                text: String::new(),
            },
        );
        assert_index_consistent(&cache, &uri);
        assert_eq!(cache.get(&uri).unwrap().text, "hello zed\nfoo bar");
    }

    #[test]
    fn incremental_changes_with_emoji_keep_index_consistent() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/emoji.txt").unwrap();
        cache.update(&uri, Some(1), "hi 😀 there".to_string());
        assert_index_consistent(&cache, &uri);

        // Replace the emoji with ASCII — removes the checkpoint.
        cache.apply_change(
            &uri,
            Some(2),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 3), Position::new(0, 5))),
                range_length: None,
                text: "x".to_string(),
            },
        );
        assert_index_consistent(&cache, &uri);
        assert_eq!(cache.get(&uri).unwrap().text, "hi x there");

        // Insert an emoji after existing ASCII — adds a new checkpoint at the right offset.
        cache.apply_change(
            &uri,
            Some(3),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 4), Position::new(0, 4))),
                range_length: None,
                text: "😂".to_string(),
            },
        );
        assert_index_consistent(&cache, &uri);
        assert_eq!(cache.get(&uri).unwrap().text, "hi x😂 there");
    }

    #[test]
    fn incremental_changes_with_crlf_keep_index_consistent() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/crlf.txt").unwrap();
        cache.update(&uri, Some(1), "line one\r\nline two".to_string());
        assert_index_consistent(&cache, &uri);

        // Edit on the second line using positions from the CRLF-aware index.
        cache.apply_change(
            &uri,
            Some(2),
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 5), Position::new(1, 8))),
                range_length: None,
                text: "three".to_string(),
            },
        );
        assert_index_consistent(&cache, &uri);
        assert_eq!(cache.get(&uri).unwrap().text, "line one\r\nline three");
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
