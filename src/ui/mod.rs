mod graph;
mod text;
mod theme;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Detail, PlanState, Subject, Tab};
use crate::model::{Plan, Sample};
use graph::{BrailleGraph, axis_max, visible};
use text::{fmt_count, fmt_ms, fmt_secs, one_line, wrap, wrap_words};
use theme::THEME;

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

/// Wraps title text in the `┤ … ├` brackets set into a box's border.
fn tag(parts: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let edge = fg(THEME.border);
    let mut spans = vec![Span::styled("┤", edge)];
    spans.extend(parts);
    spans.push(Span::styled("├", edge));
    spans
}

fn panel(title: Vec<Span<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(THEME.border))
        .title_top(Line::from(title))
}

/// Below this the table columns get squeezed until a number like `120ms` reads as `120m`.
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 20;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let message = format!(
            "terminal too small: {}x{}\nneed at least {MIN_WIDTH}x{MIN_HEIGHT}",
            area.width, area.height
        );
        f.render_widget(
            Paragraph::new(message)
                .style(fg(THEME.warn))
                .alignment(Alignment::Center),
            area.centered_vertically(Constraint::Length(2)),
        );
        return;
    }

    let [top, main, footer] = Layout::vertical([
        Constraint::Length((area.height / 4).clamp(6, 12)),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);
    let [qps, conn] =
        Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)]).areas(top);

    draw_qps(f, app, qps);
    draw_conn(f, app, conn);
    draw_table(f, app, main);
    draw_footer(f, app, footer);
    draw_detail(f, app, area);
}

