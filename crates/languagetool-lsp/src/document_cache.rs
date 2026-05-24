use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent, TextDocumentItem, Url};

#[derive(Debug, Clone)]
pub struct Document {
    pub uri: Url,
    pub version: Option<i32>,
    pub language_id: Option<String>,
    pub text: String,
}

impl Document {
    fn from_text_document(document: &TextDocumentItem) -> Self {
        Self {
            uri: document.uri.clone(),
            version: Some(document.version),
            language_id: Some(document.language_id.clone()),
            text: document.text.clone(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DocumentCache {
    documents: Arc<RwLock<HashMap<String, Document>>>,
}

impl DocumentCache {
    pub fn insert(&self, document: &TextDocumentItem) {
        let document = Document::from_text_document(document);
        self.documents
            .write()
            .expect("document cache poisoned")
            .insert(document.uri.to_string(), document);
    }

    pub fn update(&self, uri: &Url, version: Option<i32>, text: String) {
        let key = uri.to_string();
        let mut documents = self.documents.write().expect("document cache poisoned");
        if let Some(document) = documents.get_mut(&key) {
            document.version = version.or(document.version);
            document.text = text;
        } else {
            documents.insert(
                key,
                Document {
                    uri: uri.clone(),
                    version,
                    language_id: None,
                    text,
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
            if let Some(document) = documents.get_mut(&key) {
                if let Some((start, end)) =
                    byte_range_for_lsp_range(&document.text, range.start, range.end)
                {
                    document.text.replace_range(start..end, &change.text);
                    document.version = version.or(document.version);
                }
            } else {
                documents.insert(
                    key,
                    Document {
                        uri: uri.clone(),
                        version,
                        language_id: None,
                        text: change.text,
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
            .cloned()
    }

    pub fn urls(&self) -> Vec<Url> {
        self.documents
            .read()
            .expect("document cache poisoned")
            .values()
            .map(|document| document.uri.clone())
            .collect()
    }
}

fn byte_range_for_lsp_range(text: &str, start: Position, end: Position) -> Option<(usize, usize)> {
    let start = byte_offset_for_position(text, start)?;
    let end = byte_offset_for_position(text, end)?;
    Some((start.min(end), end.max(start)))
}

fn byte_offset_for_position(text: &str, position: Position) -> Option<usize> {
    let mut line = 0u32;
    let mut character = 0u32;

    for (byte_offset, ch) in text.char_indices() {
        if line == position.line && character == position.character {
            return Some(byte_offset);
        }

        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }

    if line == position.line && character == position.character {
        Some(text.len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Range;

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
}
