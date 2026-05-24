use tower_lsp::lsp_types::{Position, Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    line_starts_utf16: Vec<usize>,
    total_utf16: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts_utf16 = vec![0];
        let mut offset = 0;

        for ch in text.chars() {
            offset += ch.len_utf16();
            if ch == '\n' {
                line_starts_utf16.push(offset);
            }
        }

        Self {
            line_starts_utf16,
            total_utf16: offset,
        }
    }

    pub fn position(&self, utf16_offset: usize) -> Position {
        let utf16_offset = utf16_offset.min(self.total_utf16);
        let next_line = self
            .line_starts_utf16
            .partition_point(|line_start| *line_start <= utf16_offset);
        let line = next_line.saturating_sub(1);
        let character = utf16_offset - self.line_starts_utf16[line];

        Position {
            line: line as u32,
            character: character as u32,
        }
    }
}

pub fn utf16_offset_for_byte(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset].chars().map(char::len_utf16).sum()
}

pub fn byte_range_for_lsp_range(text: &str, range: Range) -> Option<(usize, usize)> {
    let start = byte_offset_for_position(text, range.start)?;
    let end = byte_offset_for_position(text, range.end)?;
    Some((start.min(end), end.max(start)))
}

pub fn byte_offset_for_position(text: &str, position: Position) -> Option<usize> {
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

pub fn text_for_lsp_range(text: &str, range: Range) -> String {
    let Some((start, end)) = byte_range_for_lsp_range(text, range) else {
        return String::new();
    };
    text[start..end].to_string()
}

pub fn text_for_utf16_range(text: &str, start: usize, end: usize) -> String {
    let mut offset = 0;
    let mut output = String::new();

    for ch in text.chars() {
        let next = offset + ch.len_utf16();
        if offset >= start && next <= end {
            output.push(ch);
        }
        offset = next;
        if offset >= end {
            break;
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ascii_offsets() {
        let index = LineIndex::new("hello\nworld");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(5), Position::new(0, 5));
        assert_eq!(index.position(6), Position::new(1, 0));
        assert_eq!(index.position(11), Position::new(1, 5));
    }

    #[test]
    fn maps_utf16_offsets() {
        let index = LineIndex::new("a😀b\nz");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(1), Position::new(0, 1));
        assert_eq!(index.position(3), Position::new(0, 3));
        assert_eq!(index.position(4), Position::new(0, 4));
        assert_eq!(index.position(5), Position::new(1, 0));
    }

    #[test]
    fn maps_lsp_range_to_byte_range() {
        let text = "a😀b";
        assert_eq!(
            byte_range_for_lsp_range(text, Range::new(Position::new(0, 1), Position::new(0, 3))),
            Some((1, 5))
        );
        assert_eq!(
            text_for_lsp_range(text, Range::new(Position::new(0, 1), Position::new(0, 3))),
            "😀"
        );
    }

    #[test]
    fn extracts_utf16_ranges() {
        assert_eq!(text_for_utf16_range("a😀b", 1, 3), "😀");
    }
}
