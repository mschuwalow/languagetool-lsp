use crate::diagnostics::DiagnosticData;
use crate::text_index::{ByteRange, TextIndex};
use tower_lsp_server::ls_types::{Diagnostic, Range};

#[derive(Debug, Default, Clone)]
pub struct DiagnosticsCache {
    blocks: Vec<CachedBlock>,
    options_key: String,
}

#[derive(Debug, Clone)]
pub struct CachedDiagnostic {
    pub doc_byte_range: ByteRange,
    pub diagnostic: Diagnostic,
}

#[derive(Debug, Clone)]
struct CachedBlock {
    byte_range: ByteRange,
    diagnostics: Vec<CachedDiagnostic>,
}

impl DiagnosticsCache {
    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    pub fn reset_if_options_changed(&mut self, options_key: String) {
        if self.options_key != options_key {
            self.clear();
            self.options_key = options_key;
        }
    }

    pub fn apply_edit(
        &mut self,
        edit: &ByteRange,
        new_len: usize,
        index: &TextIndex,
        document_version: i32,
    ) {
        let old_len = edit.end.0 - edit.start.0;
        let delta = new_len as isize - old_len as isize;

        self.blocks.retain_mut(|block| {
            if edit_invalidates_block(&block.byte_range, edit) {
                return false;
            }

            if block.byte_range.start.0 >= edit.end.0 && block.byte_range.start.0 != edit.start.0 {
                shift_range(&mut block.byte_range, delta);
                for diagnostic in &mut block.diagnostics {
                    shift_range(&mut diagnostic.doc_byte_range, delta);
                    update_diagnostic_range(diagnostic, index);
                }
            }

            for diagnostic in &mut block.diagnostics {
                update_diagnostic_document_version(diagnostic, document_version);
            }

            true
        });
        debug_assert!(is_sorted_by_range(&self.blocks));
    }

    #[cfg(test)]
    pub fn contains_block(&self, byte_range: &ByteRange) -> bool {
        self.find_block(byte_range).is_ok()
    }

    pub fn retain_current_and_collect_uncached<T>(
        &mut self,
        current_blocks: Vec<T>,
        byte_range: impl Fn(&T) -> &ByteRange,
    ) -> Vec<T> {
        let mut cached_blocks = std::mem::take(&mut self.blocks).into_iter().peekable();
        let mut uncached = Vec::new();
        let mut previous_current_key = None;

        for current_block in current_blocks {
            let current_key = range_key(byte_range(&current_block));
            debug_assert!(previous_current_key.is_none_or(|previous| previous < current_key));
            previous_current_key = Some(current_key);

            while cached_blocks
                .peek()
                .is_some_and(|block| range_key(&block.byte_range) < current_key)
            {
                cached_blocks.next();
            }

            if cached_blocks
                .peek()
                .is_some_and(|block| range_key(&block.byte_range) == current_key)
            {
                self.blocks
                    .push(cached_blocks.next().expect("cached block should exist"));
            } else {
                uncached.push(current_block);
            }
        }

        debug_assert!(is_sorted_by_range(&self.blocks));
        uncached
    }

    pub fn store_checked_block(
        &mut self,
        byte_range: ByteRange,
        diagnostics: Vec<CachedDiagnostic>,
    ) {
        let Err(index) = self.find_block(&byte_range) else {
            debug_assert!(false, "checked block should not already be cached");
            return;
        };
        self.blocks.insert(
            index,
            CachedBlock {
                byte_range,
                diagnostics,
            },
        );
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.blocks
            .iter()
            .flat_map(|block| {
                block
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.diagnostic.clone())
            })
            .collect()
    }

    fn find_block(&self, byte_range: &ByteRange) -> Result<usize, usize> {
        self.blocks
            .binary_search_by_key(&range_key(byte_range), |block| range_key(&block.byte_range))
    }
}

fn edit_invalidates_block(block: &ByteRange, edit: &ByteRange) -> bool {
    if edit.start == edit.end {
        return block.start <= edit.start && edit.start <= block.end;
    }

    ranges_overlap(block, edit)
}

fn update_diagnostic_document_version(diagnostic: &mut CachedDiagnostic, document_version: i32) {
    let Some(data) = diagnostic.diagnostic.data.clone() else {
        return;
    };
    let Ok(mut data) = serde_json::from_value::<DiagnosticData>(data) else {
        return;
    };
    data.document_version = Some(document_version);
    diagnostic.diagnostic.data = serde_json::to_value(data).ok();
}

fn range_key(range: &ByteRange) -> (usize, usize) {
    (range.start.0, range.end.0)
}

fn is_sorted_by_range(blocks: &[CachedBlock]) -> bool {
    blocks
        .windows(2)
        .all(|pair| range_key(&pair[0].byte_range) < range_key(&pair[1].byte_range))
}

