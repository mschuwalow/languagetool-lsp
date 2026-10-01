use crate::config::{ClientOptions, LanguageToolBackend};
use crate::diagnostics::{
    CheckedBlock, RawDiagnostic, diagnostic_data_for_text, make_lsp_diagnostic_for_range,
    match_utf16_range,
};
use crate::document::{Document, PreparedCheck, PreparedCheckData};
use crate::languagetool::{Annotation, LanguageToolError, LanguageToolMatch, LanguageToolResponse};
use crate::masking::CheckBlock;
use crate::runtime_config::RuntimeConfig;
use crate::text_index::{ByteRange, TextIndex, Utf16Range};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::{
    MessageType, Range, TextDocumentContentChangeEvent, TextDocumentItem,
};

enum DocumentEvent {
    Changed {
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    },
    Saved,
    Close,
    DebounceElapsed(u64),
    CheckCompleted {
        generation: u64,
        checked_blocks: Vec<CheckedBlock>,
    },
}

#[derive(Clone)]
pub struct DocumentActorHandle(mpsc::UnboundedSender<DocumentEvent>);

impl DocumentActorHandle {
    pub fn changed(&self, version: i32, changes: Vec<TextDocumentContentChangeEvent>) {
        let _ = self.0.send(DocumentEvent::Changed { version, changes });
    }

    pub fn saved(&self) {
        let _ = self.0.send(DocumentEvent::Saved);
    }

    pub fn close(&self) {
        let _ = self.0.send(DocumentEvent::Close);
    }
}

pub fn spawn(
    client: Client,
    config: watch::Receiver<Arc<RuntimeConfig>>,
    text_document: &TextDocumentItem,
) -> DocumentActorHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let actor = DocumentActor {
        document: Document::from_text_document(text_document),
        client,
        config,
        events: tx.clone(),
        generation: 0,
    };
    tokio::spawn(run(actor, rx));
    DocumentActorHandle(tx)
}

struct DocumentActor {
    document: Document,
    client: Client,
    config: watch::Receiver<Arc<RuntimeConfig>>,
    events: mpsc::UnboundedSender<DocumentEvent>,
    generation: u64,
}

// `Close` is a message rather than relying on the channel closing because
// the actor holds its own `events` sender for self-scheduled checks, so
// the channel never naturally closes on its own.
async fn run(mut actor: DocumentActor, mut events: mpsc::UnboundedReceiver<DocumentEvent>) {
    let uri = actor.document.uri().clone();
    log::info!("Opened document {}", uri.as_str());

    if actor.config.borrow().options.check_on_open {
        actor.start_check().await;
    }

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(DocumentEvent::Close) | None => break,
                Some(event) => actor.handle(event).await,
            },
            Ok(()) = actor.config.changed() => actor.start_check().await,
        }
    }

    log::info!("Closed document {}", uri.as_str());
    actor
        .client
        .publish_diagnostics(uri, Vec::new(), None)
        .await;
}

impl DocumentActor {
    async fn handle(&mut self, event: DocumentEvent) {
        match event {
            DocumentEvent::Changed { version, changes } => self.apply_changes(version, changes),
            DocumentEvent::Saved if self.config.borrow().options.check_on_save => {
                self.start_check().await;
            }
            DocumentEvent::Saved => {}
            DocumentEvent::DebounceElapsed(generation) if generation == self.generation => {
                self.start_check().await;
            }
            DocumentEvent::DebounceElapsed(_) => {}
            DocumentEvent::CheckCompleted {
                generation,
                checked_blocks,
            } if generation == self.generation => self.publish(checked_blocks).await,
            DocumentEvent::CheckCompleted { .. } => {}
            DocumentEvent::Close => unreachable!("handled in the main loop"),
        }
    }

    fn apply_changes(&mut self, version: i32, changes: Vec<TextDocumentContentChangeEvent>) {
        if self.document.version() >= version {
            log::warn!(
                "Ignoring stale document change for {} version={version}",
                self.document.uri().as_str()
            );
            return;
        }
        for change in changes {
            match change.range {
                Some(range) => self
                    .document
                    .incremental_update(version, range, &change.text),
                None => self.document.full_update(version, change.text),
            }
        }
        if self.config.borrow().options.check_while_typing {
            self.schedule_debounced_check();
        }
    }

