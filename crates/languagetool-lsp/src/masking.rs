use crate::languagetool::{AnnotatedText, Annotation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: usize,
    end: usize,
}

pub fn annotated_for_language(text: &str, language_id: Option<&str>) -> AnnotatedText {
    let annotation = match classify_language(language_id) {
        LanguageKind::Markdown => markdown_annotations(text),
        LanguageKind::Html => html_annotations(text),
        LanguageKind::CLike => c_like_annotations(text),
        LanguageKind::HashComment => hash_annotations(text),
        LanguageKind::PlainText => vec![Annotation::text(text.to_string())],
    };

    AnnotatedText { annotation }
}

pub fn ignored_ranges_for_language(text: &str, language_id: Option<&str>) -> Vec<(usize, usize)> {
    match classify_language(language_id) {
        LanguageKind::Markdown => {
            let mut ranges = Vec::new();
            collect_markdown_front_matter(text, &mut ranges);
            collect_markdown_fenced_code(text, &mut ranges);
            collect_inline_code(text, &mut ranges);
            collect_link_destinations(text, &mut ranges);
            ranges_as_utf16(text, &mut ranges)
        }
        LanguageKind::Html => {
            let mut ranges = Vec::new();
            collect_html_tags(text, &mut ranges);
            collect_html_element_contents(text, "script", &mut ranges);
            collect_html_element_contents(text, "style", &mut ranges);
            ranges_as_utf16(text, &mut ranges)
        }
        LanguageKind::CLike => {
            let mut keep_ranges = Vec::new();
            collect_c_like_comment_ranges(text, &mut keep_ranges);
            inverse_ranges_as_utf16(text, &mut keep_ranges)
        }
        LanguageKind::HashComment => {
            let mut keep_ranges = Vec::new();
            collect_hash_comment_ranges(text, &mut keep_ranges);
            collect_python_triple_quotes(text, &mut keep_ranges);
            inverse_ranges_as_utf16(text, &mut keep_ranges)
        }
        LanguageKind::PlainText => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanguageKind {
    Markdown,
    Html,
    CLike,
    HashComment,
    PlainText,
}

fn classify_language(language_id: Option<&str>) -> LanguageKind {
    match language_id.unwrap_or_default() {
        "markdown" | "md" | "mdx" => LanguageKind::Markdown,
        "html" => LanguageKind::Html,
        "c" | "cpp" | "c++" | "css" | "go" | "java" | "javascript" | "javascriptreact" | "jsx"
        | "typescript" | "typescriptreact" | "tsx" | "rust" | "rs" => LanguageKind::CLike,
        "python" | "shellscript" | "shell" | "bash" | "ruby" | "toml" | "yaml" => {
            LanguageKind::HashComment
        }
        "plaintext" => LanguageKind::PlainText,

        _ => LanguageKind::PlainText,
    }
}

fn markdown_annotations(text: &str) -> Vec<Annotation> {
    let mut skip_ranges = Vec::new();
    collect_markdown_front_matter(text, &mut skip_ranges);
    collect_markdown_fenced_code(text, &mut skip_ranges);
    collect_inline_code(text, &mut skip_ranges);
    collect_link_destinations(text, &mut skip_ranges);
    annotations_from_skip_ranges(text, &mut skip_ranges)
}

fn html_annotations(text: &str) -> Vec<Annotation> {
    let mut skip_ranges = Vec::new();
    collect_html_tags(text, &mut skip_ranges);
    collect_html_element_contents(text, "script", &mut skip_ranges);
    collect_html_element_contents(text, "style", &mut skip_ranges);
    annotations_from_skip_ranges(text, &mut skip_ranges)
}

fn c_like_annotations(text: &str) -> Vec<Annotation> {
    let mut keep_ranges = Vec::new();
    collect_c_like_comment_ranges(text, &mut keep_ranges);
    annotations_from_keep_ranges(text, &mut keep_ranges)
}

fn hash_annotations(text: &str) -> Vec<Annotation> {
    let mut keep_ranges = Vec::new();
    collect_hash_comment_ranges(text, &mut keep_ranges);
    collect_python_triple_quotes(text, &mut keep_ranges);
    annotations_from_keep_ranges(text, &mut keep_ranges)
}

fn collect_c_like_comment_ranges(text: &str, keep_ranges: &mut Vec<Range>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if let Some(end) = skip_raw_rust_string(bytes, i) {
            i = end;
            continue;
        }

        match (bytes[i], bytes[i + 1]) {
            (b'"' | b'\'', _) => {
                i = skip_quoted_string(bytes, i);
            }
            (b'/', b'/') => {
                let start = skip_horizontal_whitespace(bytes, i + 2);
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'\n' {
                    end += 1;
                }
                keep_ranges.push(Range { start, end });
                i = end;
            }
            (b'/', b'*') => {
                let start = skip_horizontal_whitespace(bytes, i + 2);
                let mut end = start;
                while end + 1 < bytes.len() && !(bytes[end] == b'*' && bytes[end + 1] == b'/') {
                    end += 1;
                }
                keep_ranges.push(Range { start, end });
                i = (end + 2).min(bytes.len());
            }
            _ => i += 1,
        }
    }
}

fn skip_quoted_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_raw_rust_string(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'r') {
        return None;
    }

    let mut quote_index = start + 1;
    while bytes.get(quote_index) == Some(&b'#') {
        quote_index += 1;
    }
    if bytes.get(quote_index) != Some(&b'"') {
        return None;
    }

    let hashes = quote_index - start - 1;
    let mut i = quote_index + 1;
    while i < bytes.len() {
        if bytes[i] == b'"'
            && i + 1 + hashes <= bytes.len()
            && bytes[i + 1..i + 1 + hashes]
                .iter()
                .all(|byte| *byte == b'#')
        {
            return Some(i + 1 + hashes);
        }
        i += 1;
    }

    Some(bytes.len())
}

