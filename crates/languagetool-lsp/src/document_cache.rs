pub use crate::document::{CompletedCheckBlock, PreparedCheck};
use crate::document::{Document, DocumentChangeStatus};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tower_lsp_server::ls_types::{
    Diagnostic, Range, TextDocumentContentChangeEvent, TextDocumentItem, Uri,
};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Clone)]
pub struct DocumentCache {
    documents: Arc<RwLock<HashMap<String, DocumentEntry>>>,
}

#[derive(Debug)]
pub struct DocumentEntry {
    document: Document,
    document_id: u64,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentToken {
    document_id: u64,
    version: i32,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Applied,
    OutOfSync,
    Stale,
}

fn next_document_id() -> u64 {
    NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed)
}

impl DocumentToken {
    fn new(document_id: u64, version: i32, generation: u64) -> Self {
        Self {
            document_id,
            version,
            generation,
        }
    }

    #[cfg(test)]
    pub fn document_id(self) -> u64 {
        self.document_id
    }

    #[cfg(test)]
    pub fn generation(self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(document_id: u64, version: i32, generation: u64) -> Self {
        Self::new(document_id, version, generation)
    }
}

impl DocumentEntry {
    fn new(document: Document) -> Self {
        Self {
            document,
            document_id: next_document_id(),
            generation: 0,
        }
    }

    pub fn token(&self) -> DocumentToken {
        DocumentToken::new(self.document_id, self.document.version(), self.generation)
    }
}

impl DocumentCache {
    pub fn insert(&self, document: &TextDocumentItem) {
        let document = Document::from_text_document(document);
        log::debug!(
            "Inserting document {} version={:?}",
            document.uri().as_str(),
            document.version()
        );
        let mut documents = self.documents.write().expect("document cache poisoned");
        let entry = DocumentEntry::new(document);
        documents.insert(entry.document.uri().as_str().to_string(), entry);
    }

    fn full_update_locked(
        documents: &mut HashMap<String, DocumentEntry>,
        uri: &Uri,
        version: i32,
        text: String,
    ) {
        log::debug!(
            "Applying full document update for {uri} version={version:?} bytes={}",
            text.len(),
            uri = uri.as_str()
        );
        let key = uri.as_str().to_string();
        if let Some(entry) = documents.get_mut(&key) {
            entry.document.full_update(version, text);
        } else {
            documents.insert(
                key,
                DocumentEntry::new(Document::new(uri.clone(), version, None, text)),
            );
        }
    }

    fn incremental_update_locked(
        documents: &mut HashMap<String, DocumentEntry>,
        uri: &Uri,
        version: i32,
        range: Range,
        new_text: &str,
    ) -> Option<DocumentChangeStatus> {
        log::debug!(
            "Applying ranged document change for {uri} version={version:?} range={range:?} replacement_bytes={}",
            new_text.len(),
            uri = uri.as_str()
        );
        let key = uri.as_str().to_string();
        if let Some(entry) = documents.get_mut(&key) {
            return entry.document.incremental_update(version, range, new_text);
        }

        log::error!(
            "Received ranged change for uncached document {uri}; marking out of sync",
            uri = uri.as_str()
        );
        Some(DocumentChangeStatus::OutOfSync)
    }

