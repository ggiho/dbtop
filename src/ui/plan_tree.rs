//! The plan tree drawn with branches instead of indentation and arrows, and coloured by what each
//! step does. MySQL (`EXPLAIN FORMAT=TREE`) and Postgres write their plans alike enough for one
//! reader: a line with an arrow is a step, indented under the step it feeds; any other line
//! belongs to the step above it.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::text::{printable, wrap};
use super::theme::THEME;

/// What a step does to the rows, as far as a colour can say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Reads everything there is to read.
    Scan,
    /// Finds its rows through an index.
    Index,
    Join,
    Plain,
}

fn colour(op: Op) -> Color {
    match op {
        Op::Scan => THEME.warn,
        Op::Index => THEME.heat.at(0.0),
        Op::Join => THEME.accent,
        Op::Plain => THEME.text,
    }
}

/// How steps begin, longest first, so that the first match is the most specific one. A name must
/// end where the step's own words go on: `Index scan on t` (MySQL) reads a whole index, while
/// `Index Scan using i on t` (Postgres) looks things up in one.
const OPS: [(&str, Op); 36] = [
    ("single-row covering index lookup", Op::Index),
    ("covering index range scan", Op::Index),
    ("covering index skip scan", Op::Index),
    ("multi-range index lookup", Op::Index),
    ("parallel index only scan", Op::Index),
    ("single-row index lookup", Op::Index),
    ("nested loop inner join", Op::Join),
    ("covering index lookup", Op::Index),
    ("nested loop left join", Op::Join),
    ("nested loop antijoin", Op::Join),
    ("nested loop semijoin", Op::Join),
    ("covering index scan", Op::Scan),
    ("parallel index scan", Op::Index),
    ("bitmap index scan", Op::Index),
    ("parallel seq scan", Op::Scan),
    ("bitmap heap scan", Op::Index),
    ("index range scan", Op::Index),
    ("index scan using", Op::Index),
    ("merge right join", Op::Join),
    ("hash right join", Op::Join),
    ("index only scan", Op::Index),
    ("index skip scan", Op::Index),
    ("inner hash join", Op::Join),
    ("merge left join", Op::Join),
    ("right hash join", Op::Join),
    ("hash anti join", Op::Join),
    ("hash left join", Op::Join),
    ("hash semi join", Op::Join),
    ("left hash join", Op::Join),
    ("index scan on", Op::Scan),
    ("index lookup", Op::Index),
    ("nested loop", Op::Join),
    ("merge join", Op::Join),
    ("table scan", Op::Scan),
    ("hash join", Op::Join),
    ("seq scan", Op::Scan),
];

/// The kind of step `head` begins, and how many bytes of it name the operation.
fn classify(head: &str) -> (Op, usize) {
    let lower = head.to_ascii_lowercase();
    for (name, op) in OPS {
        let Some(rest) = lower.strip_prefix(name) else {
            continue;
        };
        // A name ends where a word does: `seq scan` is not the start of `seq scanner`.
        if rest.chars().next().is_some_and(char::is_alphanumeric) {
            continue;
        }
        // A scan of a temporary or derived table (`Table scan on <temporary>`) reads what the
        // query itself just made: nothing to warn about.
        if op == Op::Scan && rest.trim_start().starts_with("on <") {
            return (Op::Plain, 0);
        }
        return (op, name.len());
    }
    (Op::Plain, 0)
}

/// A step, the lines that belong to it, and the steps it feeds.
struct Step {
    indent: usize,
    text: String,
    details: Vec<String>,
    children: Vec<usize>,
}