fn collect_hash_comment_ranges(text: &str, keep_ranges: &mut Vec<Range>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let start = skip_horizontal_whitespace(bytes, i + 1);
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            keep_ranges.push(Range { start, end });
            i = end;
        } else {
            i += 1;
        }
    }
}

fn skip_horizontal_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t') {
        index += 1;
    }
    index
}

fn collect_python_triple_quotes(text: &str, ranges: &mut Vec<Range>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        let quote = if bytes[i..].starts_with(b"'''") {
            Some(b"'''")
        } else if bytes[i..].starts_with(b"\"\"\"") {
            Some(b"\"\"\"")
        } else {
            None
        };

        let Some(quote) = quote else {
            i += 1;
            continue;
        };

        let start = i + 3;
        let mut end = start;
        while end + 2 < bytes.len() && !bytes[end..].starts_with(quote) {
            end += 1;
        }
        ranges.push(Range { start, end });
        i = (end + 3).min(bytes.len());
    }
}

fn collect_markdown_front_matter(text: &str, ranges: &mut Vec<Range>) {
    if !text.starts_with("---\n") && !text.starts_with("---\r\n") {
        return;
    }

    let mut offset = 0;
    for (idx, line) in text.split_inclusive('\n').enumerate() {
        let line_start = offset;
        offset += line.len();
        if idx == 0 {
            continue;
        }
        if line.trim() == "---" {
            ranges.push(Range {
                start: 0,
                end: line_start + line.len(),
            });
            return;
        }
    }
}

fn collect_markdown_fenced_code(text: &str, ranges: &mut Vec<Range>) {
    let mut in_fence = false;
    let mut fence_start = 0;
    let mut offset = 0;

    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if in_fence {
                ranges.push(Range {
                    start: fence_start,
                    end: offset,
                });
                in_fence = false;
            } else {
                fence_start = line_start;
                in_fence = true;
            }
        }
    }

    if in_fence {
        ranges.push(Range {
            start: fence_start,
            end: text.len(),
        });
    }
}

fn collect_inline_code(text: &str, ranges: &mut Vec<Range>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }

        let start = i;
        let mut ticks = 0;
        while i < bytes.len() && bytes[i] == b'`' {
            ticks += 1;
            i += 1;
        }

        let mut j = i;
        while j + ticks <= bytes.len() {
            if bytes[j] == b'\n' {
                break;
            }
            if bytes[j..].starts_with(&vec![b'`'; ticks]) {
                ranges.push(Range {
                    start,
                    end: j + ticks,
                });
                i = j + ticks;
                break;
            }
            j += 1;
        }
    }
}