    pub fn apply_changes(
        &self,
        uri: &Uri,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> ChangeStatus {
        let key = uri.as_str().to_string();
        let mut documents = self.documents.write().expect("document cache poisoned");
        if documents
            .get(&key)
            .is_some_and(|entry| entry.document.version() >= version)
        {
            log::warn!(
                "Ignoring stale document change for {uri} version={version}; cached version={:?}",
                documents.get(&key).map(|entry| entry.document.version()),
                uri = uri.as_str()
            );
            return ChangeStatus::Stale;
        }

        let mut status = ChangeStatus::Applied;
        for change in changes {
            let change_status = if let Some(range) = change.range {
                Self::incremental_update_locked(&mut documents, uri, version, range, &change.text)
            } else {
                Self::full_update_locked(&mut documents, uri, version, change.text);
                Some(DocumentChangeStatus::FullReplace)
            };
            if change_status == Some(DocumentChangeStatus::OutOfSync) {
                status = ChangeStatus::OutOfSync;
            }
        }
        status
    }

    #[cfg(test)]
    pub fn apply_change(
        &self,
        uri: &Uri,
        version: i32,
        change: TextDocumentContentChangeEvent,
    ) -> ChangeStatus {
        self.apply_changes(uri, version, vec![change])
    }

    pub fn remove(&self, uri: &Uri) {
        log::debug!("Removing document {uri} from cache", uri = uri.as_str());
        self.documents
            .write()
            .expect("document cache poisoned")
            .remove(uri.as_str());
    }

    pub fn token(&self, uri: &Uri) -> Option<DocumentToken> {
        self.documents
            .read()
            .expect("document cache poisoned")
            .get(uri.as_str())
            .map(DocumentEntry::token)
    }

    #[cfg(test)]
    pub fn with_bumped_entry<R>(
        &self,
        uri: &Uri,
        f: impl FnOnce(&mut DocumentEntry) -> R,
    ) -> Option<R> {
        let mut documents = self.documents.write().expect("document cache poisoned");
        let entry = documents.get_mut(uri.as_str())?;
        entry.generation += 1;
        Some(f(entry))
    }

    pub fn prepare_check(
        &self,
        uri: &Uri,
        options_key: String,
    ) -> Option<(PreparedCheck, DocumentToken)> {
        let mut documents = self.documents.write().expect("document cache poisoned");
        let entry = documents.get_mut(uri.as_str())?;
        entry.generation += 1;
        Some((entry.document.prepare_check(options_key), entry.token()))
    }

    pub fn prepare_check_if_current(
        &self,
        uri: &Uri,
        token: DocumentToken,
        options_key: String,
    ) -> Option<(PreparedCheck, DocumentToken)> {
        let mut documents = self.documents.write().expect("document cache poisoned");
        let entry = documents.get_mut(uri.as_str())?;
        if entry.token() != token {
            return None;
        }
        entry.generation += 1;
        Some((entry.document.prepare_check(options_key), entry.token()))
    }

    #[cfg(test)]
    pub fn with_bumped_entry_if_current<R>(
        &self,
        uri: &Uri,
        token: DocumentToken,
        f: impl FnOnce(&mut DocumentEntry) -> R,
    ) -> Option<R> {
        let mut documents = self.documents.write().expect("document cache poisoned");
        let entry = documents.get_mut(uri.as_str())?;
        if entry.token() != token {
            return None;
        }
        entry.generation += 1;
        Some(f(entry))
    }

    pub fn complete_check_if_current(
        &self,
        uri: &Uri,
        token: DocumentToken,
        checked_blocks: Vec<CompletedCheckBlock>,
    ) -> Option<Vec<Diagnostic>> {
        let mut documents = self.documents.write().expect("document cache poisoned");
        let entry = documents.get_mut(uri.as_str())?;
        if entry.token() != token {
            return None;
        }
        Some(entry.document.complete_check(checked_blocks))
    }

    pub fn urls(&self) -> Vec<Uri> {
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
    use tower_lsp_server::ls_types::Position;
    fn full_change(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }
    }

    fn prepared_text(entry: &mut DocumentEntry) -> String {
        let PreparedCheck::Check(prepared) = entry.document.prepare_check("test".to_string())
        else {
            panic!("document should be checkable");
        };
        prepared.text.as_ref().clone()
    }

    #[test]
    fn ranged_change_for_uncached_document_marks_out_of_sync_without_caching() {
        let cache = DocumentCache::default();
        let uri = "file:///tmp/missing.txt".parse::<Uri>().unwrap();
        let status = cache.apply_change(
            &uri,
            1,
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 0))),
                range_length: None,
                text: "x".to_string(),
            },
        );

        assert_eq!(status, ChangeStatus::OutOfSync);
        assert!(cache.token(&uri).is_none());
    }

    #[test]
    fn stale_change_does_not_roll_document_back() {
        let cache = DocumentCache::default();
        let uri = "file:///tmp/test.txt".parse::<Uri>().unwrap();
        cache.apply_change(&uri, 3, full_change("new text"));

        let status = cache.apply_change(&uri, 2, full_change("old text"));

        let token = cache.token(&uri).unwrap();
        cache
            .with_bumped_entry_if_current(&uri, token, |entry| {
                assert_eq!(status, ChangeStatus::Stale);
                assert_eq!(entry.document.version(), 3);
                assert_eq!(prepared_text(entry), "new text");
            })
            .unwrap();
    }

    #[test]
    fn multi_change_notification_uses_one_version_check() {
        let cache = DocumentCache::default();
        let uri = "file:///tmp/test.txt".parse::<Uri>().unwrap();
        cache.apply_change(&uri, 1, full_change("hello world"));

        let status = cache.apply_changes(
            &uri,
            2,
            vec![
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 0), Position::new(0, 5))),
                    range_length: None,
                    text: "hi".to_string(),
                },
                TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 8), Position::new(0, 8))),
                    range_length: None,
                    text: "!".to_string(),
                },
            ],
        );

        let token = cache.token(&uri).unwrap();
        cache
            .with_bumped_entry_if_current(&uri, token, |entry| {
                assert_eq!(status, ChangeStatus::Applied);
                assert_eq!(entry.document.version(), 2);
                assert_eq!(prepared_text(entry), "hi world!");
            })
            .unwrap();
    }

    #[test]
    fn document_version_change_invalidates_scheduled_token_without_generation_bump() {
        let cache = DocumentCache::default();
        let uri = "file:///tmp/test.txt".parse::<Uri>().unwrap();
        cache.apply_change(&uri, 1, full_change("first"));
        let scheduled = cache.token(&uri).unwrap();
        assert_eq!(scheduled.generation(), 0);

        cache.apply_change(&uri, 2, full_change("second"));

        assert_eq!(cache.token(&uri).unwrap().generation(), 0);
        assert!(cache
            .with_bumped_entry_if_current(&uri, scheduled, |_| ())
            .is_none());
    }

    #[test]
    fn tracks_generation_with_document_entry() {
        let cache = DocumentCache::default();
        let uri = "file:///tmp/test.txt".parse::<Uri>().unwrap();
        assert_eq!(cache.token(&uri), None);
        assert!(cache
            .with_bumped_entry(&uri, |entry| entry.token())
            .is_none());

        cache.apply_change(&uri, 1, full_change("hello"));
        let initial = cache.token(&uri).unwrap();
        assert_eq!(initial.generation(), 0);
        assert_eq!(
            cache
                .with_bumped_entry(&uri, |entry| entry.token())
                .unwrap()
                .generation(),
            1
        );
        assert_eq!(cache.token(&uri).unwrap().generation(), 1);
        assert_eq!(
            cache
                .with_bumped_entry(&uri, |entry| entry.token())
                .unwrap()
                .generation(),
            2
        );

        cache.remove(&uri);
        assert_eq!(cache.token(&uri), None);
    }

    #[test]
    fn replacing_document_assigns_new_document_id() {
        let cache = DocumentCache::default();
        let uri = "file:///tmp/test.txt".parse::<Uri>().unwrap();
        let first = TextDocumentItem {
            uri: uri.clone(),
            language_id: "plaintext".to_string(),
            version: 1,
            text: "first".to_string(),
        };
        let second = TextDocumentItem {
            uri: uri.clone(),
            language_id: "plaintext".to_string(),
            version: 1,
            text: "second".to_string(),
        };

        cache.insert(&first);
        let first_token = cache
            .with_bumped_entry(&uri, |entry| entry.token())
            .unwrap();
        cache.insert(&second);
        let second_token = cache.token(&uri).unwrap();

        assert_ne!(first_token.document_id(), second_token.document_id());
        assert_eq!(second_token.generation(), 0);
    }
}
