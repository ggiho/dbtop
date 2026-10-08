mod graph;
pub(crate) mod metrics;
mod plan_tree;
mod sql;
mod text;
mod theme;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Detail, PlanState, Subject, Tab};
use crate::model::{Plan, PlanTable, PlanViews, Sample};
use graph::{BrailleGraph, axis_max, visible};
use metrics::{Spec, spec};
use sql::highlight;
use text::{fmt_count, fmt_ms, fmt_secs, one_line, printable, wrap_words};
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

    let (cols, max_rows) = grid(area);
    let slots = slots(app, cols * max_rows);
    let rows = slots.len().div_ceil(cols).clamp(1, max_rows);
    // The graphs are what one looks at first: about half the screen, the table gets the rest.
    let top_height = match rows {
        1 => (area.height * 2 / 5).clamp(8, 20),
        2 => (area.height * 9 / 20).clamp(14, 30),
        _ => (area.height / 2).clamp(21, 40),
    };
    let [top, main, footer] = Layout::vertical([
        Constraint::Length(top_height),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_dashboard(f, app, top, &slots, cols);
    draw_table(f, app, main);
    draw_footer(f, app, footer);
    draw_detail(f, app, area);
}

/// Panels across and down for a terminal of this size. Each wants about 32 columns, and each row
/// after the first only comes with a taller terminal, so that the table below keeps its room.
fn grid(area: Rect) -> (usize, usize) {
    let cols = usize::from(area.width / 32).clamp(2, 4);
    let rows = match area.height {
        60.. => 3,
        40.. => 2,
        _ => 1,
    };
    (cols, rows)
}

enum Slot<'a> {
    Metric(&'a str),
    /// Two metrics that tell one story, one above the other in a single panel.
    Pair(&'a str, &'a str),
    Connections,
}

/// What to draw, best first. The throughput chart leads and the connections panel comes second;
/// whatever the backend reports after that fills the rest while there is room.
fn slots(app: &App, capacity: usize) -> Vec<Slot<'_>> {
    let mut keys: Vec<&str> = match &app.sample {
        Some(sample) => sample
            .metrics
            .iter()
            .map(|m| m.key)
            .filter(|key| spec(key).is_some())
            .collect(),
        None => vec!["qps"],
    };
    let mut slots = Vec::new();
    if !keys.is_empty() {
        slots.push(Slot::Metric(keys.remove(0)));
    }
    slots.push(Slot::Connections);
    // The oldest open transaction and the history list it lets pile up tell one story and flow
    // alike: one panel for both leaves a place for another metric.
    let paired = keys.contains(&"oldest_xact") && keys.contains(&"hll");
    slots.extend(keys.into_iter().filter_map(|key| match key {
        "oldest_xact" if paired => Some(Slot::Pair("oldest_xact", "hll")),
        "hll" if paired => None,
        key => Some(Slot::Metric(key)),
    }));
    slots.truncate(capacity);
    slots
}

fn draw_dashboard(f: &mut Frame, app: &App, area: Rect, slots: &[Slot], cols: usize) {
    let rows = slots.len().div_ceil(cols).max(1);
    // Spread evenly over the rows: nine panels are 3 + 3 + 3, not 4 + 4 and a lone one stretched
    // across the whole width.
    let per_row = slots.len().div_ceil(rows).max(1);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
    for (row, row_area) in slots.chunks(per_row).zip(row_areas.iter()) {
        // A short last row spreads out rather than leaving a hole.
        let cells = Layout::horizontal(vec![Constraint::Ratio(1, row.len() as u32); row.len()])
            .split(*row_area);
        for (slot, cell) in row.iter().zip(cells.iter()) {
            match slot {
                Slot::Metric(key) => draw_metric(f, app, *cell, key),
                Slot::Pair(upper, lower) => draw_pair(f, app, *cell, upper, lower),
                Slot::Connections => draw_conn(f, app, *cell),
            }
        }
    }
}

/// One metric's history, and how to caption its chart.
struct Series<'a> {
    spec: &'static Spec,
    history: &'a [f64],
}

