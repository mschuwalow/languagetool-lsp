use crate::diagnostics::{CheckedBlock, RawDiagnostic};
use crate::text_index::{ByteRange, TextIndex};
use std::collections::BTreeMap;
use tower_lsp_server::ls_types::{Diagnostic, Range};

#[derive(Debug, Default, Clone)]
pub struct DiagnosticsCache {
    blocks: BTreeMap<(usize, usize), CheckedBlock>,
    options_version: u64,
}

impl DiagnosticsCache {
    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    pub fn reset_if_options_changed(&mut self, options_version: u64) {
        if self.options_version != options_version {
            self.clear();
            self.options_version = options_version;
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

        let old_blocks = std::mem::take(&mut self.blocks);
        for (_, mut block) in old_blocks {
            if edit_invalidates_block(&block.byte_range, edit) {
                continue;
            }

            if block.byte_range.start.0 >= edit.end.0 && block.byte_range.start.0 != edit.start.0 {
                shift_range(&mut block.byte_range, delta);
                for diagnostic in &mut block.diagnostics {
                    shift_range(&mut diagnostic.doc_byte_range, delta);
                    update_diagnostic_range(diagnostic, index);
                }
            }

            for diagnostic in &mut block.diagnostics {
                diagnostic.data.document_version = document_version;
            }

            self.blocks.insert(range_key(&block.byte_range), block);
        }
    }

    #[cfg(test)]
    pub fn contains_block(&self, byte_range: &ByteRange) -> bool {
        self.blocks.contains_key(&range_key(byte_range))
    }

    pub fn retain_current_and_collect_uncached<T>(
        &mut self,
        current_blocks: Vec<T>,
        byte_range: impl Fn(&T) -> &ByteRange,
    ) -> Vec<T> {
        let mut old_blocks = std::mem::take(&mut self.blocks);
        let mut uncached = Vec::new();

        for current_block in current_blocks {
            let key = range_key(byte_range(&current_block));
            if let Some(block) = old_blocks.remove(&key) {
                self.blocks.insert(key, block);
            } else {
                uncached.push(current_block);
            }
        }

        uncached
    }

    pub fn store_checked_block(&mut self, block: CheckedBlock) {
        let key = range_key(&block.byte_range);
        debug_assert!(
            !self.blocks.contains_key(&key),
            "checked block should not already be cached"
        );
        self.blocks.insert(key, block);
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.blocks
            .values()
            .flat_map(|block| block.diagnostics.iter().map(RawDiagnostic::finalize))
            .collect()
    }
}

fn edit_invalidates_block(block: &ByteRange, edit: &ByteRange) -> bool {
    if edit.start == edit.end {
        return block.start <= edit.start && edit.start <= block.end;
    }

    ranges_overlap(block, edit)
}

fn range_key(range: &ByteRange) -> (usize, usize) {
    (range.start.0, range.end.0)
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
        let sub = delta.unsigned_abs();
        debug_assert!(offset >= sub, "shift_offset underflow: {offset} - {sub}");
        offset - sub
    } else {
        offset + delta as usize
    }
}

fn update_diagnostic_range(diagnostic: &mut RawDiagnostic, index: &TextIndex) {
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
    use crate::diagnostics::DiagnosticData;
    use tower_lsp_server::ls_types::Position;

    fn raw_diagnostic(start: usize, end: usize) -> RawDiagnostic {
        RawDiagnostic {
            doc_byte_range: ByteRange::new(start, end),
            diagnostic: Diagnostic {
                range: Range {
                    start: Position::new(0, start as u32),
                    end: Position::new(0, end as u32),
                },
                ..Diagnostic::default()
            },
            data: DiagnosticData {
                rule_id: "RULE".to_string(),
                category_id: None,
                issue_type: None,
                replacements: Vec::new(),
                matched_text: "test".to_string(),
                document_version: 1,
            },
        }
    }

    fn block(start: usize, end: usize) -> CheckedBlock {
        CheckedBlock {
            byte_range: ByteRange::new(start, end),
            diagnostics: vec![raw_diagnostic(start + 2, start + 6)],
        }
    }

    #[test]
    fn shifted_diagnostics_have_updated_lsp_ranges() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(CheckedBlock {
            byte_range: ByteRange::new(10, 20),
            diagnostics: vec![raw_diagnostic(12, 16)],
        });

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
        cache.store_checked_block(CheckedBlock {
            byte_range: ByteRange::new(10, 20),
            diagnostics: vec![raw_diagnostic(12, 16)],
        });

        let index = TextIndex::new("0123456789xxxxx56789");
        cache.apply_edit(&ByteRange::new(12, 15), 5, &index, 1);

        assert!(!cache.contains_block(&ByteRange::new(10, 22)));
    }

    #[test]
    fn retain_current_and_collect_uncached_drops_stale_blocks() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(block(10, 20));
        cache.store_checked_block(block(30, 40));

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
        cache.store_checked_block(CheckedBlock {
            byte_range: ByteRange::new(0, 10),
            diagnostics: vec![raw_diagnostic(2, 5)],
        });

        let index = TextIndex::new("0123456789x");
        cache.apply_edit(&ByteRange::new(10, 10), 1, &index, 1);

        assert!(!cache.contains_block(&ByteRange::new(0, 10)));
    }

    #[test]
    fn apply_edit_refreshes_embedded_document_version() {
        let mut cache = DiagnosticsCache::default();
        cache.store_checked_block(CheckedBlock {
            byte_range: ByteRange::new(0, 4),
            diagnostics: vec![raw_diagnostic(0, 4)],
        });

        let index = TextIndex::new("test x");
        cache.apply_edit(&ByteRange::new(5, 5), 1, &index, 2);

        let diagnostics = cache.diagnostics();
        let data: DiagnosticData =
            serde_json::from_value(diagnostics[0].data.clone().unwrap()).unwrap();

        assert_eq!(data.document_version, 2);
    }

    #[test]
    fn changed_options_clear_cache() {
        let mut cache = DiagnosticsCache::default();
        cache.reset_if_options_changed(1);
        cache.store_checked_block(block(0, 4));

        cache.reset_if_options_changed(2);

        assert!(!cache.contains_block(&ByteRange::new(0, 4)));
    }
}