fn draw_qps(f: &mut Frame, app: &App, area: Rect) {
    let max = axis_max(&app.qps, area.width.saturating_sub(2), 10.0);
    let now = app.qps.last().map_or("-".to_string(), |&q| fmt_count(q));
    let label = app.sample.as_ref().map_or("qps", |s| s.rate_label);
    let block = panel(tag(vec![
        Span::styled(format!(" {label} "), fg(THEME.accent).bold()),
        Span::styled(format!("{now}/s "), fg(THEME.text)),
    ]))
    .title_top(
        Line::from(tag(vec![Span::styled(
            format!(" max {} ", fmt_count(max)),
            fg(THEME.dim),
        )]))
        .right_aligned(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        BrailleGraph {
            data: visible(&app.qps, inner.width),
            max,
            gradient: THEME.cool,
        },
        inner,
    );
}

fn draw_conn(f: &mut Frame, app: &App, area: Rect) {
    let block = panel(tag(vec![Span::styled(
        " connections ",
        fg(THEME.accent).bold(),
    )]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(sample) = &app.sample else { return };

    let [conn_bar, run_bar, history] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    let connected = f64::from(sample.threads_connected);
    let running = f64::from(sample.threads_running);
    let max = f64::from(sample.max_connections);
    f.render_widget(
        Paragraph::new(gauge(
            "conn",
            connected,
            max,
            format!("{}/{}", connected as u64, max as u64),
            conn_bar.width,
        )),
        conn_bar,
    );
    // The server's running count can exceed its connection count: it includes its own
    // background threads, and dbtop's own session. So no "x/y" here, and the bar never overflows.
    f.render_widget(
        Paragraph::new(gauge(
            "run",
            running,
            connected.max(running).max(1.0),
            (running as u64).to_string(),
            run_bar.width,
        )),
        run_bar,
    );
    f.render_widget(
        BrailleGraph {
            data: visible(&app.threads, history.width),
            max: axis_max(&app.threads, history.width, 4.0),
            gradient: THEME.heat,
        },
        history,
    );
}

/// `label ████░░░░ 12/200`, the fill running from green to red along its length.
fn gauge(label: &str, value: f64, max: f64, shown: String, width: u16) -> Line<'static> {
    let label = format!("{label:<5}");
    let count = format!(" {shown}");
    let bar = usize::from(width).saturating_sub(label.width() + count.width());
    let filled = if max > 0.0 {
        ((value / max).clamp(0.0, 1.0) * bar as f64).round() as usize
    } else {
        0
    };

    let mut spans = vec![Span::styled(label, fg(THEME.dim))];
    spans.extend((0..bar).map(|i| {
        if i < filled {
            Span::styled("█", fg(THEME.heat.at(i as f32 / bar as f32)))
        } else {
            Span::styled("░", fg(THEME.border))
        }
    }));
    spans.push(Span::styled(count, fg(THEME.text)));
    Line::from(spans)
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let counts = app
        .sample
        .as_ref()
        .map_or((0, 0), |s| (s.running.len(), s.slow.len()));
    let tab = |key: &str, name: &str, n: usize, active: bool| {
        let style = if active {
            fg(THEME.accent).bold()
        } else {
            fg(THEME.dim)
        };
        tag(vec![Span::styled(format!(" {key} {name} {n} "), style)])
    };
    let block = panel(
        [
            tab("1", "running", counts.0, app.tab == Tab::Running),
            tab("2", "slow", counts.1, app.tab == Tab::Slow),
        ]
        .concat(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let App {
        sample,
        tab,
        running_sel,
        slow_sel,
        ..
    } = app;
    let Some(sample) = sample.as_ref() else {
        return note(f, inner, "connecting…");
    };
    match tab {
        Tab::Running => {
            let table = match &sample.running_note {
                Some(note) => {
                    let [banner, table] =
                        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
                    f.render_widget(
                        Paragraph::new(format!("⚠ {note}")).style(fg(THEME.warn)),
                        banner,
                    );
                    table
                }
                None => inner,
            };
            running_table(f, table, sample, &mut running_sel.state);
        }
        Tab::Slow => slow_table(f, inner, sample, &mut slow_sel.state),
    }
}

fn note(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(text)
            .style(fg(THEME.dim))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn header<'a>(columns: impl IntoIterator<Item = &'a str>) -> Row<'a> {
    Row::new(columns).style(fg(THEME.dim).bold())
}

fn selected() -> Style {
    Style::new().bg(THEME.selection).bold()
}

fn running_table(f: &mut Frame, area: Rect, sample: &Sample, state: &mut TableState) {
    if sample.running.is_empty() {
        return note(f, area, "no running queries");
    }
    let rows = sample.running.iter().map(|q| {
        let age = match q.secs {
            s if s >= 60.0 => THEME.crit,
            s if s >= 10.0 => THEME.warn,
            s if s >= 1.0 => THEME.text,
            _ => THEME.dim,
        };
        Row::new([
            Cell::from(q.id.to_string()),
            Cell::from(q.user.as_str()),
            Cell::from(q.db.as_str()),
            Cell::from(fmt_secs(q.secs)).style(fg(age)),
            Cell::from(q.state.as_str()),
            Cell::from(one_line(&q.sql)),
        ])
    });
    let widths = [
        Constraint::Length(7),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Length(7),
        Constraint::Length(20),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header(["ID", "USER", "DB", "TIME", "STATE", "QUERY"]))
        .row_highlight_style(selected())
        .column_spacing(2);
    f.render_stateful_widget(table, area, state);
}

fn slow_table(f: &mut Frame, area: Rect, sample: &Sample, state: &mut TableState) {
    if sample.slow.is_empty() {
        return note(
            f,
            area,
            sample
                .slow_note
                .as_deref()
                .unwrap_or("no slow queries recorded"),
        );
    }
    let rows = sample.slow.iter().map(|q| {
        let avg = match q.avg_ms {
            ms if ms >= 1000.0 => THEME.crit,
            ms if ms >= 100.0 => THEME.warn,
            _ => THEME.text,
        };
        Row::new([
            Cell::from(q.schema.as_str()),
            Cell::from(fmt_count(q.calls as f64)),
            Cell::from(fmt_ms(q.total_ms)),
            Cell::from(fmt_ms(q.avg_ms)).style(fg(avg)),
            Cell::from(fmt_ms(q.max_ms)),
            Cell::from(q.rows_examined_avg.map_or("-".to_string(), fmt_count)),
            Cell::from(if q.no_index_used == Some(true) {
                "yes"
            } else {
                ""
            })
            .style(fg(THEME.crit)),
            Cell::from(one_line(&q.sample_sql)),
        ])
    });
    let widths = [
        Constraint::Length(12),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(5),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header([
            "DB",
            "CALLS",
            "TOTAL",
            "AVG",
            "MAX",
            "ROWS EXAM",
            "NOIDX",
            "QUERY",
        ]))
        .row_highlight_style(selected())
        .column_spacing(2);
    f.render_stateful_widget(table, area, state);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let key = |k: &'static str| Span::styled(k, fg(THEME.accent).bold());
    let hint = |t: &'static str| Span::styled(t, fg(THEME.dim));
    let hints = Line::from(vec![
        key(" q"),
        hint(" quit   "),
        key("Tab"),
        hint(" switch   "),
        key("↑↓"),
        hint(" move   "),
        key("Enter"),
        hint(" details"),
    ]);
    let status = match (&app.error, app.updated) {
        (Some(e), _) => Line::styled(format!("poll failed: {} ", one_line(e)), fg(THEME.crit)),
        (None, Some(at)) => {
            let mut spans = vec![Span::styled(app.label.clone(), fg(THEME.dim))];
            // What the server reports, so a `prefer` that fell back to plain text shows.
            match app.sample.as_ref().and_then(|s| s.encrypted) {
                Some(true) => spans.push(Span::styled(" · tls", fg(THEME.dim))),
                Some(false) => spans.push(Span::styled(" · unencrypted", fg(THEME.warn))),
                None => {}
            }
            spans.push(Span::styled(
                format!(" · {:.1}s ago ", at.elapsed().as_secs_f64()),
                fg(THEME.dim),
            ));
            Line::from(spans)
        }
        (None, None) => Line::styled(format!("{} · connecting… ", app.label), fg(THEME.dim)),
    };
    let [left, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::try_from(status.width()).unwrap_or(u16::MAX)),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(hints), left);
    f.render_widget(Paragraph::new(status), right);
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(detail) = app.detail.as_mut() else {
        return;
    };

    let popup = Rect {
        x: area.x + area.width / 20,
        y: area.y + area.height / 20,
        width: area.width - area.width / 10,
        height: area.height - area.height / 10,
    };
    // Dim what's behind, so the table edges peeking out beside the popup don't read as content.
    f.buffer_mut()
        .set_style(area, Style::new().add_modifier(Modifier::DIM));
    f.render_widget(Clear, popup);

    let title = match &detail.subject {
        Subject::Running { query, .. } => format!(" query #{} ", query.id),
        Subject::Slow(slow) => format!(
            " digest {} ",
            slow.digest.chars().take(12).collect::<String>()
        ),
    };
    let block = panel(tag(vec![Span::styled(title, fg(THEME.accent).bold())]))
        .border_style(fg(THEME.accent));
    let inner = block.inner(popup).inner(Margin::new(1, 0));

    let lines = detail_lines(detail, usize::from(inner.width));
    let max_scroll = lines.len().saturating_sub(usize::from(inner.height));
    detail.scroll = detail.scroll.min(max_scroll);

    let mut hints = " ↑↓ PgUp PgDn scroll   g/G top/end   Esc close ".to_string();
    if max_scroll > 0 {
        let first = detail.scroll + 1;
        let last = (detail.scroll + usize::from(inner.height)).min(lines.len());
        hints = format!(" lines {first}-{last} of {} │{hints}", lines.len());
    }
    let block = block.title_bottom(Line::from(tag(vec![Span::styled(hints, fg(THEME.dim))])));

    f.render_widget(block, popup);
    let scroll = u16::try_from(detail.scroll).unwrap_or(u16::MAX);
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

fn detail_lines(detail: &Detail, width: usize) -> Vec<Line<'static>> {
    let label = fg(THEME.dim);
    let value = fg(THEME.text);
    let mut out: Vec<Line<'static>> = Vec::new();

    if let Subject::Running { finished: true, .. } = &detail.subject {
        out.push(Line::styled(
            "finished: this statement is no longer running",
            fg(THEME.warn),
        ));
    }
    let info = info_rows(&detail.subject);
    let key_width = info.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in info {
        out.push(Line::from(vec![
            Span::styled(format!("{k:<key_width$}  "), label),
            Span::styled(v, value),
        ]));
    }

    out.push(Line::raw(""));
    out.push(section("SQL", width));
    let (sql, truncated) = match &detail.subject {
        Subject::Running { query, .. } => (query.sql.as_str(), query.truncated),
        Subject::Slow(slow) => (slow.sample_sql.as_str(), slow.truncated),
    };
    if truncated {
        out.push(Line::styled(
            "⚠ the server cut this text; it is not the full statement",
            fg(THEME.warn),
        ));
    }
    out.extend(wrap(sql, width).into_iter().map(|l| Line::styled(l, value)));

    out.push(Line::raw(""));
    out.push(section("Plan", width));
    // Plans keep their layout (a tree's indentation); sentences wrap at spaces.
    let styled = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap(text, width)
            .into_iter()
            .map(|l| Line::styled(l, style))
            .collect()
    };
    let prose = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap_words(text, width)
            .into_iter()
            .map(|l| Line::styled(l, style))
            .collect()
    };
    match &detail.plan {
        PlanState::Loading => out.push(Line::styled("fetching plan…", label)),
        PlanState::Ready(Plan::Text(text)) => out.extend(styled(text, value)),
        PlanState::Ready(Plan::Estimated { text, why }) => {
            out.extend(prose(&format!("estimated: {why}"), fg(THEME.warn)));
            out.push(Line::raw(""));
            out.extend(styled(text, value));
        }
        PlanState::Ready(Plan::Unavailable(why)) => {
            out.extend(prose(&format!("not available: {why}"), fg(THEME.warn)));
        }
        PlanState::Failed(e) => out.extend(prose(&format!("EXPLAIN failed: {e}"), fg(THEME.crit))),
    }
    out
}