impl<'a> Series<'a> {
    fn of(app: &'a App, key: &str) -> Option<Self> {
        Some(Series {
            spec: spec(key)?,
            history: app.history.get(key).map_or(&[][..], Vec::as_slice),
        })
    }

    /// The top of the y axis for a chart `width` cells wide.
    fn max(&self, width: u16) -> f64 {
        self.spec
            .fixed_max
            .unwrap_or_else(|| axis_max(self.history, width, self.spec.floor))
    }

    /// The title set into a border: the latest poll's number, coloured when it needs a look, or
    /// a dash, since an older number would pass for current.
    fn title(&self) -> Vec<Span<'static>> {
        let (shown, tone) = match self.history.last().filter(|v| v.is_finite()) {
            Some(&value) => (self.spec.format(value), self.spec.tone(value)),
            None => ("-".to_string(), THEME.dim),
        };
        tag(vec![
            Span::styled(format!(" {} ", self.spec.title), fg(THEME.accent).bold()),
            Span::styled(format!("{shown} "), fg(tone)),
        ])
    }

    /// The scale for the border's other end. It is a nicety: a narrow panel's title needs the room.
    fn scale(&self, max: f64, panel_width: u16) -> Option<Line<'static>> {
        (self.spec.fixed_max.is_none() && panel_width >= 40).then(|| {
            Line::from(tag(vec![Span::styled(
                format!(" max {} ", self.spec.format(max)),
                fg(THEME.dim),
            )]))
            .right_aligned()
        })
    }

    fn graph(&self, width: u16, max: f64) -> BrailleGraph<'a> {
        BrailleGraph {
            data: visible(self.history, width),
            max,
            gradient: self.spec.gradient(),
        }
    }
}

