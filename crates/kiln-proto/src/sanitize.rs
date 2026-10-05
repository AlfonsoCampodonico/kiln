//! Sanitising untrusted text before printing it (spec §13, threat T8): strings from
//! images and from guests (`InitFailed`, console excerpts).

/// Longest sanitised string, in characters.
pub const MAX_CHARS: usize = 4096;

/// Unicode format characters (category Cf: bidi overrides and isolates, zero-width
/// characters, BOM, tags) and the line and paragraph separators (Zl, Zp). They
/// can reorder or hide what the terminal shows ("Trojan Source"), so they go too.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}' | '\u{0890}'..='\u{0891}'
        | '\u{08E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}')
}

/// Removes control characters (C0, DEL, C1, so ESC too) except `\n` and `\t`,
/// and invisible format characters, and caps the length.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_CHARS));
    for (i, c) in s
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !(c.is_control() || is_invisible(c)))
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
    fn strips_bidi_overrides_and_zero_width_characters() {
        let s = "cmd\u{202E}gnp.exe\u{2066}x\u{2069}\u{200B}\u{FEFF}y\u{2028}z\u{E0041}";
        assert_eq!(clean(s), "cmdgnp.exexyz");
        assert_eq!(clean("café ünï 日本"), "café ünï 日本", "ordinary text is untouched");
    }

    #[test]
    fn caps_length() {
        let s = clean(&"x".repeat(MAX_CHARS + 10));
        assert_eq!(s.chars().count(), MAX_CHARS + 1);
        assert!(s.ends_with('…'));
    }
}