/// The steps in the order they were written, and which of them are at the top.
fn parse(text: &str) -> (Vec<Step>, Vec<usize>) {
    let mut steps: Vec<Step> = Vec::new();
    let mut roots = Vec::new();
    // The step last written and the ones it feeds, innermost last.
    let mut open: Vec<usize> = Vec::new();
    for raw in text.lines() {
        let line: String = raw.chars().map(printable).collect();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let step_text = match trimmed.strip_prefix("->") {
            Some(rest) => rest.trim().to_string(),
            // Postgres writes its top step without an arrow.
            None if steps.is_empty() => trimmed.trim_end().to_string(),
            None => {
                if let Some(last) = steps.last_mut() {
                    last.details.push(trimmed.trim_end().to_string());
                }
                continue;
            }
        };
        while open.last().is_some_and(|&p| steps[p].indent >= indent) {
            open.pop();
        }
        let at = steps.len();
        match open.last() {
            Some(&parent) => steps[parent].children.push(at),
            None => roots.push(at),
        }
        steps.push(Step {
            indent,
            text: step_text,
            details: Vec::new(),
            children: Vec::new(),
        });
        open.push(at);
    }
    (steps, roots)
}

/// A word, or the estimate that ends a step, with how it is drawn.
struct Token {
    parts: Vec<(String, Style)>,
    /// Spaces before it, unless it starts a line.
    gap: usize,
}

impl Token {
    fn word(text: &str, style: Style) -> Token {
        Token {
            parts: vec![(text.to_string(), style)],
            gap: 1,
        }
    }

    fn width(&self) -> usize {
        self.parts.iter().map(|(text, _)| text.width()).sum()
    }
}

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

/// `Table scan on o  (cost=8123 rows=812300)` as the step and its estimate.
fn split_estimate(text: &str) -> (&str, Option<&str>) {
    if let Some(at) = text.rfind("(cost=") {
        let head = &text[..at];
        if text.ends_with(')') && (head.is_empty() || head.ends_with(' ')) {
            return (head.trim_end(), Some(&text[at..]));
        }
    }
    (text, None)
}

/// The estimate, dim, but for the number of rows: that is what a plan is read for.
fn estimate_token(estimate: &str) -> Token {
    let (dim, strong) = (fg(THEME.dim), fg(THEME.text));
    let mut parts = Vec::new();
    match estimate.find("rows=") {
        Some(at) => {
            let from = at + "rows=".len();
            let to = from
                + estimate[from..]
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .count();
            parts.push((estimate[..from].to_string(), dim));
            parts.push((estimate[from..to].to_string(), strong));
            parts.push((estimate[to..].to_string(), dim));
        }
        None => parts.push((estimate.to_string(), dim)),
    }
    parts.retain(|(text, _)| !text.is_empty());
    Token { parts, gap: 2 }
}

/// The words of a step: its operation in the colour of what it does, then the rest, then the
/// estimate. Spaces are kept as written, so a literal with two in it still has two.
fn step_tokens(text: &str) -> Vec<Token> {
    let (head, estimate) = split_estimate(text);
    let (op, len) = classify(head);
    let (name, rest) = head.split_at(len);
    let mut tokens = Vec::new();
    if len > 0 {
        tokens.extend(name.split(' ').map(|w| Token::word(w, fg(colour(op)))));
    }
    let rest = if len > 0 {
        rest.strip_prefix(' ').unwrap_or(rest)
    } else {
        rest
    };
    if len == 0 || !rest.is_empty() {
        tokens.extend(rest.split(' ').map(|w| Token::word(w, fg(THEME.text))));
    }
    tokens.extend(estimate.map(estimate_token));
    tokens
}

/// The words of a line that belongs to a step (`Filter: …`), all dim.
fn detail_tokens(text: &str) -> Vec<Token> {
    text.split(' ')
        .map(|w| Token::word(w, fg(THEME.dim)))
        .collect()
}

