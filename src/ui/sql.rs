//! Colours for SQL text. This is a lexer, not a parser: it reads MySQL and Postgres text well
//! enough to tell keywords, strings, numbers, comments and names apart, which is all a colour needs.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use super::text::{printable, wrap};
use super::theme::THEME;

/// Past this the text is shown plain: a statement can be megabytes, and it is drawn every frame.
const MAX_COLOURED: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Keyword,
    Function,
    Str,
    Number,
    Comment,
    Quoted,
    Operator,
    Param,
}

fn style(kind: Kind) -> Style {
    let fg = |color| Style::new().fg(color);
    match kind {
        Kind::Plain => fg(THEME.text),
        Kind::Keyword => fg(THEME.keyword),
        Kind::Function => fg(THEME.function),
        Kind::Str => fg(THEME.string),
        Kind::Number => fg(THEME.number),
        Kind::Comment => fg(THEME.comment).add_modifier(Modifier::ITALIC),
        Kind::Quoted => fg(THEME.quoted),
        Kind::Operator => fg(THEME.operator),
        Kind::Param => fg(THEME.param),
    }
}

/// Upper case and sorted, for a binary search.
const KEYWORDS: [&str; 123] = [
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CALL",
    "CASE",
    "CHECK",
    "COLLATE",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DO",
    "DROP",
    "DUPLICATE",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FORCE",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "IGNORE",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOCK",
    "LOCKED",
    "NATURAL",
    "NEXT",
    "NOT",
    "NOTHING",
    "NOWAIT",
    "NULL",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRECEDING",
    "PRIMARY",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "REGEXP",
    "REPLACE",
    "RETURNING",
    "REVOKE",
    "RIGHT",
    "RLIKE",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SCHEMA",
    "SELECT",
    "SET",
    "SHARE",
    "SHOW",
    "SKIP",
    "START",
    "STRAIGHT_JOIN",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRUE",
    "TRUNCATE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

fn is_keyword(word: &str) -> bool {
    word.is_ascii()
        && KEYWORDS
            .binary_search_by(|k| k.cmp(&word.to_ascii_uppercase().as_str()))
            .is_ok()
}

/// A name may hold any letter, and a UTF-8 sequence is only ever bytes of 0x80 and up.
fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_operator_byte(c: u8) -> bool {
    b"=<>!+-*/%|&^~:#".contains(&c)
}

fn run_of(b: &[u8], mut i: usize, keep: impl Fn(u8) -> bool) -> usize {
    while i < b.len() && keep(b[i]) {
        i += 1;
    }
    i
}

/// The end of a line comment starting at `i`: the newline stays outside it.
fn line_end(b: &[u8], i: usize) -> usize {
    run_of(b, i, |c| c != b'\n')
}

/// The end of a `/* … */` comment starting at `i`; one never closed runs to the end.
fn block_end(b: &[u8], i: usize) -> usize {
    (i + 2..b.len().saturating_sub(1))
        .find(|&j| b[j] == b'*' && b[j + 1] == b'/')
        .map_or(b.len(), |j| j + 2)
}

/// The end of the text quoted with `quote` that starts at `i`. A doubled quote is a quote inside,
/// and in a string so is a backslash escape; one never closed runs to the end.
fn quoted_end(b: &[u8], i: usize, quote: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' if quote == b'\'' => j += 2,
            c if c == quote && b.get(j + 1) == Some(&quote) => j += 2,
            c if c == quote => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

fn number_end(b: &[u8], i: usize) -> usize {
    if b[i] == b'0'
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        return run_of(b, i + 2, |c| c.is_ascii_hexdigit());
    }
    let mut j = run_of(b, i, |c| c.is_ascii_digit());
    if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
        j = run_of(b, j + 1, |c| c.is_ascii_digit());
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(j + 1), Some(b'+' | b'-')));
        if b.get(j + 1 + sign).is_some_and(u8::is_ascii_digit) {
            j = run_of(b, j + 1 + sign, |c| c.is_ascii_digit());
        }
    }
    j
}

