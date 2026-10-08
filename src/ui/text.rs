use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn fmt_secs(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{}m{:02}s", secs as u64 / 60, secs as u64 % 60)
    } else {
        format!("{}h{:02}m", secs as u64 / 3600, secs as u64 % 3600 / 60)
    }
}

pub fn fmt_ms(ms: f64) -> String {
    fmt_secs(ms / 1000.0)
}

pub fn fmt_count(n: f64) -> String {
    if n < 1_000.0 {
        format!("{n:.0}")
    } else if n < 10_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else if n < 1_000_000.0 {
        format!("{:.0}k", n / 1_000.0)
    } else {
        format!("{:.1}M", n / 1_000_000.0)
    }
}

/// Query text comes from the database, so a control character in it must not reach the terminal.
/// Whitespace controls become a plain space; anything else becomes U+FFFD.
fn printable(c: char) -> char {
    match c {
        '\t' | '\n' | '\r' | '\x0b' | '\x0c' => ' ',
        c if c.is_control() => '\u{FFFD}',
        c => c,
    }
}

/// Collapses a statement to one line for a table cell. Only the head is kept: no cell is
/// wider than a screen, and statements can be megabytes.
pub fn one_line(s: &str) -> String {
    let head: String = s.chars().take(512).map(printable).collect();
    head.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Hard-wraps `text` to `width` display columns, keeping its own line breaks.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut cur = String::new();
        let mut used = 0;
        for c in line.replace('\t', "    ").chars().map(printable) {
            let w = c.width().unwrap_or(0);
            if used + w > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                used = 0;
            }
            cur.push(c);
            used += w;
        }
        out.push(cur);
    }
    out
}

/// Wraps prose at spaces, for sentences. A word longer than a line is broken like `wrap` does.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text: String = text.chars().map(printable).collect();
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            out.push(std::mem::take(&mut line));
        }
        if word.width() > width {
            let mut pieces = wrap(word, width);
            line = pieces.pop().unwrap_or_default();
            out.extend(pieces);
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_secs(0.25), "250ms");
        assert_eq!(fmt_secs(3.21), "3.2s");
        assert_eq!(fmt_secs(65.0), "1m05s");
        assert_eq!(fmt_secs(3725.0), "1h02m");
        assert_eq!(fmt_ms(91_000.0), "1m31s");
    }

    #[test]
    fn formats_counts() {
        assert_eq!(fmt_count(842.0), "842");
        assert_eq!(fmt_count(1234.0), "1.2k");
        assert_eq!(fmt_count(25_000.0), "25k");
        assert_eq!(fmt_count(1_500_000.0), "1.5M");
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("SELECT  1\n  FROM\tt"), "SELECT 1 FROM t");
    }

    #[test]
    fn control_characters_never_survive() {
        assert!(!one_line("a\x1b[31mb").contains('\x1b'));
        assert!(wrap("a\x1b[31mb", 80).iter().all(|l| !l.contains('\x1b')));
        assert_eq!(
            wrap("a\rb\x0bc", 80),
            ["a b c"],
            "a bare CR would move the cursor"
        );
    }

    #[test]
    fn tabs_indent_in_wrapped_text_and_collapse_in_cells() {
        assert_eq!(wrap("\tSELECT 1", 80), ["    SELECT 1"]);
        assert_eq!(one_line("a\t\tb"), "a b");
    }

    #[test]
    fn wrap_counts_display_width_and_keeps_line_breaks() {
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("가나다", 4), ["가나", "다"], "CJK is two columns wide");
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    }

    #[test]
    fn prose_breaks_at_spaces_not_inside_words() {
        let sentence = "EXPLAIN FOR CONNECTION was refused (ERROR 1045: Access denied for user (using password: YES))";
        let lines = wrap_words(sentence, 40);
        assert!(lines.iter().all(|l| l.width() <= 40), "{lines:?}");
        assert_eq!(
            lines.join(" "),
            sentence,
            "nothing lost, nothing split: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.starts_with(' ') && !l.ends_with(' '))
        );
    }

    #[test]
    fn prose_handles_long_words_cjk_and_nothing() {
        assert_eq!(
            wrap_words("ab abcdefghij cd", 4),
            ["ab", "abcd", "efgh", "ij", "cd"]
        );
        assert_eq!(wrap_words("월말 정산 배치", 6), ["월말", "정산", "배치"]);
        assert!(wrap_words("   ", 10).is_empty());
        assert_eq!(wrap_words("a\x1b[31mb", 20), ["a\u{FFFD}[31mb"]);
    }

    #[test]
    fn wrap_never_loops_on_a_character_wider_than_the_width() {
        assert_eq!(wrap("가나", 1), ["가", "나"]);
    }
}