/// Fills lines of at most `room` columns with `tokens`. A word wider than a line is broken like
/// `text::wrap` breaks it.
fn flow(tokens: Vec<Token>, room: usize) -> Vec<Vec<Span<'static>>> {
    let room = room.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for token in tokens {
        let pieces = if token.width() > room {
            let style = token.parts.first().map_or(Style::new(), |(_, s)| *s);
            let joined: String = token.parts.iter().map(|(t, _)| t.as_str()).collect();
            wrap(&joined, room)
                .into_iter()
                .enumerate()
                .map(|(i, piece)| Token {
                    parts: vec![(piece, style)],
                    gap: if i == 0 { token.gap } else { 0 },
                })
                .collect()
        } else {
            vec![token]
        };
        let count = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            let width = piece.width();
            if used > 0 && used + piece.gap + width > room {
                lines.push(Vec::new());
                used = 0;
            }
            let line = lines.last_mut().expect("there is always a line");
            if used > 0 && piece.gap > 0 {
                line.push(Span::raw(" ".repeat(piece.gap)));
                used += piece.gap;
            }
            line.extend(
                piece
                    .parts
                    .into_iter()
                    .map(|(text, style)| Span::styled(text, style)),
            );
            used += width;
            // A word broken in pieces goes on in the next line.
            if i + 1 < count {
                lines.push(Vec::new());
                used = 0;
            }
        }
    }
    lines
}

/// Where a step stands among the ones beside it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Top,
    Middle,
    Last,
}

fn draw(
    steps: &[Step],
    at: usize,
    lead: &str,
    place: Place,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let step = &steps[at];
    // `carry` is what the lines under the step put in the column of its branch.
    let (branch, carry) = match place {
        Place::Top => ("", ""),
        Place::Middle => ("├─ ", "│  "),
        Place::Last => ("└─ ", "   "),
    };
    let edge = fg(THEME.border);
    let room = width.saturating_sub(lead.width() + branch.width());

    for (i, spans) in flow(step_tokens(&step.text), room).into_iter().enumerate() {
        let guide = if i == 0 {
            format!("{lead}{branch}")
        } else {
            format!("{lead}{carry}")
        };
        let mut line = vec![Span::styled(guide, edge)];
        line.extend(spans);
        out.push(Line::from(line));
    }
    let under = format!("{lead}{carry}");
    for detail in &step.details {
        for spans in flow(detail_tokens(detail), room) {
            let mut line = vec![Span::styled(under.clone(), edge)];
            line.extend(spans);
            out.push(Line::from(line));
        }
    }
    for (n, &child) in step.children.iter().enumerate() {
        let place = if n + 1 == step.children.len() {
            Place::Last
        } else {
            Place::Middle
        };
        draw(steps, child, &under, place, width, out);
    }
}