    fn schedule_debounced_check(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        let debounce = self.config.borrow().options.debounce_ms;
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            let _ = events.send(DocumentEvent::DebounceElapsed(generation));
        });
    }

    // Bumping `generation` on every start, not just on edits, lets an
    // immediate check (on save, or triggered by a config change) supersede
    // an older debounce that's still pending or a check still in flight.
    async fn start_check(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        let config = Arc::clone(&self.config.borrow());
        match self.document.prepare_check(config.options_version) {
            PreparedCheck::Check(data) => {
                let client = self.client.clone();
                let events = self.events.clone();
                tokio::spawn(async move {
                    let checked_blocks = run_check(data, &config, &client).await;
                    let _ = events.send(DocumentEvent::CheckCompleted {
                        generation,
                        checked_blocks,
                    });
                });
            }
            PreparedCheck::ReuseCached | PreparedCheck::Clear => {
                self.publish(Vec::new()).await;
            }
        }
    }

    async fn publish(&mut self, checked_blocks: Vec<CheckedBlock>) {
        let diagnostics = self.document.complete_check(checked_blocks);
        let uri = self.document.uri().clone();
        let version = self.document.version();
        log::debug!(
            "Publishing {} diagnostic(s) for {} version={version}",
            diagnostics.len(),
            uri.as_str()
        );
        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }
}

async fn run_check(
    data: PreparedCheckData,
    config: &RuntimeConfig,
    client: &Client,
) -> Vec<CheckedBlock> {
    let mut checks = tokio::task::JoinSet::new();
    for block in data.blocks {
        let language_tool = Arc::clone(&config.language_tool);
        let options = Arc::clone(&config.options);
        checks.spawn(async move {
            let result = language_tool
                .check_annotated(&block.annotated, &options)
                .await;
            (block, result)
        });
    }

    let mut responses = Vec::new();
    while let Some(result) = checks.join_next().await {
        match result {
            Ok((block, Ok(response))) => responses.push((block, response)),
            Ok((_, Err(err))) => log_check_error(client, &config.options, err).await,
            Err(err) => {
                let message = format!("LanguageTool check task failed: {err}");
                log::warn!("{message}");
                client.log_message(MessageType::WARNING, message).await;
            }
        }
    }

    responses.sort_by_key(|(block, _)| block.byte_range.start.0);
    completed_blocks_from_responses(
        responses,
        &data.text,
        &data.index,
        data.version,
        &config.options,
    )
}

async fn log_check_error(client: &Client, options: &ClientOptions, err: LanguageToolError) {
    let message = match &err {
        LanguageToolError::Api { .. }
            if matches!(options.backend, LanguageToolBackend::Custom { .. }) =>
        {
            format!(
                "LanguageTool is not reachable at {}. Is the custom server running? {err}",
                options.endpoint()
            )
        }
        _ => format!("LanguageTool check failed: {err}"),
    };
    log::warn!("{message}");
    client.log_message(MessageType::WARNING, message).await;
}

struct TextSegment<'a> {
    lt_utf16: Utf16Range,
    doc_byte: ByteRange,
    text: &'a str,
}

fn completed_blocks_from_responses(
    responses: Vec<(CheckBlock, LanguageToolResponse)>,
    text: &str,
    index: &TextIndex,
    version: i32,
    options: &ClientOptions,
) -> Vec<CheckedBlock> {
    responses
        .into_iter()
        .map(|(block, response)| {
            let diagnostics =
                diagnostics_for_block(&block, response.matches, text, index, version, options);
            CheckedBlock {
                byte_range: block.byte_range,
                diagnostics,
            }
        })
        .collect()
}

fn diagnostics_for_block(
    block: &CheckBlock,
    matches: Vec<LanguageToolMatch>,
    text: &str,
    index: &TextIndex,
    version: i32,
    options: &ClientOptions,
) -> Vec<RawDiagnostic> {
    let segments = text_segments_for_block(block);
    matches
        .iter()
        .filter_map(|item| match_utf16_range(item).map(|range| (item, range)))
        .filter_map(|(item, lt_range)| {
            let doc_byte_range = map_lt_range_to_doc_bytes(&segments, lt_range)?;
            let matched_text = text.get(doc_byte_range.start.0..doc_byte_range.end.0)?;
            if matched_text.trim().is_empty() || options.is_ignored_word(matched_text) {
                return None;
            }

            let utf16_start = index.utf16_offset_for_byte(doc_byte_range.start);
            let utf16_end = index.utf16_offset_for_byte(doc_byte_range.end);
            let range = Range {
                start: index.position(utf16_start),
                end: index.position(utf16_end),
            };
            let data = diagnostic_data_for_text(matched_text.to_string(), item, options, version);
            Some(RawDiagnostic {
                doc_byte_range,
                diagnostic: make_lsp_diagnostic_for_range(range, item, options),
                data,
            })
        })
        .collect()
}

fn text_segments_for_block(block: &CheckBlock) -> Vec<TextSegment<'_>> {
    let mut segments = Vec::new();
    let mut lt_utf16_cursor = 0usize;
    let mut doc_byte_cursor = block.byte_range.start.0;

    for annotation in &block.annotated.annotation {
        let content = annotation_content(annotation);
        let utf16_len = content.chars().map(char::len_utf16).sum::<usize>();
        let byte_len = content.len();
        if let Annotation::Text { text } = annotation {
            segments.push(TextSegment {
                lt_utf16: Utf16Range::new(lt_utf16_cursor, lt_utf16_cursor + utf16_len),
                doc_byte: ByteRange::new(doc_byte_cursor, doc_byte_cursor + byte_len),
                text,
            });
        }
        lt_utf16_cursor += utf16_len;
        doc_byte_cursor += byte_len;
    }

    segments
}

