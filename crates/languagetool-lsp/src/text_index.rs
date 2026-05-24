use tower_lsp::lsp_types::{Position, Range};

/// Per-document index built in a single O(n) pass.
///
/// Provides O(log n) UTF-16 ↔ byte offset conversion and O(log n) flat-UTF-16-offset →
/// LSP `Position` lookup.
///
/// The UTF-16/byte mapping works by recording a `(utf16_offset, byte_offset)` checkpoint
/// at the position *after* every non-ASCII character. Between consecutive checkpoints all
/// characters are ASCII (1 UTF-16 unit == 1 byte), so any offset within a gap resolves
/// by simple arithmetic from the nearest checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextIndex {
    line_starts_utf16: Vec<usize>,
    checkpoints: Vec<(usize, usize)>,
    total_utf16: usize,
    total_bytes: usize,
}

impl TextIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts_utf16 = vec![0usize];
        let mut checkpoints: Vec<(usize, usize)> = Vec::new();
        let mut utf16 = 0usize;
        let mut prev_was_cr = false;

        for (byte_off, ch) in text.char_indices() {
            let u16_len = ch.len_utf16();
            let u8_len = ch.len_utf8();

            // A bare \r (not followed by \n) ends a line; \r\n counts as one line ending
            // and the new line starts after the \n.
            if prev_was_cr && ch != '\n' {
                line_starts_utf16.push(utf16);
            }
            prev_was_cr = ch == '\r';

            utf16 += u16_len;
            if u16_len != u8_len {
                checkpoints.push((utf16, byte_off + u8_len));
            }
            if ch == '\n' {
                line_starts_utf16.push(utf16);
            }
        }
        // Trailing \r with no following \n.
        if prev_was_cr {
            line_starts_utf16.push(utf16);
        }

        Self {
            line_starts_utf16,
            checkpoints,
            total_utf16: utf16,
            total_bytes: text.len(),
        }
    }

    /// Incrementally update the index for a single range replacement without
    /// re-scanning the whole document.
    ///
    /// `byte_start`/`byte_end` are the byte boundaries of the replaced region in the
    /// *old* text. `utf16_start`/`utf16_end` are the corresponding UTF-16 offsets.
    /// `new_text` is the replacement string. Obtain all four offset values cheaply via
    /// [`Self::edit_offsets`] before mutating the text.
    pub fn apply_edit(
        &mut self,
        byte_start: usize,
        byte_end: usize,
        utf16_start: usize,
        utf16_end: usize,
        new_text: &str,
    ) {
        let new_utf16_len: usize = new_text.chars().map(|c| c.len_utf16()).sum();
        let new_byte_len = new_text.len();
        let utf16_delta = new_utf16_len as isize - (utf16_end - utf16_start) as isize;
        let byte_delta = new_byte_len as isize - (byte_end - byte_start) as isize;

        // Checkpoints inside [byte_start, byte_end) are replaced by checkpoints from
        // new_text; those after byte_end are shifted by the byte/utf16 deltas.
        let cp_first = self
            .checkpoints
            .partition_point(|&(_, cb)| cb <= byte_start);
        let cp_last = self.checkpoints.partition_point(|&(_, cb)| cb <= byte_end);

        let mut new_cps: Vec<(usize, usize)> = Vec::new();
        let mut utf16 = utf16_start;
        for (off, ch) in new_text.char_indices() {
            let u16_len = ch.len_utf16();
            let u8_len = ch.len_utf8();
            utf16 += u16_len;
            if u16_len != u8_len {
                new_cps.push((utf16, byte_start + off + u8_len));
            }
        }

        for (cu, cb) in &mut self.checkpoints[cp_last..] {
            *cu = (*cu as isize + utf16_delta) as usize;
            *cb = (*cb as isize + byte_delta) as usize;
        }
        self.checkpoints.splice(cp_first..cp_last, new_cps);

        // Line starts inside (utf16_start, utf16_end] are replaced by line starts
        // contributed by new_text; those after utf16_end are shifted by utf16_delta.
        let ls_first = self
            .line_starts_utf16
            .partition_point(|&ls| ls <= utf16_start);
        let ls_last = self
            .line_starts_utf16
            .partition_point(|&ls| ls <= utf16_end);

        let mut new_ls: Vec<usize> = Vec::new();
        let mut utf16 = utf16_start;
        let mut prev_was_cr = false;
        for ch in new_text.chars() {
            if prev_was_cr && ch != '\n' {
                new_ls.push(utf16);
            }
            prev_was_cr = ch == '\r';
            utf16 += ch.len_utf16();
            if ch == '\n' {
                new_ls.push(utf16);
            }
        }
        if prev_was_cr {
            new_ls.push(utf16);
        }

        for ls in &mut self.line_starts_utf16[ls_last..] {
            *ls = (*ls as isize + utf16_delta) as usize;
        }
        self.line_starts_utf16.splice(ls_first..ls_last, new_ls);

        self.total_utf16 = (self.total_utf16 as isize + utf16_delta) as usize;
        self.total_bytes = (self.total_bytes as isize + byte_delta) as usize;
    }

    pub fn position(&self, utf16_offset: usize) -> Position {
        let utf16_offset = utf16_offset.min(self.total_utf16);
        let next_line = self
            .line_starts_utf16
            .partition_point(|&ls| ls <= utf16_offset);
        let line = next_line.saturating_sub(1);
        let character = utf16_offset - self.line_starts_utf16[line];
        Position {
            line: line as u32,
            character: character as u32,
        }
    }

    pub fn byte_offset_for_utf16(&self, utf16_offset: usize) -> Option<usize> {
        if utf16_offset > self.total_utf16 {
            return None;
        }
        if utf16_offset == self.total_utf16 {
            return Some(self.total_bytes);
        }
        let idx = self
            .checkpoints
            .partition_point(|&(cp_utf16, _)| cp_utf16 <= utf16_offset);
        let (base_utf16, base_byte) = if idx == 0 {
            (0, 0)
        } else {
            self.checkpoints[idx - 1]
        };
        Some(base_byte + (utf16_offset - base_utf16))
    }

    pub fn utf16_offset_for_byte(&self, byte_offset: usize) -> usize {
        if byte_offset >= self.total_bytes {
            return self.total_utf16;
        }
        let idx = self
            .checkpoints
            .partition_point(|&(_, cp_byte)| cp_byte <= byte_offset);
        let (base_utf16, base_byte) = if idx == 0 {
            (0, 0)
        } else {
            self.checkpoints[idx - 1]
        };
        base_utf16 + (byte_offset - base_byte)
    }

    pub fn text_for_utf16_range<'t>(
        &self,
        text: &'t str,
        start: usize,
        end: usize,
    ) -> Option<&'t str> {
        if start > end {
            return None;
        }
        let byte_start = self.byte_offset_for_utf16(start)?;
        let byte_end = self.byte_offset_for_utf16(end)?;
        text.get(byte_start..byte_end)
    }

    pub fn byte_range_for_lsp_range(&self, range: Range) -> Option<(usize, usize)> {
        let start = self.byte_offset_for_lsp_position(range.start)?;
        let end = self.byte_offset_for_lsp_position(range.end)?;
        Some((start.min(end), end.max(start)))
    }

    /// Like [`Self::byte_range_for_lsp_range`] but also returns the UTF-16 offsets,
    /// avoiding a second lookup when both are needed (e.g. for [`Self::apply_edit`]).
    pub fn edit_offsets(&self, range: Range) -> Option<(usize, usize, usize, usize)> {
        let utf16_start =
            self.line_starts_utf16.get(range.start.line as usize)? + range.start.character as usize;
        let utf16_end =
            self.line_starts_utf16.get(range.end.line as usize)? + range.end.character as usize;
        let (utf16_lo, utf16_hi) = if utf16_start <= utf16_end {
            (utf16_start, utf16_end)
        } else {
            (utf16_end, utf16_start)
        };
        let byte_lo = self.byte_offset_for_utf16(utf16_lo)?;
        let byte_hi = self.byte_offset_for_utf16(utf16_hi)?;
        Some((byte_lo, byte_hi, utf16_lo, utf16_hi))
    }

    pub fn byte_offset_for_lsp_position(&self, position: Position) -> Option<usize> {
        let line = position.line as usize;
        if line >= self.line_starts_utf16.len() {
            if line == self.line_starts_utf16.len() && position.character == 0 {
                return Some(self.total_bytes);
            }
            return None;
        }
        let utf16_offset = self.line_starts_utf16[line] + position.character as usize;
        self.byte_offset_for_utf16(utf16_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ascii_offsets() {
        let index = TextIndex::new("hello\nworld");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(5), Position::new(0, 5));
        assert_eq!(index.position(6), Position::new(1, 0));
        assert_eq!(index.position(11), Position::new(1, 5));
    }

    #[test]
    fn maps_utf16_offsets() {
        let index = TextIndex::new("a😀b\nz");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(1), Position::new(0, 1));
        assert_eq!(index.position(3), Position::new(0, 3));
        assert_eq!(index.position(4), Position::new(0, 4));
        assert_eq!(index.position(5), Position::new(1, 0));
    }

    #[test]
    fn maps_lsp_range_to_byte_range() {
        let text = "a😀b";
        let index = TextIndex::new(text);
        assert_eq!(
            index.byte_range_for_lsp_range(Range::new(Position::new(0, 1), Position::new(0, 3))),
            Some((1, 5))
        );
        assert_eq!(index.text_for_utf16_range(text, 1, 3), Some("😀"));
    }

    #[test]
    fn byte_offset_roundtrip() {
        let text = "hello 😀 wörld\nfoo";
        let index = TextIndex::new(text);
        let mut utf16 = 0usize;
        for (byte_off, ch) in text.char_indices() {
            assert_eq!(
                index.byte_offset_for_utf16(utf16),
                Some(byte_off),
                "utf16={utf16} ch={ch:?}"
            );
            assert_eq!(
                index.utf16_offset_for_byte(byte_off),
                utf16,
                "byte={byte_off} ch={ch:?}"
            );
            utf16 += ch.len_utf16();
        }
        assert_eq!(index.byte_offset_for_utf16(utf16), Some(text.len()));
        assert_eq!(index.utf16_offset_for_byte(text.len()), utf16);
    }

    #[test]
    fn ascii_only_has_no_checkpoints() {
        let index = TextIndex::new("hello world");
        assert!(index.checkpoints.is_empty());
    }

    // Helper: apply an edit via apply_edit and verify the result equals TextIndex::new
    // on the post-edit text.
    fn check_apply_edit(before: &str, range: Range, new_text: &str) {
        let mut index = TextIndex::new(before);
        let (byte_start, byte_end, utf16_start, utf16_end) = index.edit_offsets(range).unwrap();

        index.apply_edit(byte_start, byte_end, utf16_start, utf16_end, new_text);

        let mut after = before.to_string();
        after.replace_range(byte_start..byte_end, new_text);
        let expected = TextIndex::new(&after);

        assert_eq!(index, expected, "after text: {after:?}");
    }

    #[test]
    fn apply_edit_ascii_replace() {
        check_apply_edit(
            "hello world",
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "zed",
        );
    }

    #[test]
    fn apply_edit_insert_newline() {
        check_apply_edit(
            "hello world",
            Range::new(Position::new(0, 5), Position::new(0, 5)),
            "\n",
        );
    }

    #[test]
    fn apply_edit_delete_newline() {
        check_apply_edit(
            "hello\nworld",
            Range::new(Position::new(0, 5), Position::new(1, 0)),
            "",
        );
    }

    #[test]
    fn apply_edit_replace_emoji_with_ascii() {
        check_apply_edit(
            "a😀b",
            Range::new(Position::new(0, 1), Position::new(0, 3)),
            "x",
        );
    }

    #[test]
    fn apply_edit_insert_emoji() {
        check_apply_edit(
            "ab",
            Range::new(Position::new(0, 1), Position::new(0, 1)),
            "😀",
        );
    }

    #[test]
    fn apply_edit_multiline_replace() {
        check_apply_edit(
            "line one\nline two\nline three",
            Range::new(Position::new(0, 5), Position::new(2, 4)),
            "X\nY",
        );
    }

    #[test]
    fn maps_crlf_line_endings() {
        // \r\n counts as one line ending; the new line starts after the \n.
        let index = TextIndex::new("hello\r\nworld");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(5), Position::new(0, 5)); // the \r
        assert_eq!(index.position(6), Position::new(0, 6)); // the \n
        assert_eq!(index.position(7), Position::new(1, 0)); // 'w'
        assert_eq!(index.position(12), Position::new(1, 5));
    }

    #[test]
    fn maps_bare_cr_line_endings() {
        let index = TextIndex::new("hello\rworld");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(5), Position::new(0, 5)); // the \r
        assert_eq!(index.position(6), Position::new(1, 0)); // 'w'
    }

    #[test]
    fn apply_edit_crlf_insert() {
        check_apply_edit(
            "hello\r\nworld",
            Range::new(Position::new(0, 5), Position::new(0, 5)),
            " there",
        );
    }

    #[test]
    fn apply_edit_replaces_crlf_with_lf() {
        check_apply_edit(
            "a\r\nb",
            Range::new(Position::new(0, 1), Position::new(1, 0)),
            "\n",
        );
    }
}
