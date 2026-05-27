use crate::document::{ChangeStatus, Document};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Url};

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

    fn full_update(&self, uri: &Url, version: i32, text: String) {
        log::debug!(
            "Applying full document update for {uri} version={version:?} bytes={}",
            text.len()
        );
        let key = uri.to_string();
        let mut documents = self.documents.write().expect("document cache poisoned");
        if let Some(entry) = documents.get_mut(&key) {
            entry.document.full_update(version, text);
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

    fn incremental_update(
        &self,
        uri: &Url,
        version: i32,
        range: tower_lsp::lsp_types::Range,
        new_text: &str,
    ) -> ChangeStatus {
        log::debug!(
            "Applying ranged document change for {uri} version={version:?} range={range:?} replacement_bytes={}",
            new_text.len()
        );
        let key = uri.to_string();
        let mut documents = self.documents.write().expect("document cache poisoned");
        if let Some(entry) = documents.get_mut(&key) {
            return entry.document.incremental_update(version, range, new_text);
        }

        log::error!("Received ranged change for uncached document {uri}; marking out of sync");
        documents.insert(
            key,
            DocumentEntry {
                document: Document::out_of_sync(uri.clone(), version),
                generation: 0,
            },
        );
        ChangeStatus::OutOfSync
    }

    pub fn apply_change(
        &self,
        uri: &Url,
        version: i32,
        change: TextDocumentContentChangeEvent,
    ) -> ChangeStatus {
        if let Some(range) = change.range {
            self.incremental_update(uri, version, range, &change.text)
        } else {
            self.full_update(uri, version, change.text);
            ChangeStatus::Applied
        }
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
    use tower_lsp::lsp_types::{Position, Range};

    #[test]
    fn ranged_change_for_uncached_document_marks_out_of_sync() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/missing.txt").unwrap();
        let status = cache.apply_change(
            &uri,
            1,
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 0))),
                range_length: None,
                text: "x".to_string(),
            },
        );

        let document = cache.get(&uri).unwrap();
        assert_eq!(status, ChangeStatus::OutOfSync);
        assert_eq!(document.version(), 1);
        assert!(document.checkable(|_| true).is_none());
    }

    #[test]
    fn tracks_generation_with_document_entry() {
        let cache = DocumentCache::default();
        let uri = Url::parse("file:///tmp/test.txt").unwrap();
        assert_eq!(cache.generation(&uri), 0);
        assert_eq!(cache.bump_generation(&uri), 0);

        cache.full_update(&uri, 1, "hello".to_string());
        assert_eq!(cache.generation(&uri), 0);
        assert_eq!(cache.bump_generation(&uri), 1);
        assert_eq!(cache.generation(&uri), 1);
        assert_eq!(cache.bump_generation(&uri), 2);

        cache.remove(&uri);
        assert_eq!(cache.generation(&uri), 0);
    }
}