fn collect_link_destinations(text: &str, ranges: &mut Vec<Range>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 1;
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b')' && bytes[j] != b'\n' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b')' {
                ranges.push(Range { start, end: j + 1 });
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

fn collect_html_tags(text: &str, ranges: &mut Vec<Range>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        let mut quote = None;
        while i < bytes.len() {
            match (quote, bytes[i]) {
                (Some(q), b) if b == q => quote = None,
                (None, b'\'' | b'"') => quote = Some(bytes[i]),
                (None, b'>') => {
                    ranges.push(Range { start, end: i + 1 });
                    i += 1;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn collect_html_element_contents(text: &str, tag: &str, ranges: &mut Vec<Range>) {
    let lower = text.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut search_start = 0;

    while let Some(open_rel) = lower[search_start..].find(&open) {
        let open_start = search_start + open_rel;
        let Some(open_end_rel) = lower[open_start..].find('>') else {
            break;
        };
        let content_start = open_start + open_end_rel + 1;
        let Some(close_rel) = lower[content_start..].find(&close) else {
            ranges.push(Range {
                start: content_start,
                end: text.len(),
            });
            break;
        };
        let content_end = content_start + close_rel;
        ranges.push(Range {
            start: content_start,
            end: content_end,
        });
        search_start = content_end + close.len();
    }
}

fn annotations_from_skip_ranges(text: &str, skip_ranges: &mut [Range]) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut cursor = 0;
    for range in merge_ranges(skip_ranges) {
        push_annotation_text(text, cursor, range.start, &mut annotations);
        push_annotation_markup(text, range.start, range.end, &mut annotations);
        cursor = cursor.max(range.end);
    }
    push_annotation_text(text, cursor, text.len(), &mut annotations);
    annotations
}

fn annotations_from_keep_ranges(text: &str, keep_ranges: &mut [Range]) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut cursor = 0;
    for range in merge_ranges(keep_ranges) {
        push_annotation_markup(text, cursor, range.start, &mut annotations);
        push_annotation_text(text, range.start, range.end, &mut annotations);
        cursor = cursor.max(range.end);
    }
    push_annotation_markup(text, cursor, text.len(), &mut annotations);
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

fn merge_ranges(ranges: &mut [Range]) -> Vec<Range> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range> = Vec::new();
    for range in ranges
        .iter()
        .copied()
        .filter(|range| range.start < range.end)
    {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

fn ranges_as_utf16(text: &str, ranges: &mut [Range]) -> Vec<(usize, usize)> {
    merge_ranges(ranges)
        .into_iter()
        .map(|range| {
            (
                utf16_offset_for_byte(text, range.start),
                utf16_offset_for_byte(text, range.end),
            )
        })
        .collect()
}

fn inverse_ranges_as_utf16(text: &str, keep_ranges: &mut [Range]) -> Vec<(usize, usize)> {
    let mut ignored = Vec::new();
    let mut cursor = 0;
    for range in merge_ranges(keep_ranges) {
        if cursor < range.start {
            ignored.push(Range {
                start: cursor,
                end: range.start,
            });
        }
        cursor = cursor.max(range.end);
    }
    if cursor < text.len() {
        ignored.push(Range {
            start: cursor,
            end: text.len(),
        });
    }
    ranges_as_utf16(text, &mut ignored)
}

fn utf16_offset_for_byte(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset].chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn c_like_annotations_mark_code_as_markup_and_comments_as_text() {
        let text = indoc! {r#"
            let value = 1; // This are a comment.
            let other = "This are code";
            /* This are block docs. */
        "#};

        let data = annotated_for_language(text, Some("rust"));
        let checked_text = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<Vec<_>>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert_eq!(
            checked_text,
            vec!["This are a comment.", "This are block docs. "]
        );
        assert!(markup.contains("let value = 1; //"));
        assert!(markup.contains("This are code"));
    }

    #[test]
    fn line_comments_are_separated_by_newline_interpretation() {
        let text = indoc! {"
            // I am a catz.
            // I like chickz.
        "};
        let data = annotated_for_language(text, Some("rust"));
        let checked_text = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<Vec<_>>();
        let separators = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.interpret_as())
            .collect::<Vec<_>>();

        assert_eq!(checked_text, vec!["I am a catz.", "I like chickz."]);
        assert!(separators.iter().any(|separator| separator.contains('\n')));
    }

    #[test]
    fn markdown_annotations_mark_skipped_areas_as_markup() {
        let text = indoc! {"
            This are prose.
            ```rust
            This are code.
            ```
            More prose.
        "};

        let data = annotated_for_language(text, Some("markdown"));
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert!(checked.contains("This are prose."));
        assert!(checked.contains("More prose."));
        assert!(!checked.contains("This are code"));
        assert!(markup.contains("This are code"));
    }

    #[test]
    fn markdown_annotations_keep_context_around_inline_code() {
        let text = "Use `This are inline code` carefully.";
        let data = annotated_for_language(text, Some("markdown"));
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert_eq!(checked, "Use  carefully.");
        assert_eq!(markup, "`This are inline code`");
    }
}