fn ranges_overlap(left: &ByteRange, right: &ByteRange) -> bool {
    left.start.0 < right.end.0 && left.end.0 > right.start.0
}

fn shift_range(range: &mut ByteRange, delta: isize) {
    range.start.0 = shift_offset(range.start.0, delta);
    range.end.0 = shift_offset(range.end.0, delta);
}

fn shift_offset(offset: usize, delta: isize) -> usize {
    if delta.is_negative() {
        offset - delta.unsigned_abs()
    } else {
        offset + delta as usize
    }
}

fn update_diagnostic_range(diagnostic: &mut CachedDiagnostic, index: &TextIndex) {
    let utf16_start = index.utf16_offset_for_byte(diagnostic.doc_byte_range.start);
    let utf16_end = index.utf16_offset_for_byte(diagnostic.doc_byte_range.end);
    diagnostic.diagnostic.range = Range {
        start: index.position(utf16_start),
        end: index.position(utf16_end),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;

    fn cached_diagnostic(start: usize, end: usize) -> CachedDiagnostic {
        CachedDiagnostic {
            doc_byte_range: ByteRange::new(start, end),
            diagnostic: Diagnostic {
                range: Range {
                    start: Position::new(0, start as u32),
                    end: Position::new(0, end as u32),
                },
                ..Diagnostic::default()
            },
        }
    }

    #[test]
    fn shifted_diagnostics_have_updated_lsp_ranges() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(ByteRange::new(10, 20), vec![cached_diagnostic(12, 16)]);

        let index = TextIndex::new("abcxxxxx0123456789012345");
        cache.apply_edit(&ByteRange::new(3, 3), 5, &index, 1);
        assert!(cache.contains_block(&ByteRange::new(15, 25)));
        let diagnostics = cache.diagnostics();

        assert_eq!(diagnostics[0].range.start, Position::new(0, 17));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 21));
    }

    #[test]
    fn overlapping_edit_drops_block() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(ByteRange::new(10, 20), vec![cached_diagnostic(12, 16)]);

        let index = TextIndex::new("0123456789xxxxx56789");
        cache.apply_edit(&ByteRange::new(12, 15), 5, &index, 1);

        assert!(!cache.contains_block(&ByteRange::new(10, 22)));
    }

    #[test]
    fn retain_current_and_collect_uncached_drops_stale_blocks() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(ByteRange::new(10, 20), vec![cached_diagnostic(12, 16)]);
        cache.store_checked_block(ByteRange::new(30, 40), vec![cached_diagnostic(32, 36)]);

        let uncached = cache.retain_current_and_collect_uncached(
            vec![ByteRange::new(30, 40), ByteRange::new(50, 60)],
            |range| range,
        );

        assert!(!cache.contains_block(&ByteRange::new(10, 20)));
        assert!(cache.contains_block(&ByteRange::new(30, 40)));
        assert_eq!(uncached, vec![ByteRange::new(50, 60)]);
    }

    #[test]
    fn insertion_at_block_boundary_drops_block() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(ByteRange::new(0, 10), vec![cached_diagnostic(2, 5)]);

        let index = TextIndex::new("0123456789x");
        cache.apply_edit(&ByteRange::new(10, 10), 1, &index, 1);

        assert!(!cache.contains_block(&ByteRange::new(0, 10)));
    }

    #[test]
    fn apply_edit_refreshes_embedded_document_version() {
        let mut cache = DiagnosticsCache::default();
        let mut diagnostic = cached_diagnostic(0, 4);
        diagnostic.diagnostic.data = serde_json::to_value(DiagnosticData {
            rule_id: "RULE".to_string(),
            category_id: None,
            issue_type: None,
            replacements: Vec::new(),
            matched_text: "test".to_string(),
            document_version: Some(1),
        })
        .ok();
        cache.store_checked_block(ByteRange::new(0, 4), vec![diagnostic]);

        let index = TextIndex::new("test x");
        cache.apply_edit(&ByteRange::new(5, 5), 1, &index, 2);

        let diagnostics = cache.diagnostics();
        let data: DiagnosticData =
            serde_json::from_value(diagnostics[0].data.clone().unwrap()).unwrap();

        assert_eq!(data.document_version, Some(2));
    }

    #[test]
    fn changed_options_clear_cache() {
        let mut cache = DiagnosticsCache::default();
        cache.reset_if_options_changed("one".to_string());
        cache.store_checked_block(ByteRange::new(0, 4), vec![cached_diagnostic(0, 4)]);

        cache.reset_if_options_changed("two".to_string());

        assert!(!cache.contains_block(&ByteRange::new(0, 4)));
    }
}