/// A chart with its latest value in the title, coloured when the value needs a look.
fn draw_metric(f: &mut Frame, app: &App, area: Rect, key: &str) {
    let Some(series) = Series::of(app, key) else {
        return;
    };
    let max = series.max(area.width.saturating_sub(2));
    let mut block = panel(series.title());
    if let Some(scale) = series.scale(max, area.width) {
        block = block.title_top(scale);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(series.graph(inner.width, max), inner);
}

/// Two charts in one frame, one above the other, with a rule between them that carries the
/// second one's title.
fn draw_pair(f: &mut Frame, app: &App, area: Rect, upper: &str, lower: &str) {
    let (Some(upper_series), Some(lower_series)) = (Series::of(app, upper), Series::of(app, lower))
    else {
        return;
    };
    // Three lines of chart each and the rule between them, inside the frame: 2 + 3 + 1 + 3.
    if area.height < 9 {
        draw_metric(f, app, area, upper);
        return;
    }
    let width = area.width.saturating_sub(2);
    let (upper_max, lower_max) = (upper_series.max(width), lower_series.max(width));
    let mut frame = panel(upper_series.title());
    if let Some(scale) = upper_series.scale(upper_max, area.width) {
        frame = frame.title_top(scale);
    }
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let upper_height = (inner.height - 1) / 2;
    let upper_area = Rect {
        height: upper_height,
        ..inner
    };
    let rule = Rect {
        x: area.x,
        y: inner.y + upper_height,
        width: area.width,
        height: 1,
    };
    let lower_area = Rect {
        y: rule.y + 1,
        height: inner.height - upper_height - 1,
        ..inner
    };

    // The rule is joined to the frame's sides, and its title sits in it like the frame's own.
    let mut set = BorderType::Rounded.to_border_set();
    set.top_left = "├";
    set.top_right = "┤";
    let mut divider = Block::new()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_set(set)
        .border_style(fg(THEME.border))
        .title_top(Line::from(lower_series.title()));
    if let Some(scale) = lower_series.scale(lower_max, area.width) {
        divider = divider.title_top(scale);
    }
    f.render_widget(divider, rule);
    f.render_widget(upper_series.graph(inner.width, upper_max), upper_area);
    f.render_widget(lower_series.graph(inner.width, lower_max), lower_area);
}

fn draw_conn(f: &mut Frame, app: &App, area: Rect) {
    let block = panel(tag(vec![Span::styled(
        " Connections ",
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
            "Conn",
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
            "Run",
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
    out.extend(highlight(sql, width));

    out.push(Line::raw(""));
    out.push(section("Plan", width));
    // Sentences wrap at spaces; a plan keeps its layout.
    let prose = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap_words(text, width)
            .into_iter()
            .map(|l| Line::styled(l, style))
            .collect()
    };
    match &detail.plan {
        PlanState::Loading => out.push(Line::styled("fetching plan…", label)),
        PlanState::Ready(Plan::Text(views)) => out.extend(plan_lines(views, width)),
        PlanState::Ready(Plan::Estimated { text: views, why }) => {
            out.extend(prose(&format!("estimated: {why}"), fg(THEME.warn)));
            out.push(Line::raw(""));
            out.extend(plan_lines(views, width));
        }
        PlanState::Ready(Plan::Unavailable(why)) => {
            out.extend(prose(&format!("not available: {why}"), fg(THEME.warn)));
        }
        PlanState::Failed(e) => out.extend(prose(&format!("EXPLAIN failed: {e}"), fg(THEME.crit))),
    }
    out
}

/// The plan in each form the server drew it. With two, each gets a heading.
fn plan_lines(views: &PlanViews, width: usize) -> Vec<Line<'static>> {
    let label = fg(THEME.dim);
    let headed = views.tree.is_some() && views.table.is_some();
    let mut out = Vec::new();
    if let Some(tree) = &views.tree {
        if headed {
            out.push(Line::styled("tree", label));
        }
        out.extend(plan_tree::lines(tree, width));
    }
    if let Some(table) = &views.table {
        if headed {
            out.push(Line::raw(""));
            out.push(Line::styled("table", label));
        }
        out.extend(table_lines(table, width));
    }
    out
}

/// The classic `EXPLAIN` table, boxed the way the `mysql` client draws it, and fitted to `width`.
/// Columns that are NULL all the way down are the first to go; then the last one, which holds the
/// longest text (`Extra`), wraps under itself; a table that still does not fit is shown a row at a
/// time.
fn table_lines(table: &PlanTable, width: usize) -> Vec<Line<'static>> {
    // Each column also takes a border and a space either side of its text, and the box one more
    // border at its left.
    const AROUND: usize = 3;
    const NARROWEST_LAST: usize = 12;
    let (label, value, edge) = (fg(THEME.dim), fg(THEME.text), fg(THEME.border));

    // The text came from the server: no control character may reach the terminal.
    let clean = |text: &String| text.chars().map(printable).collect::<String>();
    let table = PlanTable {
        header: table.header.iter().map(clean).collect(),
        rows: table
            .rows
            .iter()
            .map(|row| row.iter().map(clean).collect())
            .collect(),
    };
    let cell = |row: &Vec<String>, c: usize| row.get(c).cloned().unwrap_or_default();

    let natural: Vec<usize> = (0..table.header.len())
        .map(|c| {
            (table.rows.iter().map(|row| cell(row, c).width()))
                .chain([table.header[c].width()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let boxed = |cols: &[usize]| 1 + cols.iter().map(|&c| natural[c] + AROUND).sum::<usize>();

    let mut cols: Vec<usize> = (0..table.header.len()).collect();
    if boxed(&cols) > width {
        cols.retain(|&c| table.rows.iter().any(|row| cell(row, c) != "NULL"));
    }
    let Some((&last, head)) = cols.split_last() else {
        return Vec::new();
    };
    // The last column takes what the others leave.
    let room = width.saturating_sub(boxed(head) + AROUND);
    if room < natural[last].min(NARROWEST_LAST) {
        return rows_one_at_a_time(&table, width);
    }
    let last_width = natural[last].min(room);
    let widths: Vec<usize> = head
        .iter()
        .map(|&c| natural[c])
        .chain([last_width])
        .collect();

    let rule = |left: &str, join: &str, right: &str| {
        let bars: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line::styled(format!("{left}{}{right}", bars.join(join)), edge)
    };
    let line = |cells: Vec<String>, style: Style| {
        let mut spans = vec![Span::styled("│", edge)];
        for (text, &w) in cells.iter().zip(&widths) {
            let pad = " ".repeat(w.saturating_sub(text.width()));
            spans.push(Span::styled(format!(" {text}{pad} "), style));
            spans.push(Span::styled("│", edge));
        }
        Line::from(spans)
    };

    let header_cells = head
        .iter()
        .chain([&last])
        .map(|&c| table.header[c].clone())
        .collect();
    let mut out = vec![
        rule("╭", "┬", "╮"),
        line(header_cells, label),
        rule("├", "┼", "┤"),
    ];
    for row in &table.rows {
        let mut wrapped = wrap_words(&cell(row, last), last_width).into_iter();
        let mut cells: Vec<String> = head.iter().map(|&c| cell(row, c)).collect();
        cells.push(wrapped.next().unwrap_or_default());
        out.push(line(cells, value));
        for rest in wrapped {
            let mut cells = vec![String::new(); head.len()];
            cells.push(rest);
            out.push(line(cells, value));
        }
    }
    out.push(rule("╰", "┴", "╯"));
    out
}

/// A block per row, a line for each column that says something: for a table too wide for the room.
fn rows_one_at_a_time(table: &PlanTable, width: usize) -> Vec<Line<'static>> {
    let (label, value) = (fg(THEME.dim), fg(THEME.text));
    let key = table.header.iter().map(|h| h.width()).max().unwrap_or(0);
    let mut out = Vec::new();
    for (n, row) in table.rows.iter().enumerate() {
        if n > 0 {
            out.push(Line::raw(""));
        }
        for (name, cell) in table.header.iter().zip(row).filter(|(_, c)| *c != "NULL") {
            for (i, part) in wrap_words(cell, width.saturating_sub(key + 2))
                .into_iter()
                .enumerate()
            {
                let name = if i == 0 { name.as_str() } else { "" };
                out.push(Line::from(vec![
                    Span::styled(format!("{name:<key$}  "), label),
                    Span::styled(part, value),
                ]));
            }
        }
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
    use crate::model::{Raw, Reading, Sample, SlowQuery};

    fn loaded() -> (App, Fake) {
        let mut fake = Fake::seeded(3);
        let mut app = App::new("test".into());
        for i in 0..40 {
            let sample = fake.poll().unwrap();
            // Rates need real time to pass; give every chart a rising line instead.
            let values = sample
                .metrics
                .iter()
                .map(|m| {
                    let value = if m.key == "hit" {
                        99.0
                    } else {
                        100.0 + 20.0 * f64::from(i)
                    };
                    (m.key, Some(value))
                })
                .collect();
            app.on_msg(Msg::Poll { sample, values });
        }
        (app, fake)
    }

    /// Sends `sample` with a number for each of its metrics.
    fn send(app: &mut App, sample: Sample) {
        let values = sample.metrics.iter().map(|m| (m.key, Some(50.0))).collect();
        app.on_msg(Msg::Poll { sample, values });
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
            "QPS",
            "Connections",
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

    /// What a line says, without its styles.
    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn explain_table() -> PlanTable {
        let row = |cells: [&str; 8]| cells.map(String::from).to_vec();
        PlanTable {
            header: row([
                "id",
                "select_type",
                "table",
                "partitions",
                "type",
                "key",
                "rows",
                "Extra",
            ]),
            rows: vec![
                row([
                    "1",
                    "SIMPLE",
                    "orders",
                    "NULL",
                    "ALL",
                    "NULL",
                    "812300",
                    "Using where; Using temporary; Using filesort",
                ]),
                row(["1", "SIMPLE", "c", "NULL", "eq_ref", "PRIMARY", "1", "NULL"]),
            ],
        }
    }

    #[test]
    fn the_sql_in_the_details_is_coloured_on_the_screen() {
        let (mut app, _) = loaded();
        press(&mut app, KeyCode::Enter);
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();

        let (x, y) = find(buffer, "SELECT DATE(").expect("the statement is shown");
        assert_eq!(buffer[(x, y)].fg, THEME.keyword, "SELECT is a keyword");
        assert_eq!(buffer[(x + 7, y)].fg, THEME.function, "DATE( is a call");
        assert_eq!(
            buffer[(x + 6, y)].fg,
            THEME.text,
            "the space between is plain"
        );
    }

    #[test]
    fn a_plan_in_two_forms_shows_both_under_a_heading_each() {
        let both = PlanViews {
            tree: Some("-> Table scan on orders".into()),
            table: Some(explain_table()),
        };
        let lines = plain(&plan_lines(&both, 120));
        let at = |text: &str| lines.iter().position(|l| l == text);
        let (tree, table) = (at("tree").unwrap(), at("table").unwrap());
        assert!(lines[tree + 1].starts_with("Table scan"), "{lines:?}");
        assert!(
            tree < table
                && lines[table + 1].starts_with('╭')
                && lines[table + 2].contains("select_type"),
            "{lines:?}"
        );

        // With only one form there is nothing to tell apart: no heading.
        let tree_only = plain(&plan_lines(&"-> Seq Scan".into(), 120));
        assert_eq!(tree_only, ["Seq Scan"]);
        let table_only = PlanViews {
            tree: None,
            table: Some(explain_table()),
        };
        let lines = plain(&plan_lines(&table_only, 120));
        assert!(
            lines[0].starts_with('╭') && !lines.contains(&"table".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn the_details_of_a_running_query_carry_its_tree_and_its_table() {
        let (mut app, mut fake) = loaded();
        let reqs = press(&mut app, KeyCode::Enter);
        let Some(Req::Explain(target)) = reqs.try_recv().ok() else {
            panic!("no plan requested");
        };
        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Ok(fake.explain(&target).unwrap()),
        });
        let lines = plain(&detail_lines(app.detail.as_ref().unwrap(), 140));
        for expected in ["tree", "table"] {
            assert!(
                lines.iter().any(|l| l == expected),
                "no {expected:?} heading: {lines:?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("│ id") && l.contains("select_type")),
            "{lines:?}"
        );
    }

    /// Where the vertical rules of a boxed line are.
    fn rules_in(line: &str) -> Vec<usize> {
        line.chars()
            .enumerate()
            .filter(|(_, c)| matches!(c, '│' | '┬' | '┼' | '┴'))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn the_tree_in_the_details_is_drawn_with_branches_and_not_with_arrows() {
        let (mut app, mut fake) = loaded();
        let reqs = press(&mut app, KeyCode::Enter);
        let Some(Req::Explain(target)) = reqs.try_recv().ok() else {
            panic!("no plan requested");
        };
        app.on_msg(Msg::Plan {
            key: target.key(),
            result: Ok(fake.explain(&target).unwrap()),
        });
        let lines = plain(&detail_lines(app.detail.as_ref().unwrap(), 140));
        let at = lines
            .iter()
            .position(|l| l == "tree")
            .expect("a tree heading");
        let tree: Vec<&String> = lines[at + 1..]
            .iter()
            .take_while(|l| !l.is_empty())
            .collect();
        assert!(tree.len() >= 3, "{lines:?}");
        assert!(
            tree.iter().all(|l| !l.contains("->")),
            "no arrows: {tree:?}"
        );
        assert!(
            tree.iter().skip(1).any(|l| l.contains("└─ ")),
            "branches: {tree:?}"
        );
    }

    #[test]
    fn a_table_that_fits_is_shown_whole_in_a_box() {
        let lines = plain(&table_lines(&explain_table(), 120));
        assert_eq!(
            lines.len(),
            6,
            "top, header, rule, two rows, bottom: {lines:?}"
        );
        assert!(
            lines[0].starts_with('╭') && lines[0].ends_with('╮'),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with('├') && lines[2].ends_with('┤'),
            "{lines:?}"
        );
        assert!(
            lines[5].starts_with('╰') && lines[5].ends_with('╯'),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("partitions") && lines[1].starts_with("│ id "),
            "{lines:?}"
        );
        assert!(
            lines[3].contains("812300") && lines[4].contains("PRIMARY"),
            "{lines:?}"
        );

        // A rectangle, with the columns one under the other all the way down.
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        let columns = rules_in(&lines[1]);
        for row in [&lines[3], &lines[4]] {
            assert_eq!(rules_in(row), columns, "{lines:?}");
        }
    }

    #[test]
    fn a_table_too_wide_loses_the_columns_that_are_null_all_the_way_down_first() {
        let lines = plain(&table_lines(&explain_table(), 110));
        assert_eq!(lines.len(), 6, "still a line a row: {lines:?}");
        assert!(!lines[1].contains("partitions"), "{lines:?}");
        assert!(
            lines[1].contains("select_type") && lines[1].contains("Extra"),
            "{lines:?}"
        );
        assert!(lines.iter().all(|l| l.width() <= 110), "{lines:?}");
    }

    #[test]
    fn then_the_last_column_wraps_under_itself() {
        let lines = plain(&table_lines(&explain_table(), 80));
        assert_eq!(lines.len(), 8, "Extra took three lines: {lines:?}");
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(
            widths.iter().all(|w| *w == 80),
            "the box fills what it was given: {widths:?}"
        );
        // The lines under the first hold nothing but the rest of Extra.
        let words = |line: &str| {
            line.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        };
        assert!(words(&lines[3]).ends_with("UsingwhereUsing"), "{lines:?}");
        assert_eq!(words(&lines[4]), "temporaryUsing", "{lines:?}");
        assert_eq!(words(&lines[5]), "filesort", "{lines:?}");
        // The second row still has its own line, after the wrapped ones.
        assert!(
            lines[6].contains("eq_ref") && lines[6].contains("PRIMARY"),
            "{lines:?}"
        );
    }

    #[test]
    fn control_characters_in_a_plan_table_never_reach_the_screen() {
        let mut table = explain_table();
        table.rows[0][2] = "\x1b[31mevil".into();
        table.header[0] = "i\x07d".into();
        for width in [200, 80, 40] {
            let text = plain(&table_lines(&table, width)).join("\n");
            assert!(
                !text.contains('\x1b') && !text.contains('\x07') && text.contains('\u{FFFD}'),
                "{width}: {text:?}"
            );
        }
    }

    #[test]
    fn a_table_that_cannot_fit_at_all_is_shown_a_row_at_a_time() {
        let lines = plain(&table_lines(&explain_table(), 40));
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let orders = lines
            .iter()
            .position(|l| l.starts_with("table") && l.contains("orders"));
        assert!(orders.is_some(), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("partitions")),
            "NULL fields are left out: {lines:?}"
        );
        assert!(
            lines.contains(&String::new()),
            "a blank line between rows: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("key") && l.contains("PRIMARY")),
            "the second row is there: {lines:?}"
        );
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
            send(&mut app, sample);
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
        send(&mut app, sample);

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
        send(&mut app, sample);

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
        send(&mut app, sample);

        press(&mut app, KeyCode::Char('2'));
        assert!(screen(&mut app, 120, 40).contains("performance_schema is OFF"));
    }

    #[test]
    fn the_throughput_chart_is_named_after_what_is_counted() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        sample.metrics = vec![Reading {
            key: "tps",
            raw: Raw::Counter(1),
        }];
        send(&mut app, sample);
        let screen = screen(&mut app, 120, 40);
        assert!(
            screen.contains(" TPS ") && !screen.contains(" QPS "),
            "{screen}"
        );
    }

    #[test]
    fn the_grid_follows_the_terminal() {
        let cells = |width, height| grid(Rect::new(0, 0, width, height));
        assert_eq!(
            cells(80, 20),
            (2, 1),
            "the smallest terminal keeps the two basic panels"
        );
        assert_eq!(cells(100, 30), (3, 1), "a panel needs about 32 columns");
        assert_eq!(cells(120, 30), (3, 1), "wider means more across");
        assert_eq!(cells(120, 40), (3, 2), "taller means a second row");
        assert_eq!(cells(160, 44), (4, 2));
        assert_eq!(cells(160, 60), (4, 3), "and a third");
        assert_eq!(cells(400, 100), (4, 3), "and no more than that");
    }

    #[test]
    fn the_more_room_the_more_metrics_are_shown() {
        let (mut app, _) = loaded();
        let names = [
            "QPS",
            "Connections",
            "Slow",
            "Buffer Pool Hit",
            "Lock Waits",
            "Deadlocks",
            "Oldest Xact",
            "History List",
            "Net",
            "Tmp Disk",
        ];
        let shown = |app: &mut App, width, height| {
            let screen = screen(app, width, height);
            // Only the dashboard: "slow" is also the name of a tab of the table below it.
            let dashboard: String = screen
                .lines()
                .take_while(|l| !l.contains("1 running"))
                .collect();
            names
                .iter()
                .filter(|name| dashboard.contains(&format!(" {name} ")))
                .count()
        };
        assert_eq!(shown(&mut app, 80, 24), 2, "QPS and Connections");
        assert_eq!(shown(&mut app, 120, 30), 3, "one row of three");
        assert_eq!(shown(&mut app, 120, 40), 6, "two rows of three");
        assert_eq!(
            shown(&mut app, 160, 44),
            9,
            "two rows of four, one of the eight holding two metrics, which leaves room for net"
        );
        assert_eq!(
            shown(&mut app, 160, 60),
            10,
            "three rows of four hold everything the demo reports"
        );
    }

    /// What `slots` decided, spelled out.
    fn plan(app: &App, capacity: usize) -> Vec<String> {
        slots(app, capacity)
            .iter()
            .map(|slot| match slot {
                Slot::Metric(key) => key.to_string(),
                Slot::Pair(upper, lower) => format!("{upper}+{lower}"),
                Slot::Connections => "connections".to_string(),
            })
            .collect()
    }

    #[test]
    fn the_oldest_transaction_and_the_history_list_share_a_panel_when_both_exist() {
        let (mut app, mut fake) = loaded();
        assert_eq!(
            plan(&app, 12),
            [
                "qps",
                "connections",
                "slow",
                "hit",
                "lock_waits",
                "deadlocks",
                "oldest_xact+hll",
                "net",
                "tmp_disk"
            ]
        );

        // Postgres has no history list: the age keeps a panel to itself.
        let mut sample = fake.poll().unwrap();
        sample.metrics.retain(|m| m.key != "hll");
        send(&mut app, sample);
        let alone = plan(&app, 12);
        assert!(alone.contains(&"oldest_xact".to_string()), "{alone:?}");
        assert!(!alone.iter().any(|slot| slot.contains('+')), "{alone:?}");
    }

    #[test]
    fn a_pair_is_two_charts_in_one_frame_with_the_second_title_on_a_rule() {
        let (mut app, _) = loaded();
        let mut terminal = Terminal::new(TestBackend::new(160, 44)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();

        let (x, top) = find(buffer, "╭┤ Oldest Xact").expect("the upper title");
        let (rule_x, rule) = find(buffer, "├┤ History List")
            .expect("the lower title sits on a rule joined to the frame");
        assert_eq!(rule_x, x, "the rule starts at the frame's own left edge");
        assert!(rule >= top + 4, "charts sit above the rule: {top} {rule}");
        let closes = (rule + 1..buffer.area.height).find(|&y| buffer[(x, y)].symbol() == "╰");
        assert!(
            closes.is_some_and(|y| y >= rule + 4),
            "the frame closes below the lower chart: {closes:?}"
        );
    }

    #[test]
    fn a_short_last_row_is_balanced_not_a_lone_panel_stretched_across() {
        let (mut app, _) = loaded();
        // The demo has nine panels, which four across would leave as 4 + 4 + 1.
        let screen = screen(&mut app, 160, 64);
        let last = screen
            .lines()
            .find(|l| l.contains(" Tmp Disk "))
            .expect("the last panel is drawn");
        assert_eq!(last.matches('╭').count(), 3, "three rows of three: {last}");
    }

    #[test]
    fn a_pair_too_short_for_two_charts_keeps_the_upper_one() {
        let (app, _) = loaded();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|f| draw_pair(f, &app, f.area(), "oldest_xact", "hll"))
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains(" Oldest Xact ") && !screen.contains(" History List "),
            "{screen}"
        );
    }

    #[test]
    fn the_longest_title_and_its_value_fit_the_narrowest_panel() {
        let (mut app, _) = loaded();
        // 128 columns make four panels of 32, the narrowest the grid ever draws.
        let screen = screen(&mut app, 128, 44);
        let edge = screen
            .lines()
            .find(|l| l.contains(" Buffer Pool Hit "))
            .expect("the title is drawn");
        let value = edge.split(" Buffer Pool Hit ").nth(1).unwrap();
        assert!(
            value.starts_with(|c: char| c.is_ascii_digit()) && value.contains('%'),
            "the value was cut off: {edge}"
        );
    }

    #[test]
    fn the_graphs_take_about_half_the_screen_and_the_table_keeps_room_below() {
        let (mut app, _) = loaded();
        for (width, height) in [(80, 24), (100, 30), (120, 40), (160, 44), (160, 64)] {
            let screen = screen(&mut app, width, height);
            // The table's tabs sit on its top edge.
            let table = screen
                .lines()
                .position(|l| l.contains("1 running"))
                .unwrap();
            let height = usize::from(height);
            assert!(
                (height * 35 / 100..=height * 55 / 100).contains(&table),
                "{width}x{height}: the table starts on line {table}"
            );
            assert!(
                height - table >= 14,
                "{width}x{height}: the table has only {} lines",
                height - table
            );
        }
    }

    /// Where `text` starts on the screen.
    fn find(buffer: &ratatui::buffer::Buffer, text: &str) -> Option<(u16, u16)> {
        (0..buffer.area.height).find_map(|y| {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            row.find(text)
                .map(|byte| (row[..byte].chars().count() as u16, y))
        })
    }

    #[test]
    fn a_value_sits_in_its_panels_title_and_is_coloured_by_how_bad_it_is() {
        let (mut app, mut fake) = loaded();
        let mut terminal = Terminal::new(TestBackend::new(160, 44)).unwrap();
        for (hit, colour) in [(99.2, THEME.text), (90.0, THEME.warn), (70.0, THEME.crit)] {
            let sample = fake.poll().unwrap();
            let values = sample
                .metrics
                .iter()
                .map(|m| (m.key, Some(if m.key == "hit" { hit } else { 1.0 })))
                .collect();
            app.on_msg(Msg::Poll { sample, values });
            terminal.draw(|f| draw(f, &mut app)).unwrap();

            let shown = format!("{hit:.1}%");
            let buffer = terminal.backend().buffer();
            let (x, y) =
                find(buffer, &shown).unwrap_or_else(|| panic!("{shown} is not on the screen"));
            assert_eq!(buffer[(x, y)].fg, colour, "{shown}");
            assert!(y <= 1 || y >= 6, "it is in a title on a panel's top edge");
        }
    }

    #[test]
    fn a_metric_with_no_number_yet_shows_a_dash_and_an_unknown_one_is_not_drawn() {
        let (mut app, mut fake) = loaded();
        let mut sample = fake.poll().unwrap();
        sample.metrics.push(Reading {
            key: "mystery",
            raw: Raw::Gauge(1.0),
        });
        let values = sample.metrics.iter().map(|m| (m.key, None)).collect();
        app.on_msg(Msg::Poll { sample, values });

        let screen = screen(&mut app, 160, 44);
        assert!(
            screen.contains(" QPS -") && screen.contains(" Buffer Pool Hit -"),
            "{screen}"
        );
        assert!(!screen.contains("mystery"));
    }

    #[test]
    fn before_the_first_poll_the_two_basic_panels_are_there() {
        let mut app = App::new("test".into());
        let screen = screen(&mut app, 160, 44);
        assert!(
            screen.contains(" QPS ") && screen.contains(" Connections "),
            "{screen}"
        );
    }

    #[test]
    fn everything_the_demo_reports_can_be_drawn() {
        let mut fake = Fake::seeded(1);
        for reading in fake.poll().unwrap().metrics {
            assert!(
                metrics::spec(reading.key).is_some(),
                "{} has no spec",
                reading.key
            );
        }
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
        send(&mut app, sample);

        // The cursor keeps following the row it was on, so go to the top explicitly.
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Enter);
        assert!(screen(&mut app, 120, 40).contains("the server cut this text"));
    }
}