/// The plan `text` as a tree of at most `width` columns.
pub fn lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let (steps, roots) = parse(text);
    let mut out = Vec::new();
    for root in roots {
        draw(&steps, root, "", Place::Top, width, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL: &str = concat!(
        "-> Limit: 50 row(s)  (cost=18432 rows=50)\n",
        "    -> Nested loop inner join  (cost=18432 rows=81230)\n",
        "        -> Sort: o.created_at DESC  (cost=8123 rows=81230)\n",
        "            -> Filter: (o.status = 'PENDING')  (cost=8123 rows=81230)\n",
        "                -> Table scan on o  (cost=8123 rows=812300)\n",
        "        -> Single-row index lookup on c using PRIMARY (id=o.customer_id)  (cost=0.25 rows=1)",
    );

    const POSTGRES: &str = concat!(
        "Limit  (cost=146.07..146.08 rows=5 width=8)\n",
        "  ->  Sort  (cost=146.07..146.10 rows=13 width=8)\n",
        "        Sort Key: o.id\n",
        "        ->  Bitmap Heap Scan on orders o  (cost=4.60..145.85 rows=13 width=8)\n",
        "              Recheck Cond: (1 = customer_id)\n",
        "              Filter: (status = 'PAID'::text)\n",
        "              ->  Bitmap Index Scan on idx_orders_customer  (cost=0.00..4.59 rows=40 width=0)\n",
        "                    Index Cond: (customer_id = 1)",
    );

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The spans of the first line that holds `text`.
    fn spans_of<'a>(lines: &'a [Line<'static>], text: &str) -> &'a [Span<'static>] {
        let found = lines.iter().find(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .contains(text)
        });
        &found
            .unwrap_or_else(|| panic!("no line holds {text:?}"))
            .spans
    }

    fn style_of(spans: &[Span], text: &str) -> Style {
        spans
            .iter()
            .find(|s| s.content == text)
            .unwrap_or_else(|| panic!("no span {text:?} in {spans:?}"))
            .style
    }

    #[test]
    fn a_mysql_tree_is_drawn_with_branches() {
        assert_eq!(
            plain(&lines(MYSQL, 120)),
            [
                "Limit: 50 row(s)  (cost=18432 rows=50)",
                "└─ Nested loop inner join  (cost=18432 rows=81230)",
                "   ├─ Sort: o.created_at DESC  (cost=8123 rows=81230)",
                "   │  └─ Filter: (o.status = 'PENDING')  (cost=8123 rows=81230)",
                "   │     └─ Table scan on o  (cost=8123 rows=812300)",
                "   └─ Single-row index lookup on c using PRIMARY (id=o.customer_id)  (cost=0.25 rows=1)",
            ]
        );
    }

    #[test]
    fn a_postgres_plan_keeps_its_top_step_and_the_lines_that_belong_to_a_step() {
        assert_eq!(
            plain(&lines(POSTGRES, 120)),
            [
                "Limit  (cost=146.07..146.08 rows=5 width=8)",
                "└─ Sort  (cost=146.07..146.10 rows=13 width=8)",
                "   Sort Key: o.id",
                "   └─ Bitmap Heap Scan on orders o  (cost=4.60..145.85 rows=13 width=8)",
                "      Recheck Cond: (1 = customer_id)",
                "      Filter: (status = 'PAID'::text)",
                "      └─ Bitmap Index Scan on idx_orders_customer  (cost=0.00..4.59 rows=40 width=0)",
                "         Index Cond: (customer_id = 1)",
            ]
        );
    }

    #[test]
    fn a_long_step_wraps_under_its_own_text_and_the_branches_go_on_beside_it() {
        let last = plain(&lines(MYSQL, 36));
        assert_eq!(
            last[last.len() - 4..],
            [
                "   └─ Single-row index lookup on c",
                "      using PRIMARY",
                "      (id=o.customer_id)",
                "      (cost=0.25 rows=1)",
            ]
        );
        // A step with brothers after it keeps the line down beside what wraps.
        let sort = plain(&lines(MYSQL, 30));
        assert!(
            sort.contains(&"   │  (cost=8123 rows=81230)".to_string()),
            "{sort:?}"
        );
    }

    #[test]
    fn nothing_is_lost_or_added_whatever_the_width() {
        // Everything but the layout: no blanks, no arrows, no branches.
        let soup = |text: &str, glyphs: &str| -> String {
            text.chars()
                .filter(|c| !c.is_whitespace() && !glyphs.contains(*c))
                .collect()
        };
        for plan in [MYSQL, POSTGRES] {
            let original = soup(&plan.replace("->", ""), "");
            for width in [1, 8, 20, 36, 80, 200] {
                let drawn = plain(&lines(plan, width)).join("\n");
                assert_eq!(soup(&drawn, "│├└─"), original, "width {width}");
            }
        }
    }

    #[test]
    fn spaces_inside_a_step_stay_as_written() {
        let drawn = plain(&lines("-> Filter: (name = 'a  b')  (cost=1 rows=2)", 80));
        assert_eq!(drawn, ["Filter: (name = 'a  b')  (cost=1 rows=2)"]);
    }

    #[test]
    fn a_step_is_coloured_by_what_it_does() {
        let drawn = lines(MYSQL, 120);

        let scan = spans_of(&drawn, "Table scan");
        assert_eq!(
            style_of(scan, "Table").fg,
            Some(THEME.warn),
            "reads the whole table"
        );
        assert_eq!(style_of(scan, "scan").fg, Some(THEME.warn));
        assert_eq!(
            style_of(scan, "on").fg,
            Some(THEME.text),
            "only the operation is coloured"
        );

        let lookup = spans_of(&drawn, "index lookup");
        assert_eq!(
            style_of(lookup, "index").fg,
            Some(THEME.heat.at(0.0)),
            "uses an index"
        );

        let join = spans_of(&drawn, "Nested loop");
        assert_eq!(style_of(join, "Nested").fg, Some(THEME.accent));
        assert_eq!(style_of(join, "join").fg, Some(THEME.accent));

        // The estimate is dim, but for the rows: that is what a plan is read for.
        assert_eq!(style_of(scan, "(cost=8123 rows=").fg, Some(THEME.dim));
        assert_eq!(style_of(scan, "812300").fg, Some(THEME.text));
        assert_eq!(style_of(scan, ")").fg, Some(THEME.dim));

        // The branches are drawn like the borders.
        assert_eq!(style_of(scan, "   │     └─ ").fg, Some(THEME.border));
    }

    #[test]
    fn what_the_query_made_itself_is_not_a_table_scan_to_warn_about() {
        let drawn = lines("-> Table scan on <temporary>", 80);
        assert_eq!(style_of(&drawn[0].spans, "Table").fg, Some(THEME.text));
        // Nor is it every kind of scan: an index range scan is a way to find rows.
        assert_eq!(classify("Index range scan on t").0, Op::Index);
        assert_eq!(
            classify("Index scan on t using idx").0,
            Op::Scan,
            "MySQL: a whole index"
        );
        assert_eq!(
            classify("Index Scan using idx on t").0,
            Op::Index,
            "Postgres: a lookup"
        );
    }

    #[test]
    fn the_name_of_an_operation_ends_where_a_word_does() {
        assert_eq!(classify("Seq Scan on orders"), (Op::Scan, 8));
        assert_eq!(classify("Nested loop inner join"), (Op::Join, 22));
        assert_eq!(classify("Nested Loop Left Join"), (Op::Join, 21));
        assert_eq!(classify("Sequential scanner"), (Op::Plain, 0));
        assert_eq!(classify("Seq Scanner on t"), (Op::Plain, 0));
        assert_eq!(classify("Filter: (x)"), (Op::Plain, 0));
    }

    #[test]
    fn a_more_specific_name_comes_before_the_shorter_one_it_begins_with() {
        for (i, (earlier, _)) in OPS.iter().enumerate() {
            assert_eq!(*earlier, earlier.to_ascii_lowercase());
            for (later, _) in &OPS[i + 1..] {
                assert!(
                    !later.starts_with(earlier),
                    "{later:?} is hidden by {earlier:?}"
                );
            }
        }
    }

    #[test]
    fn control_characters_never_reach_the_screen() {
        let drawn = plain(&lines(
            "-> Table scan on \x1b[31mevil\x07\n   Filter: \x1b[0m",
            80,
        ))
        .join("\n");
        assert!(
            !drawn.contains('\x1b') && !drawn.contains('\x07'),
            "{drawn:?}"
        );
        assert!(drawn.contains('\u{FFFD}'), "{drawn:?}");
    }

    #[test]
    fn text_that_is_not_a_plan_is_shown_as_it_is() {
        assert_eq!(
            plain(&lines("just a sentence\nand another", 80)),
            ["just a sentence", "and another"]
        );
        assert!(lines("", 80).is_empty());
        assert!(lines("  \n\n   ", 80).is_empty());
    }

    #[test]
    fn odd_indentation_and_odd_estimates_never_panic() {
        let corpus = [
            "->",
            "-> ",
            "->\n->",
            "    -> deep\n-> shallow\n        -> deeper\n  -> between",
            "\n\n-> a\n\n",
            "-> a  (cost=",
            "-> a (cost=1 rows=)",
            "-> (cost=1 rows=2)",
            "-> a (cost=1 rows=2) b (cost=3 rows=4)",
            "-> index lookup",
            "-> 한글 인덱스 lookup on 테이블  (cost=1 rows=2)",
        ];
        let long = "x".repeat(500);
        for text in corpus.iter().copied().chain([long.as_str()]) {
            for width in [0, 1, 2, 5, 80] {
                let drawn = lines(text, width);
                assert!(!drawn.is_empty(), "{text:?} at {width}");
            }
        }
    }
}