fn annotation_content(annotation: &Annotation) -> &str {
    match annotation {
        Annotation::Text { text } => text,
        Annotation::Markup { markup, .. } => markup,
    }
}

fn map_lt_range_to_doc_bytes(
    segments: &[TextSegment<'_>],
    lt_range: Utf16Range,
) -> Option<ByteRange> {
    let segment = segments.iter().find(|segment| {
        segment.lt_utf16.start <= lt_range.start && lt_range.end <= segment.lt_utf16.end
    });
    let segment = segment?;
    let relative_start = lt_range.start.0 - segment.lt_utf16.start.0;
    let relative_end = lt_range.end.0 - segment.lt_utf16.start.0;
    let byte_start =
        segment.doc_byte.start.0 + byte_offset_for_utf16_in_text(segment.text, relative_start)?;
    let byte_end =
        segment.doc_byte.start.0 + byte_offset_for_utf16_in_text(segment.text, relative_end)?;
    Some(ByteRange::new(byte_start, byte_end))
}

fn byte_offset_for_utf16_in_text(text: &str, target: usize) -> Option<usize> {
    let mut utf16 = 0usize;
    for (byte, ch) in text.char_indices() {
        if utf16 == target {
            return Some(byte);
        }
        utf16 += ch.len_utf16();
        if utf16 == target {
            return Some(byte + ch.len_utf8());
        }
        if utf16 > target {
            return None;
        }
    }
    (utf16 == target).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languagetool::{LanguageToolCategory, LanguageToolReplacement, LanguageToolRule};
    use tower_lsp_server::ls_types::{Position, Uri};

    struct TestRequest {
        block: CheckBlock,
        text: Arc<String>,
        index: Arc<TextIndex>,
    }

    fn prepare_test_request(mut document: Document) -> TestRequest {
        let PreparedCheck::Check(prepared) = document.prepare_check(0) else {
            panic!("document should be checkable");
        };
        let block = prepared
            .blocks
            .into_iter()
            .next()
            .expect("document should have a check block");
        TestRequest {
            block,
            text: prepared.text,
            index: prepared.index,
        }
    }

    #[test]
    fn builds_diagnostics_for_document() {
        let document = Document::new(
            "file:///tmp/test.txt".parse::<Uri>().unwrap(),
            1,
            Some("plaintext".to_string()),
            "This are a tset.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "Possible spelling mistake found.".to_string(),
            short_message: None,
            offset: 11,
            length: 4,
            replacements: vec![LanguageToolReplacement {
                value: Some("test".to_string()),
            }],
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "MORFOLOGIK_RULE_EN_US".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: Some("misspelling".to_string()),
                category: Box::new(LanguageToolCategory {
                    id: Some("TYPOS".to_string()),
                    name: None,
                }),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 11));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 15));
    }

    #[test]
    fn diagnostics_use_original_document_offsets() {
        let document = Document::new(
            "file:///tmp/test.rs".parse::<Uri>().unwrap(),
            1,
            Some("rust".to_string()),
            "let value = 1; // This are a comment.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "The singular demonstrative pronoun does not agree.".to_string(),
            short_message: None,
            offset: 3,
            length: 4,
            replacements: Vec::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "THIS_NNS".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: None,
                category: Box::new(LanguageToolCategory::new()),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 18));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 22));
    }

    #[test]
    fn diagnostics_drop_matches_in_markup_regions() {
        let document = Document::new(
            "file:///tmp/test.rs".parse::<Uri>().unwrap(),
            1,
            Some("rust".to_string()),
            "let typoo = 1; // This are a comment.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "Possible spelling mistake found.".to_string(),
            short_message: None,
            offset: 0,
            length: 2,
            replacements: Vec::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "MORFOLOGIK_RULE_EN_US".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: Some("misspelling".to_string()),
                category: Box::new(LanguageToolCategory {
                    id: Some("TYPOS".to_string()),
                    name: None,
                }),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostics_use_languagetool_utf16_offsets() {
        let document = Document::new(
            "file:///tmp/test.txt".parse::<Uri>().unwrap(),
            1,
            Some("plaintext".to_string()),
            "😀 This are a tset.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "The verb 'are' is plural.".to_string(),
            short_message: None,
            offset: 3,
            length: 8,
            replacements: Vec::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "PLURAL_VERB_AFTER_THIS".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: Some("grammar".to_string()),
                category: Box::new(LanguageToolCategory {
                    id: Some("GRAMMAR".to_string()),
                    name: None,
                }),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 3));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 11));

        assert_eq!(diagnostics[0].data.matched_text, "This are");
    }
}