fn section(name: &str, width: usize) -> Line<'static> {
    let head = format!("── {name} ");
    let rule = "─".repeat(width.saturating_sub(head.chars().count()));
    Line::styled(format!("{head}{rule}"), fg(THEME.accent))
}

fn info_rows(subject: &Subject) -> Vec<(String, String)> {
    match subject {
        Subject::Running { query: q, .. } => {
            let mut rows = vec![
                ("user".to_string(), format!("{}@{}", q.user, q.host)),
                ("db".to_string(), q.db.clone()),
                ("state".to_string(), q.state.clone()),
                ("time".to_string(), fmt_secs(q.secs)),
            ];
            rows.extend(q.details.iter().cloned());
            rows
        }
        Subject::Slow(s) => vec![
            ("schema".to_string(), s.schema.clone()),
            ("digest".to_string(), s.digest.clone()),
            ("calls".to_string(), s.calls.to_string()),
            ("total time".to_string(), fmt_ms(s.total_ms)),
            (
                "avg / max".to_string(),
                format!("{} / {}", fmt_ms(s.avg_ms), fmt_ms(s.max_ms)),
            ),
            (
                "rows examined".to_string(),
                s.rows_examined_avg
                    .map_or("n/a".to_string(), |n| format!("{} per call", fmt_count(n))),
            ),
            (
                "no index used".to_string(),
                match s.no_index_used {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "n/a",
                }
                .to_string(),
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent},
    };

    use super::*;
    use crate::backend::{Backend, fake::Fake};
    use crate::collector::{Msg, Req};
    use crate::model::SlowQuery;

    fn loaded() -> (App, Fake) {
        let mut fake = Fake::seeded(3);
        let mut app = App::new("test".into());
        for i in 0..40 {
            let sample = fake.poll().unwrap();
            app.on_msg(Msg::Poll {
                sample,
                qps: Some(500.0 + 20.0 * f64::from(i)),
            });
        }
        (app, fake)
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().to_string()
    }

    fn press(app: &mut App, code: KeyCode) -> mpsc::Receiver<Req> {
        let (tx, rx) = mpsc::channel();
        app.on_key(KeyEvent::from(code), &tx);
        rx
    }

    #[test]
    fn draws_every_panel() {
        let (mut app, _) = loaded();
        let screen = screen(&mut app, 120, 40);
        for expected in [
            "qps",
            "connections",
            "1 running",
            "2 slow",
            "USER",
            "QUERY",
            "SELECT",
            "Enter",
        ] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in:\n{screen}"
            );
        }
        assert!(
            screen.contains('⣿') || screen.contains('⣀'),
            "no braille graph in:\n{screen}"
        );
    }

    #[test]
    fn draws_before_the_first_poll() {
        let mut app = App::new("test".into());
        assert!(screen(&mut app, 100, 30).contains("connecting"));
    }

    #[test]
    fn slow_tab_lists_digests() {
        let (mut app, _) = loaded();
        press(&mut app, KeyCode::Char('2'));
        let screen = screen(&mut app, 140, 40);
        for expected in ["CALLS", "NOIDX", "ROWS EXAM", "notifications"] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in:\n{screen}"
            );
        }
    }

    #[test]
    fn survives_every_terminal_size_with_and_without_details() {
        for with_detail in [false, true] {
            for (w, h) in [
                (1, 1),
                (3, 2),
                (10, 3),
                (40, 10),
                (80, 24),
                (200, 60),
                (300, 8),
            ] {
                let (mut app, _) = loaded();
                if with_detail {
                    press(&mut app, KeyCode::Enter);
                }
                screen(&mut app, w, h);
            }
        }
    }

    #[test]
    fn details_show_info_sql_and_plan() {
        let (mut app, mut fake) = loaded();
        let reqs = press(&mut app, KeyCode::Enter);

        let Some(Req::Explain(target)) = reqs.try_recv().ok() else {
            panic!("no plan requested");
        };
        let before = screen(&mut app, 120, 40);
        assert!(before.contains("fetching plan"), "{before}");

        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Ok(fake.explain(&target).unwrap()),
        });
        let after = screen(&mut app, 120, 40);
        for expected in [
            "rows_examined",
            "── SQL",
            "SELECT DATE(created_at)",
            "── Plan",
            "Aggregate using temporary table",
        ] {
            assert!(
                after.contains(expected),
                "missing {expected:?} in:\n{after}"
            );
        }
    }

    #[test]
    fn unavailable_and_failed_plans_say_why() {
        let (mut app, _) = loaded();
        let reqs = press(&mut app, KeyCode::Enter);
        let Some(Req::Explain(target)) = reqs.try_recv().ok() else {
            panic!("no plan requested");
        };

        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Ok(Plan::Unavailable("text was cut".into())),
        });
        assert!(screen(&mut app, 120, 40).contains("not available: text was cut"));

        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Err("access denied".into()),
        });
        assert!(screen(&mut app, 120, 40).contains("EXPLAIN failed: access denied"));
    }

    #[test]
    fn too_small_a_terminal_says_so_instead_of_squeezing_numbers() {
        let (mut app, _) = loaded();
        let small = screen(&mut app, MIN_WIDTH - 1, 40);
        assert!(
            small.contains("terminal too small") && small.contains("80x20"),
            "{small}"
        );
        let short = screen(&mut app, 120, MIN_HEIGHT - 1);
        assert!(short.contains("terminal too small"), "{short}");
        assert!(!screen(&mut app, MIN_WIDTH, MIN_HEIGHT).contains("too small"));
    }

    #[test]
    fn the_dashboard_behind_a_popup_is_dimmed() {
        let (mut app, _) = loaded();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(
            !terminal.backend().buffer()[(0, 0)]
                .modifier
                .contains(Modifier::DIM)
        );

        press(&mut app, KeyCode::Enter);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer[(0, 0)].modifier.contains(Modifier::DIM),
            "outside the popup"
        );
        assert!(
            !buffer[(60, 20)].modifier.contains(Modifier::DIM),
            "inside the popup"
        );
    }

    #[test]
    fn the_footer_says_whether_the_connection_is_encrypted() {
        let (mut app, mut fake) = loaded();
        let last_line = |app: &mut App| screen(app, 120, 40).lines().last().unwrap().to_string();

        for (encrypted, shown, hidden) in [
            (Some(true), " · tls", "unencrypted"),
            (Some(false), " · unencrypted", " · tls"),
            (None, "test", "unencrypted"),
        ] {
            let mut sample = fake.poll().unwrap();
            sample.encrypted = encrypted;
            app.on_msg(Msg::Poll { sample, qps: None });
            let footer = last_line(&mut app);
            assert!(
                footer.contains(shown) && !footer.contains(hidden),
                "{encrypted:?}: {footer}"
            );
        }
    }

    #[test]
    fn a_hidden_sessions_warning_sits_above_the_running_list() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        sample.running_note =
            Some("this role cannot see other roles' queries: grant pg_read_all_stats".into());
        app.on_msg(Msg::Poll { sample, qps: None });

        let screen = screen(&mut app, 120, 40);
        let lines: Vec<_> = screen.lines().collect();
        let warning = lines
            .iter()
            .position(|l| l.contains("⚠ this role cannot see"))
            .expect("banner");
        let header = lines
            .iter()
            .position(|l| l.contains("USER") && l.contains("QUERY"))
            .expect("table header");
        assert!(warning < header, "{screen}");
    }

    #[test]
    fn an_estimated_plan_is_labelled_as_one() {
        let (mut app, _) = loaded();
        let reqs = press(&mut app, KeyCode::Enter);
        let Some(Req::Explain(target)) = reqs.try_recv().ok() else {
            panic!("no plan requested");
        };
        let plan = Plan::Estimated {
            text: "-> Table scan on orders".into(),
            why: "refused (ERROR 1045)".into(),
        };
        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Ok(plan),
        });

        let screen = screen(&mut app, 120, 40);
        assert!(
            screen.contains("estimated: refused (ERROR 1045)"),
            "{screen}"
        );
        assert!(screen.contains("Table scan on orders"), "{screen}");
    }

    #[test]
    fn long_details_scroll_to_the_end() {
        let (mut app, _) = loaded();
        press(&mut app, KeyCode::Enter);
        let top = screen(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(top.contains("lines 1-"), "{top}");

        press(&mut app, KeyCode::End);
        let end = screen(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(!end.contains("lines 1-"), "{end}");
        assert!(
            end.contains("fetching plan"),
            "the last line should be visible:\n{end}"
        );
    }

    #[test]
    fn slow_details_show_stats_sql_and_plan() {
        let (mut app, mut fake) = loaded();
        press(&mut app, KeyCode::Char('2'));
        let reqs = press(&mut app, KeyCode::Enter);
        let Some(Req::Explain(target)) = reqs.try_recv().ok() else {
            panic!("no plan requested");
        };
        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Ok(fake.explain(&target).unwrap()),
        });

        let screen = screen(&mut app, 120, 40);
        for expected in [
            "digest",
            "calls",
            "avg / max",
            "── SQL",
            "FROM notifications",
            "── Plan",
            "Index lookup on notifications",
        ] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in:\n{screen}"
            );
        }
    }

    fn bare_digest() -> SlowQuery {
        SlowQuery {
            digest: "abc".into(),
            schema: "app".into(),
            sample_sql: "SELECT $1".into(),
            truncated: true,
            calls: 3,
            total_ms: 30.0,
            avg_ms: 10.0,
            max_ms: 20.0,
            rows_examined_avg: None,
            no_index_used: None,
        }
    }

    #[test]
    fn untracked_columns_show_as_unavailable_not_as_zero() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        sample.slow = vec![bare_digest()];
        app.on_msg(Msg::Poll { sample, qps: None });

        press(&mut app, KeyCode::Char('2'));
        let table = screen(&mut app, 120, 40);
        let row = table
            .lines()
            .find(|l| l.contains("SELECT $1"))
            .expect("digest row");
        assert!(
            !row.contains("yes"),
            "no_index_used is unknown, not true: {row}"
        );
        assert!(row.contains(" - "), "rows examined is unknown: {row}");

        press(&mut app, KeyCode::Enter);
        let detail = screen(&mut app, 120, 40);
        assert!(detail.contains("n/a"), "{detail}");
        assert!(detail.contains("the server cut this text"), "{detail}");
    }

    #[test]
    fn an_empty_slow_list_explains_itself() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        sample.slow.clear();
        sample.slow_note = Some("performance_schema is OFF on this server".into());
        app.on_msg(Msg::Poll { sample, qps: None });

        press(&mut app, KeyCode::Char('2'));
        assert!(screen(&mut app, 120, 40).contains("performance_schema is OFF"));
    }

    #[test]
    fn the_throughput_graph_is_named_after_what_it_counts() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        sample.rate_label = "tps";
        app.on_msg(Msg::Poll {
            sample,
            qps: Some(10.0),
        });
        let screen = screen(&mut app, 120, 40);
        assert!(
            screen.contains(" tps ") && !screen.contains(" qps "),
            "{screen}"
        );
    }

    #[test]
    fn a_truncated_statement_is_flagged() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        let mut cut = sample.running[0].clone();
        cut.id = 4242;
        cut.truncated = true;
        cut.secs = 5000.0; // sorts to the top
        sample.running.push(cut);
        app.on_msg(Msg::Poll { sample, qps: None });

        // The cursor keeps following the row it was on, so go to the top explicitly.
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Enter);
        assert!(screen(&mut app, 120, 40).contains("the server cut this text"));
    }
}