/// Whether the `#` at `i` starts a MySQL comment. In Postgres `#>>` and `#-` are operators, so it
/// only does when a space follows or it is the first thing on its line.
fn hash_starts_a_comment(b: &[u8], i: usize) -> bool {
    match b.get(i + 1) {
        None | Some(b' ' | b'\t' | b'\n' | b'\r') => true,
        _ => b[..i]
            .iter()
            .rev()
            .take_while(|&&c| c != b'\n')
            .all(|&c| c == b' ' || c == b'\t'),
    }
}

/// Splits `sql` into runs of one kind that together are the whole text.
fn lex(sql: &str) -> Vec<(Kind, &str)> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            _ if c.is_ascii_whitespace() => {
                i = run_of(b, i, |c| c.is_ascii_whitespace());
                Kind::Plain
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = line_end(b, i);
                Kind::Comment
            }
            b'#' if hash_starts_a_comment(b, i) => {
                i = line_end(b, i);
                Kind::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = block_end(b, i);
                Kind::Comment
            }
            b'\'' => {
                i = quoted_end(b, i, b'\'');
                Kind::Str
            }
            b'"' | b'`' => {
                i = quoted_end(b, i, c);
                Kind::Quoted
            }
            b'0'..=b'9' => {
                i = number_end(b, i);
                Kind::Number
            }
            b'?' => {
                i += 1;
                Kind::Param
            }
            b'$' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i = run_of(b, i + 1, |c| c.is_ascii_digit());
                Kind::Param
            }
            b'@' => {
                i = run_of(b, i, |c| c == b'@');
                i = run_of(b, i, is_word_byte);
                Kind::Param
            }
            _ if is_word_start(c) => {
                i = run_of(b, i, is_word_byte);
                let word = &sql[start..i];
                if is_keyword(word) {
                    Kind::Keyword
                // A call has its parenthesis against the name; `INSERT INTO t (a)` does not.
                } else if b.get(i) == Some(&b'(') {
                    Kind::Function
                } else {
                    Kind::Plain
                }
            }
            _ if is_operator_byte(c) => {
                // Never run on into the start of a comment.
                i += 1;
                while i < b.len()
                    && is_operator_byte(b[i])
                    && !matches!(
                        (b[i], b.get(i + 1)),
                        (b'-', Some(b'-')) | (b'/', Some(b'*'))
                    )
                {
                    i += 1;
                }
                Kind::Operator
            }
            // Punctuation, which is ASCII: anything else starts a name.
            _ => {
                i += 1;
                Kind::Plain
            }
        };
        out.push((kind, &sql[start..i]));
    }
    out
}

/// Lines being filled, wrapped where `text::wrap` would wrap them.
struct Lines {
    width: usize,
    done: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    run: String,
    run_style: Style,
    used: usize,
    chars: usize,
}

impl Lines {
    fn push(&mut self, c: char, style: Style) {
        let w = c.width().unwrap_or(0);
        if self.used + w > self.width && self.chars > 0 {
            self.end_line();
        }
        if style != self.run_style {
            self.end_run();
            self.run_style = style;
        }
        self.run.push(c);
        self.used += w;
        self.chars += 1;
    }

    fn end_run(&mut self) {
        if !self.run.is_empty() {
            self.spans
                .push(Span::styled(std::mem::take(&mut self.run), self.run_style));
        }
    }

    fn end_line(&mut self) {
        self.end_run();
        self.done.push(Line::from(std::mem::take(&mut self.spans)));
        self.used = 0;
        self.chars = 0;
    }
}

