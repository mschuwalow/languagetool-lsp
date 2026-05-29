use super::CheckBlock;
use crate::languagetool::{AnnotatedText, Annotation};
use crate::text_index::ByteRange;

/// A normalized comment node extracted from a syntax tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentBlock {
    /// Full byte span of the comment node, including `//`, `/*`, `*/`, etc.
    pub full_range: ByteRange,
    /// Byte span of the comment content after stripping markers and surrounding whitespace.
    pub content_range: ByteRange,
    /// True if the comment starts with a block-comment marker.
    pub is_block: bool,
    /// True if only horizontal whitespace precedes the comment on its line.
    pub is_standalone_line: bool,
}

pub fn merge_comment_blocks(text: &str, comments: &[CommentBlock]) -> Vec<CheckBlock> {
    if comments.is_empty() {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut group_start = 0usize;

    for i in 1..comments.len() {
        let prev = &comments[i - 1];
        let curr = &comments[i];

        if comments_split_block(text, prev, curr) {
            push_check_block(text, &comments[group_start..i], &mut blocks);
            group_start = i;
        }
    }
    push_check_block(text, &comments[group_start..], &mut blocks);

    blocks
}

fn comments_split_block(text: &str, prev: &CommentBlock, curr: &CommentBlock) -> bool {
    prev.is_block
        || curr.is_block
        || !prev.is_standalone_line
        || comment_gap_splits_block(text, prev.full_range.end.0, curr.full_range.start.0)
}

/// Returns true if the comment gap contains a physical blank line or any
/// non-whitespace character. Some grammars include the trailing line ending in
/// line-comment nodes, so include it when checking for blank-line separators.
fn comment_gap_splits_block(text: &str, prev_end: usize, next_start: usize) -> bool {
    let prev_end = include_trailing_line_ending(text, prev_end);
    let gap = match text.get(prev_end..next_start) {
        Some(g) => g,
        None => return true,
    };
    let bytes = gap.as_bytes();
    if bytes
        .iter()
        .any(|b| !matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
    {
        return true;
    }
    gap_contains_blank_line(gap)
}

fn gap_contains_blank_line(gap: &str) -> bool {
    let mut saw_newline = false;
    for &b in gap.as_bytes() {
        match b {
            b'\n' => {
                if saw_newline {
                    return true;
                }
                saw_newline = true;
            }
            b'\r' => {}
            b' ' | b'\t' => {}
            _ => saw_newline = false,
        }
    }
    false
}

fn include_trailing_line_ending(text: &str, end: usize) -> usize {
    let bytes = text.as_bytes();
    if end == 0 || bytes.get(end - 1) != Some(&b'\n') {
        return end;
    }
    if end >= 2 && bytes.get(end - 2) == Some(&b'\r') {
        end - 2
    } else {
        end - 1
    }
}

fn push_check_block(text: &str, group: &[CommentBlock], blocks: &mut Vec<CheckBlock>) {
    if group.is_empty() {
        return;
    }

    let block_start = group[0].full_range.start.0;
    let block_end = group[group.len() - 1].full_range.end.0;
    let annotation = annotations_for_comment_group(text, group, block_start, block_end);
    let annotated = AnnotatedText { annotation };
    if !annotated.has_text() {
        return;
    }

    blocks.push(CheckBlock {
        byte_range: ByteRange::new(block_start, block_end),
        annotated,
    });
}

fn annotations_for_comment_group(
    text: &str,
    group: &[CommentBlock],
    block_start: usize,
    block_end: usize,
) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut cursor = block_start;

    for comment in group {
        let content_start = comment.content_range.start.0;
        let content_end = comment.content_range.end.0;
        push_annotation_markup(text, cursor, content_start, &mut annotations);
        push_annotation_text(text, content_start, content_end, &mut annotations);
        cursor = content_end;
    }
    push_annotation_markup(text, cursor, block_end, &mut annotations);
    annotations
}

fn push_annotation_text(text: &str, start: usize, end: usize, annotations: &mut Vec<Annotation>) {
    if start >= end {
        return;
    }
    if let Some(segment) = text.get(start..end) {
        if !segment.is_empty() {
            annotations.push(Annotation::text(segment.to_string()));
        }
    }
}

fn push_annotation_markup(text: &str, start: usize, end: usize, annotations: &mut Vec<Annotation>) {
    if start >= end {
        return;
    }
    if let Some(segment) = text.get(start..end) {
        if !segment.is_empty() {
            annotations.push(Annotation::markup(
                segment.to_string(),
                interpret_as_for_markup(segment),
            ));
        }
    }
}

fn interpret_as_for_markup(markup: &str) -> Option<String> {
    let line_breaks = markup
        .chars()
        .filter(|ch| matches!(ch, '\n' | '\r'))
        .collect::<String>();
    if !line_breaks.is_empty() {
        Some(line_breaks)
    } else if markup.chars().any(char::is_whitespace) {
        Some(" ".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(
        full_range: std::ops::Range<usize>,
        content_range: std::ops::Range<usize>,
        is_block: bool,
        is_standalone_line: bool,
    ) -> CommentBlock {
        CommentBlock {
            full_range: full_range.into(),
            content_range: content_range.into(),
            is_block,
            is_standalone_line,
        }
    }

    fn all_text(blocks: &[CheckBlock]) -> String {
        blocks
            .iter()
            .flat_map(|block| block.annotated.annotation.iter())
            .filter_map(|annotation| annotation.as_text())
            .collect()
    }

    #[test]
    fn adjacent_standalone_line_comments_form_one_block() {
        let text = "// foo\n// bar\n";
        let blocks = merge_comment_blocks(
            text,
            &[
                comment(0..7, 3..6, false, true),
                comment(7..14, 10..13, false, true),
            ],
        );

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].byte_range, ByteRange::new(0, 14));
        assert_eq!(all_text(&blocks), "foobar");
    }

    #[test]
    fn blank_line_between_standalone_line_comments_splits_blocks() {
        let text = "// foo\n\n// bar\n";
        let blocks = merge_comment_blocks(
            text,
            &[
                comment(0..7, 3..6, false, true),
                comment(8..15, 11..14, false, true),
            ],
        );

        assert_eq!(blocks.len(), 2);
        assert_eq!(all_text(&blocks[0..1]), "foo");
        assert_eq!(all_text(&blocks[1..2]), "bar");
    }

    #[test]
    fn inline_comment_followed_by_standalone_comment_splits_blocks() {
        let text = "let x = 1; // foo\n// bar\n";
        let blocks = merge_comment_blocks(
            text,
            &[
                comment(11..18, 14..17, false, false),
                comment(18..25, 21..24, false, true),
            ],
        );

        assert_eq!(blocks.len(), 2);
        assert_eq!(all_text(&blocks[0..1]), "foo");
        assert_eq!(all_text(&blocks[1..2]), "bar");
    }

    #[test]
    fn standalone_comments_after_code_line_still_form_one_block() {
        let text = "let x = 1;\n// foo\n// bar\n";
        let blocks = merge_comment_blocks(
            text,
            &[
                comment(11..18, 14..17, false, true),
                comment(18..25, 21..24, false, true),
            ],
        );

        assert_eq!(blocks.len(), 1);
        assert_eq!(all_text(&blocks), "foobar");
    }

    #[test]
    fn non_whitespace_between_comments_splits_blocks() {
        let text = "// foo\nlet x = 1;\n// bar\n";
        let blocks = merge_comment_blocks(
            text,
            &[
                comment(0..7, 3..6, false, true),
                comment(18..25, 21..24, false, true),
            ],
        );

        assert_eq!(blocks.len(), 2);
        assert_eq!(all_text(&blocks[0..1]), "foo");
        assert_eq!(all_text(&blocks[1..2]), "bar");
    }

    #[test]
    fn block_comments_always_split_blocks() {
        let text = "// foo\n/* bar */\n// baz\n";
        let blocks = merge_comment_blocks(
            text,
            &[
                comment(0..7, 3..6, false, true),
                comment(7..17, 10..13, true, true),
                comment(17..24, 20..23, false, true),
            ],
        );

        assert_eq!(blocks.len(), 3);
        assert_eq!(all_text(&blocks[0..1]), "foo");
        assert_eq!(all_text(&blocks[1..2]), "bar");
        assert_eq!(all_text(&blocks[2..3]), "baz");
    }
}
