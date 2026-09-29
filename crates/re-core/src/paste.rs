use anyhow::{Result, bail};
use ropey::Rope;

pub struct PasteResult {
    pub rope: Rope,
    pub cursor_char: usize,
    pub line_count_changed: bool,
}

pub fn apply(
    mut rope: Rope,
    start_char: usize,
    end_char: usize,
    pasted_text: &str,
) -> Result<PasteResult> {
    if start_char > end_char || end_char > rope.len_chars() {
        bail!("the paste selection is no longer valid");
    }

    let original_line_count = rope.len_lines();
    let mut removed_and_suffix = rope.split_off(start_char);
    let suffix = removed_and_suffix.split_off(end_char - start_char);
    let inserted = Rope::from_str(pasted_text);
    let inserted_chars = inserted.len_chars();

    rope.append(inserted);
    rope.append(suffix);

    Ok(PasteResult {
        line_count_changed: rope.len_lines() != original_line_count,
        rope,
        cursor_char: start_char + inserted_chars,
    })
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use super::apply;

    #[test]
    fn multiline_paste_preserves_newlines() {
        let result = apply(Rope::from_str("before after"), 7, 7, "one\ntwo\n")
            .expect("paste should succeed");

        assert_eq!(result.rope.to_string(), "before one\ntwo\nafter");
        assert_eq!(result.cursor_char, 15);
        assert!(result.line_count_changed);
    }

    #[test]
    fn large_paste_is_stored_in_rope_chunks() {
        let pasted = "x".repeat(2_000_000);
        let result =
            apply(Rope::from_str("start:end"), 6, 6, &pasted).expect("paste should succeed");

        assert_eq!(result.rope.len_chars(), 2_000_009);
        assert_eq!(result.cursor_char, 2_000_006);
        assert!(result.rope.chunks().count() > 1);
        assert!(!result.line_count_changed);
    }

    #[test]
    fn rope_splice_replaces_selection_and_uses_unicode_character_counts() {
        let result =
            apply(Rope::from_str("left OLD right"), 5, 8, "日é").expect("paste should succeed");

        assert_eq!(result.rope.to_string(), "left 日é right");
        assert_eq!(result.cursor_char, 7);
        assert!(!result.line_count_changed);
    }
}
