//! Sanitising untrusted text before printing it (spec §13, threat T8).

/// Longest sanitised string, in characters.
pub const MAX_CHARS: usize = 4096;

/// Removes control characters (C0, DEL, C1, so ESC too) except `\n` and `\t`,
/// and caps the length.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_CHARS));
    for (i, c) in s
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .enumerate()
    {
        if i == MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// [`clean`] that also flattens newlines and tabs, for single-line fields.
pub fn clean_line(s: &str) -> String {
    clean(s).replace(['\n', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes_and_controls_but_keeps_newlines_and_tabs() {
        assert_eq!(clean("a\x1b[31mred\x1b[0m\x07\u{9b}2J\n\tb\x7f"), "a[31mred[0m2J\n\tb");
        assert_eq!(clean_line("a\nb\tc"), "a b c");
    }

    #[test]
    fn caps_length() {
        let s = clean(&"x".repeat(MAX_CHARS + 10));
        assert_eq!(s.chars().count(), MAX_CHARS + 1);
        assert!(s.ends_with('…'));
    }
}
