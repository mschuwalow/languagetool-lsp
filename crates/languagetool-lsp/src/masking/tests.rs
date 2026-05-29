use super::*;
use indoc::indoc;

fn check_blocks_for_test(text: &str, language_id: &str) -> Vec<CheckBlock> {
    let language = SupportedLanguage::from_language_id(language_id).unwrap();
    let mask = Masker::new(text, language);
    mask.check_blocks(text)
}

fn all_text(blocks: &[CheckBlock]) -> String {
    blocks
        .iter()
        .flat_map(|b| b.annotated.annotation.iter())
        .filter_map(|a| a.as_text())
        .collect()
}

fn all_markup(blocks: &[CheckBlock]) -> String {
    blocks
        .iter()
        .flat_map(|b| b.annotated.annotation.iter())
        .filter_map(|a| a.as_markup())
        .collect()
}

#[test]
fn rust_annotations_mark_code_as_markup_and_comments_as_text() {
    let text = indoc! {r#"
            let value = 1; // This are a comment.
            let other = "This are code";
            /* This are block docs. */
        "#};

    let blocks = check_blocks_for_test(text, "rust");
    let checked_text: Vec<&str> = blocks
        .iter()
        .flat_map(|b| b.annotated.annotation.iter())
        .filter_map(|a| a.as_text())
        .collect();
    let markup = all_markup(&blocks);

    assert!(
        checked_text.contains(&"This are a comment."),
        "checked_text={checked_text:?}"
    );
    assert!(
        checked_text.contains(&"This are block docs. "),
        "checked_text={checked_text:?}"
    );
    assert!(markup.contains("//"));
    assert!(markup.contains("/*"));
}

#[test]
fn consecutive_line_comments_form_one_block() {
    let text = indoc! {"
            // I am a catz.
            // I like chickz.
        "};
    let blocks = check_blocks_for_test(text, "rust");
    assert_eq!(blocks.len(), 1, "expected one block, got {}", blocks.len());
    let checked_text: Vec<&str> = blocks[0]
        .annotated
        .annotation
        .iter()
        .filter_map(|a| a.as_text())
        .collect();
    assert_eq!(checked_text, vec!["I am a catz.", "I like chickz."]);
    let separators: Vec<&str> = blocks[0]
        .annotated
        .annotation
        .iter()
        .filter_map(|a| a.interpret_as())
        .collect();
    assert!(separators.iter().any(|s| s.contains('\n')));
}

#[test]
fn blank_line_splits_line_comments_into_two_blocks() {
    let text = indoc! {"
            // First block.

            // Second block.
        "};
    let blocks = check_blocks_for_test(text, "rust");
    assert_eq!(blocks.len(), 2, "expected two blocks, got {}", blocks.len());
    let first_text = all_text(&blocks[0..1]);
    let second_text = all_text(&blocks[1..2]);
    assert!(first_text.contains("First block."), "first={first_text:?}");
    assert!(
        second_text.contains("Second block."),
        "second={second_text:?}"
    );
}

#[test]
fn blank_line_splits_standalone_line_comments_into_two_blocks() {
    let text = indoc! {"
            // foo

            // bar
        "};
    let blocks = check_blocks_for_test(text, "rust");

    assert_eq!(blocks.len(), 2, "blocks={blocks:#?}");
    assert_eq!(all_text(&blocks[0..1]), "foo");
    assert_eq!(all_text(&blocks[1..2]), "bar");
}

#[test]
fn rust_doc_comments_and_line_comments_separated_by_blank_line_are_separate_blocks() {
    let text = indoc! {r#"
            fn main() {
                // This are a tset dd in a comment.
                //
                // dds
                //

                /// This is the next block.
                /// This is the next block.
                /// This is the next block.

                // This is the next block
                // This is the next block
                // This is the next block
                // This is the next block
                let _value = "This are not checked in a string yet.";
            }
        "#};
    let blocks = check_blocks_for_test(text, "rust");

    assert_eq!(blocks.len(), 3, "blocks={blocks:#?}");
    assert_eq!(
        all_text(&blocks[0..1]),
        "This are a tset dd in a comment.dds"
    );
    assert_eq!(
        all_text(&blocks[1..2]),
        "This is the next block.This is the next block.This is the next block."
    );
    assert_eq!(
        all_text(&blocks[2..3]),
        "This is the next blockThis is the next blockThis is the next blockThis is the next block"
    );
}

#[test]
fn adjacent_rust_doc_comments_and_line_comments_form_one_block() {
    let text = indoc! {r#"
            /// This is one block.
            /// This is one block.
            // This is one block.
            // This is one block.
        "#};
    let blocks = check_blocks_for_test(text, "rust");

    assert_eq!(blocks.len(), 1, "blocks={blocks:#?}");
    assert_eq!(
        all_text(&blocks),
        "This is one block.This is one block.This is one block.This is one block."
    );
}

#[test]
fn block_comment_is_its_own_block() {
    let text = indoc! {"
            // Line comment.
            /* Block comment. */
            // After block.
        "};
    let blocks = check_blocks_for_test(text, "rust");
    assert_eq!(
        blocks.len(),
        3,
        "expected three blocks, got {}",
        blocks.len()
    );
}

#[test]
fn rust_lifetimes_do_not_hide_following_comments() {
    let text = "let value: &'a str = input; // This are docs.\n";
    let blocks = check_blocks_for_test(text, "rust");
    let checked = all_text(&blocks);
    assert_eq!(checked, "This are docs.");
}

#[test]
fn rust_tree_sitter_checks_doc_comments() {
    let text = indoc! {r#"
            /// This are public docs.
            //! This are module docs.
            /** This are block docs. */
            /*! This are inner block docs. */
            fn main() {}
        "#};
    let blocks = check_blocks_for_test(text, "rust");
    let checked_texts: Vec<&str> = blocks
        .iter()
        .flat_map(|b| b.annotated.annotation.iter())
        .filter_map(|a| a.as_text())
        .collect();

    assert!(
        checked_texts.contains(&"This are public docs."),
        "{checked_texts:?}"
    );
    assert!(
        checked_texts.contains(&"This are module docs."),
        "{checked_texts:?}"
    );
    assert!(
        checked_texts
            .iter()
            .any(|t| t.contains("This are block docs.")),
        "{checked_texts:?}"
    );
    assert!(
        checked_texts
            .iter()
            .any(|t| t.contains("This are inner block docs.")),
        "{checked_texts:?}"
    );
}

#[test]
fn rust_tree_sitter_ignores_comment_markers_inside_strings() {
    let text = indoc! {r##"
            let normal = "// This are code";
            let raw = r#"/* This are raw string code */"#;
            let ch = '/';
            // This are a real comment.
        "##};
    let blocks = check_blocks_for_test(text, "rust");
    let checked = all_text(&blocks);

    assert!(!checked.contains("This are code"));
    assert!(!checked.contains("This are raw string code"));
    assert!(checked.contains("This are a real comment."));
}

#[test]
fn rust_tree_sitter_ignored_ranges_keep_only_comments() {
    let text = indoc! {r##"
            let code = "This are code";
            // This are a comment.
        "##};
    let blocks = check_blocks_for_test(text, "rust");
    let checked = all_text(&blocks);
    assert_eq!(checked, "This are a comment.");
}

#[test]
fn comment_tree_masking_supports_explicit_languages() {
    let cases = [
        (
            "scala",
            indoc! {r#"
                    val code = "// This are code"
                    // This are a Scala comment.
                "#},
            "This are a Scala comment.",
            "This are code",
        ),
        (
            "nix",
            indoc! {r##"
                    let code = "# This are code";
                    # This are a Nix comment.
                "##},
            "This are a Nix comment.",
            "This are code",
        ),
        (
            "java",
            indoc! {r#"
                    class T { String code = "// This are code"; }
                    // This are a Java comment.
                "#},
            "This are a Java comment.",
            "This are code",
        ),
        (
            "python",
            indoc! {r##"
                    code = "# This are code"
                    # This are a Python comment.
                "##},
            "This are a Python comment.",
            "This are code",
        ),
        (
            "javascript",
            indoc! {r#"
                    const code = "// This are code";
                    // This are a JavaScript comment.
                "#},
            "This are a JavaScript comment.",
            "This are code",
        ),
        (
            "typescript",
            indoc! {r#"
                    const code: string = "// This are code";
                    // This are a TypeScript comment.
                "#},
            "This are a TypeScript comment.",
            "This are code",
        ),
        (
            "tsx",
            indoc! {r#"
                    const code: string = "// This are code";
                    // This are a TSX comment.
                "#},
            "This are a TSX comment.",
            "This are code",
        ),
    ];

    for (language, text, expected, unexpected) in cases {
        let blocks = check_blocks_for_test(text, language);
        let checked = all_text(&blocks);

        assert!(
            checked.contains(expected),
            "language={language} checked={checked:?}"
        );
        assert!(
            !checked.contains(unexpected),
            "language={language} checked={checked:?}"
        );
        assert!(
            !checked.contains("// This are") && !checked.contains("# This are"),
            "language={language} checked={checked:?}"
        );
    }
}

#[test]
fn language_specific_comment_strippers_remove_block_markers() {
    let cases = [
        (
            "rust",
            "/* This are Rust block docs. */",
            "This are Rust block docs. ",
        ),
        ("java", "/** This are Java docs. */", "This are Java docs. "),
        (
            "scala",
            "/* This are Scala docs. */",
            "This are Scala docs. ",
        ),
        (
            "javascript",
            "/* This are JS docs. */",
            "This are JS docs. ",
        ),
        (
            "typescript",
            "/* This are TS docs. */",
            "This are TS docs. ",
        ),
    ];

    for (language, text, expected) in cases {
        let blocks = check_blocks_for_test(text, language);
        let checked = all_text(&blocks);
        assert_eq!(checked, expected, "language={language}");
    }
}

#[test]
fn plaintext_produces_one_block() {
    let text = "Hello world. This are a test.";
    let blocks = check_blocks_for_test(text, "plaintext");
    assert_eq!(blocks.len(), 1);
    assert_eq!(all_text(&blocks), text);
}

#[test]
fn markdown_tree_sitter_skips_fenced_code() {
    let text = indoc! {"
            This are prose.
            ```rust
            This are code.
            ```
            More are prose.
        "};

    let blocks = check_blocks_for_test(text, "markdown");
    let checked = all_text(&blocks);
    let markup = all_markup(&blocks);

    assert!(checked.contains("This are prose."));
    assert!(checked.contains("More are prose."));
    assert!(!checked.contains("This are code."));
    assert!(markup.contains("This are code."));
}

#[test]
fn python_hash_markers_inside_strings_are_not_comments() {
    let text = indoc! {r##"
            value = "# This are code"
            # This are a comment.
        "##};
    let blocks = check_blocks_for_test(text, "python");
    let checked = all_text(&blocks);

    assert!(!checked.contains("This are code"));
    assert!(checked.contains("This are a comment."));
}

#[test]
fn html_masks_script_contents_and_tags() {
    let text = indoc! {"
            <p>This are a tset.</p>
            <script>This are code and should not be checked.</script>
        "};
    let blocks = check_blocks_for_test(text, "html");
    let checked = all_text(&blocks);
    let markup = all_markup(&blocks);

    assert!(checked.contains("This are a tset."));
    assert!(!checked.contains("This are code"));
    assert!(!checked.contains("<p>"));
    assert!(markup.contains("<script>This are code and should not be checked.</script>"));
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

    let blocks = check_blocks_for_test(text, "markdown");
    let checked = all_text(&blocks);
    let markup = all_markup(&blocks);

    assert!(checked.contains("This are prose."));
    assert!(checked.contains("More prose."));
    assert!(!checked.contains("This are code"));
    assert!(markup.contains("This are code"));
}

#[test]
fn markdown_annotations_keep_context_around_inline_code() {
    let text = "Use `This are inline code` carefully.";
    let blocks = check_blocks_for_test(text, "markdown");
    let checked = all_text(&blocks);
    let markup = all_markup(&blocks);

    assert_eq!(checked, "Use  carefully.");
    assert_eq!(markup, "`This are inline code`");
}

#[test]
fn block_byte_ranges_cover_comment_nodes() {
    let text = "let x = 1; // First.\n// Second.\n";
    let blocks = check_blocks_for_test(text, "rust");
    assert_eq!(blocks.len(), 2);
    let first = &text[blocks[0].byte_range.start.0..blocks[0].byte_range.end.0];
    let second = &text[blocks[1].byte_range.start.0..blocks[1].byte_range.end.0];
    assert!(first.contains("First."), "first={first:?}");
    assert!(second.contains("Second."), "second={second:?}");
}

#[test]
fn consecutive_standalone_line_comments_form_one_block_after_code() {
    let text = "let x = 1;\n// First.\n// Second.\n";
    let blocks = check_blocks_for_test(text, "rust");
    assert_eq!(blocks.len(), 1);
    let block = &text[blocks[0].byte_range.start.0..blocks[0].byte_range.end.0];

    assert!(block.contains("First."), "block={block:?}");
    assert!(block.contains("Second."), "block={block:?}");
}

#[test]
fn byte_ranges_for_non_ascii_comments() {
    // Ensure byte offsets are correct when non-ASCII chars precede comments.
    let text = "let x = \"café\"; // Ünïcödé comment.\n";
    let blocks = check_blocks_for_test(text, "rust");
    assert_eq!(blocks.len(), 1);
    let block = &blocks[0];
    // The text annotation inside the block should yield the comment content
    // at the right byte offset.
    let content_text: String = block
        .annotated
        .annotation
        .iter()
        .filter_map(|a| a.as_text())
        .collect();
    assert!(
        content_text.contains("Ünïcödé comment."),
        "content={content_text:?}"
    );
    // Verify that the byte_range actually indexes back correctly.
    let block_slice = &text[block.byte_range.start.0..block.byte_range.end.0];
    assert!(
        block_slice.contains("Ünïcödé comment."),
        "block_slice={block_slice:?}"
    );
}

#[test]
fn crlf_line_comments_follow_same_grouping_rules() {
    let adjacent = "// foo\r\n// bar\r\n";
    let adjacent_blocks = check_blocks_for_test(adjacent, "rust");
    assert_eq!(adjacent_blocks.len(), 1, "blocks={adjacent_blocks:#?}");
    assert_eq!(all_text(&adjacent_blocks), "foobar");

    let separated = "// foo\r\n\r\n// bar\r\n";
    let separated_blocks = check_blocks_for_test(separated, "rust");
    assert_eq!(separated_blocks.len(), 2, "blocks={separated_blocks:#?}");
    assert_eq!(all_text(&separated_blocks[0..1]), "foo");
    assert_eq!(all_text(&separated_blocks[1..2]), "bar");
}

#[test]
fn javascript_masks_strings_and_checks_line_and_block_comments() {
    let text = indoc! {r#"
            const value = "/* This are string */";
            /* This are block comment. */
            // This are line comment.
        "#};
    let blocks = check_blocks_for_test(text, "javascript");
    let checked = all_text(&blocks);

    assert!(!checked.contains("This are string"), "checked={checked:?}");
    assert!(
        checked.contains("This are block comment."),
        "checked={checked:?}"
    );
    assert!(
        checked.contains("This are line comment."),
        "checked={checked:?}"
    );
}

#[test]
fn python_masks_hashes_inside_triple_quoted_strings() {
    let text = indoc! {r##"
            value = """# This are string"""
            # This are comment.
        "##};
    let blocks = check_blocks_for_test(text, "python");
    let checked = all_text(&blocks);

    assert!(!checked.contains("This are string"), "checked={checked:?}");
    assert!(checked.contains("This are comment."), "checked={checked:?}");
}

#[test]
fn markdown_masks_link_destinations_but_checks_link_text() {
    let text = "Read [This are link text](https://example.com/This_are_url) now.";
    let blocks = check_blocks_for_test(text, "markdown");
    let checked = all_text(&blocks);
    let markup = all_markup(&blocks);

    assert!(
        checked.contains("This are link text"),
        "checked={checked:?}"
    );
    assert!(!checked.contains("This_are_url"), "checked={checked:?}");
    assert!(markup.contains("This_are_url"), "markup={markup:?}");
}

#[test]
fn html_masks_style_contents() {
    let text = indoc! {r#"
            <style>.bad::before { content: "This are style code"; }</style>
            <p>This are prose.</p>
        "#};
    let blocks = check_blocks_for_test(text, "html");
    let checked = all_text(&blocks);
    let markup = all_markup(&blocks);

    assert!(checked.contains("This are prose."), "checked={checked:?}");
    assert!(
        !checked.contains("This are style code"),
        "checked={checked:?}"
    );
    assert!(markup.contains("This are style code"), "markup={markup:?}");
}