/// `sql` as lines of at most `width` columns, coloured. The breaks fall where `text::wrap` puts
/// them, so the text is laid out as it always was.
pub fn highlight(sql: &str, width: usize) -> Vec<Line<'static>> {
    if sql.len() > MAX_COLOURED {
        return wrap(sql, width)
            .into_iter()
            .map(|line| Line::styled(line, style(Kind::Plain)))
            .collect();
    }
    let sql = sql.replace("\r\n", "\n");
    let mut lines = Lines {
        width: width.max(1),
        done: Vec::new(),
        spans: Vec::new(),
        run: String::new(),
        run_style: style(Kind::Plain),
        used: 0,
        chars: 0,
    };
    for (kind, text) in lex(&sql) {
        let style = style(kind);
        for c in text.chars() {
            match c {
                '\n' => lines.end_line(),
                '\t' => (0..4).for_each(|_| lines.push(' ', style)),
                c => lines.push(printable(c), style),
            }
        }
    }
    // Like `lines()`, a text that ends in a newline has no empty line after it.
    if lines.chars > 0 {
        lines.end_line();
    }
    lines.done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::wrap;

    /// The runs that are not blanks.
    fn kinds(sql: &str) -> Vec<(Kind, &str)> {
        lex(sql)
            .into_iter()
            .filter(|(kind, text)| !(*kind == Kind::Plain && text.trim().is_empty()))
            .collect()
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn the_keywords_are_upper_case_and_sorted_for_the_binary_search() {
        assert!(KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(KEYWORDS.iter().all(|k| *k == k.to_ascii_uppercase()));
        assert!(is_keyword("select") && is_keyword("Where") && !is_keyword("orders"));
        assert!(!is_keyword("SÉLECT"), "not ASCII, not a keyword");
    }

    #[test]
    fn a_select_is_told_apart() {
        use Kind::*;
        assert_eq!(
            kinds("SELECT COUNT(*) FROM orders WHERE status = 'PAID' AND amount > 1.5 -- tail"),
            [
                (Keyword, "SELECT"),
                (Function, "COUNT"),
                (Plain, "("),
                (Operator, "*"),
                (Plain, ")"),
                (Keyword, "FROM"),
                (Plain, "orders"),
                (Keyword, "WHERE"),
                (Plain, "status"),
                (Operator, "="),
                (Str, "'PAID'"),
                (Keyword, "AND"),
                (Plain, "amount"),
                (Operator, ">"),
                (Number, "1.5"),
                (Comment, "-- tail"),
            ]
        );
    }

    #[test]
    fn quotes_are_closed_where_the_sql_closes_them() {
        use Kind::*;
        assert_eq!(
            kinds(r#"'it''s' 'a\'b' "Order" `sta``tus` 'open"#),
            [
                (Str, "'it''s'"),
                (Str, r"'a\'b'"),
                (Quoted, r#""Order""#),
                (Quoted, "`sta``tus`"),
                (Str, "'open"),
            ],
            "the last one is never closed and runs to the end"
        );
    }

    #[test]
    fn comments_in_both_dialects_and_the_postgres_operators_that_look_like_one() {
        use Kind::*;
        assert_eq!(
            kinds("# note\nSELECT a #>> 'x' -- c\n/* b\nc */ /*! STRAIGHT_JOIN */ d"),
            [
                (Comment, "# note"),
                (Keyword, "SELECT"),
                (Plain, "a"),
                (Operator, "#>>"),
                (Str, "'x'"),
                (Comment, "-- c"),
                (Comment, "/* b\nc */"),
                (Comment, "/*! STRAIGHT_JOIN */"),
                (Plain, "d"),
            ]
        );
        assert_eq!(kinds("/* never closed"), [(Comment, "/* never closed")]);
    }

    #[test]
    fn numbers_and_placeholders() {
        use Kind::*;
        assert_eq!(
            kinds("42 3.14 1e10 2.5E-3 0xFF ? $1 @v @@max_connections"),
            [
                (Number, "42"),
                (Number, "3.14"),
                (Number, "1e10"),
                (Number, "2.5E-3"),
                (Number, "0xFF"),
                (Param, "?"),
                (Param, "$1"),
                (Param, "@v"),
                (Param, "@@max_connections"),
            ]
        );
        // A digit inside a name does not start a number.
        assert_eq!(
            kinds("t1.col2"),
            [(Plain, "t1"), (Plain, "."), (Plain, "col2")]
        );
    }

    #[test]
    fn a_call_is_a_function_but_a_keyword_or_a_name_with_a_space_before_the_parenthesis_is_not() {
        let found = kinds("date(created_at) IN (1) INSERT INTO orders (id) VALUES (1)");
        let kind_of = |text: &str| found.iter().find(|(_, t)| *t == text).unwrap().0;
        assert_eq!(kind_of("date"), Kind::Function);
        assert_eq!(kind_of("IN"), Kind::Keyword);
        assert_eq!(kind_of("orders"), Kind::Plain);
        assert_eq!(kind_of("VALUES"), Kind::Keyword);
    }

    #[test]
    fn an_operator_stops_before_the_comment_that_follows_it() {
        use Kind::*;
        assert_eq!(
            kinds("a=-- c\nb<=/*x*/c"),
            [
                (Plain, "a"),
                (Operator, "="),
                (Comment, "-- c"),
                (Plain, "b"),
                (Operator, "<="),
                (Comment, "/*x*/"),
                (Plain, "c"),
            ]
        );
    }

    #[test]
    fn text_outside_ascii_is_kept_whole() {
        use Kind::*;
        assert_eq!(
            kinds("SELECT '한글' AS 별칭, 🙂 FROM t"),
            [
                (Keyword, "SELECT"),
                (Str, "'한글'"),
                (Keyword, "AS"),
                (Plain, "별칭"),
                (Plain, ","),
                (Plain, "🙂"),
                (Keyword, "FROM"),
                (Plain, "t"),
            ]
        );
    }

    /// Texts that end in the middle of something, or hold what is not SQL at all.
    const AWKWARD: [&str; 22] = [
        "",
        "SELECT 1",
        "'",
        "'abc",
        "\"",
        "`",
        "/*",
        "/* x",
        "-- ",
        "--",
        "#",
        "# x",
        "a\\",
        "'a\\",
        "0x",
        "1e+",
        "$",
        "@@",
        "한글'",
        "x'한",
        "SELECT\t*\nFROM  t -- c\n/* b */\r\n\x0b\u{0}\u{7f}",
        "é\u{301}",
    ];

    #[test]
    fn lexing_loses_nothing_whatever_the_text() {
        for text in AWKWARD {
            let joined: String = lex(text).into_iter().map(|(_, t)| t).collect();
            assert_eq!(joined, text);
        }
    }

    #[test]
    fn colouring_lays_the_text_out_exactly_as_the_plain_wrapper_does() {
        let mut texts: Vec<String> = AWKWARD.iter().map(ToString::to_string).collect();
        texts.extend([
            "SELECT a,\tb\r\nFROM t\r\n\r\nWHERE x = 'a long string that has to be broken somewhere'\n".into(),
            "a\n\n".into(),
            "\n".into(),
            "한글 한글 한글 한글 한글 'x' 한글한글한글한글".into(),
            "x".repeat(300),
            "SELECT 1\n".repeat(5),
        ]);
        for text in &texts {
            for width in [1, 2, 3, 7, 20, 80] {
                assert_eq!(
                    plain(&highlight(text, width)),
                    wrap(text, width),
                    "{text:?} at {width}"
                );
            }
        }
    }

    #[test]
    fn same_coloured_text_is_one_span_and_a_comment_is_italic() {
        let lines = highlight("SELECT 'x' -- c", 80);
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(spans[0].content, "SELECT");
        assert_eq!(spans[0].style.fg, Some(THEME.keyword));
        assert_eq!(spans[2].content, "'x'");
        assert_eq!(spans[2].style.fg, Some(THEME.string));
        let comment = spans.last().unwrap();
        assert_eq!(comment.content, "-- c");
        assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn a_token_broken_across_lines_keeps_its_colour_on_both() {
        let lines = highlight("'abcdefghij'", 6);
        assert_eq!(plain(&lines), ["'abcde", "fghij'"]);
        for line in &lines {
            assert!(line.spans.iter().all(|s| s.style.fg == Some(THEME.string)));
        }
    }

    #[test]
    fn a_statement_too_long_is_shown_plain_but_still_whole() {
        let text = "SELECT 1 ".repeat(10_000);
        assert!(text.len() > MAX_COLOURED);
        let lines = highlight(&text, 100);
        assert_eq!(plain(&lines), wrap(&text, 100));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .all(|s| s.style.fg != Some(THEME.keyword)),
            "no colour"
        );
    }
}
