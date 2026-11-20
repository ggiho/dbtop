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

use crate::app::{App, Detail, Filter, PlanState, SlowSort, Subject, Tab, waiter_counts};
use crate::model::{OpenTransaction, Plan, PlanTable, PlanViews, RunningQuery, Sample, SlowQuery};
use graph::{BrailleGraph, axis_max, visible};
use metrics::{Spec, spec};
use sql::highlight;
use text::{fmt_count, fmt_ms, fmt_secs, one_line, printable, tail, wrap_words};
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
    if app.help {
        draw_help(f, area);
    }
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
    let counts = app.sample.as_ref().map_or((0, 0, 0), |s| {
        (s.running.len(), s.slow.len(), s.transactions.len())
    });
    // With a filter on, how many of each it lets through.
    let shown = match &app.sample {
        Some(s) if app.filter.is_on() => (
            app.filter.running(s).len(),
            app.filter.slow(s).len(),
            app.filter.transactions(s).len(),
        ),
        _ => counts,
    };
    // A transaction open for long is why the tab is not left to look quiet.
    let oldest = app
        .sample
        .as_ref()
        .and_then(|s| s.transactions.iter().map(|t| t.secs).reduce(f64::max))
        .map_or(THEME.dim, age_colour);
    let tab = |key: &str, name: &str, n: (usize, usize), active: bool, alert: Option<Color>| {
        let style = match (active, alert) {
            (true, _) => fg(THEME.accent).bold(),
            (false, Some(alert)) => fg(alert),
            (false, None) => fg(THEME.dim),
        };
        let count = match n {
            (shown, all) if shown == all => all.to_string(),
            (shown, all) => format!("{shown}/{all}"),
        };
        tag(vec![Span::styled(format!(" {key} {name} {count} "), style)])
    };
    let tabs = [
        tab(
            "1",
            "running",
            (shown.0, counts.0),
            app.tab == Tab::Running,
            None,
        ),
        tab("2", "slow", (shown.1, counts.1), app.tab == Tab::Slow, None),
        tab(
            "3",
            "transactions",
            (shown.2, counts.2),
            app.tab == Tab::Transactions,
            [THEME.warn, THEME.crit].contains(&oldest).then_some(oldest),
        ),
    ]
    .concat();
    let room = usize::from(area.width).saturating_sub(Line::from(tabs.clone()).width() + 3);
    let mut block = panel(tabs);
    if let Some(filter) = filter_tag(&app.filter, room) {
        block = block.title_top(filter.right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    let App {
        sample,
        tab,
        running_sel,
        slow_sel,
        slow_sort,
        trx_sel,
        filter,
        ..
    } = app;
    let Some(sample) = sample.as_ref() else {
        return note(f, inner, "connecting…");
    };
    match tab {
        Tab::Running => {
            let table = banner(f, inner, sample.running_note.as_deref());
            let rows = filter.running(sample);
            running_table(f, table, sample, &rows, &mut running_sel.state);
        }
        Tab::Slow => {
            let rows = filter.slow(sample);
            slow_table(f, inner, sample, &rows, &mut slow_sel.state, *slow_sort);
        }
        Tab::Transactions => {
            // With nothing to list the note is the whole screen; with a list it is a warning above it.
            let warning = sample
                .transactions_note
                .as_deref()
                .filter(|_| !sample.transactions.is_empty());
            let table = banner(f, inner, warning);
            let rows = filter.transactions(sample);
            transactions_table(f, table, sample, &rows, &mut trx_sel.state);
        }
    }
}

/// The filter as the table's border carries it, like a search box. Where it does not fit, the
/// end of it: that is where the typing is.
fn filter_tag(filter: &Filter, room: usize) -> Option<Line<'static>> {
    if !filter.typing && !filter.is_on() {
        return None;
    }
    // The tag's two edges, the slash with its spaces, the cursor and the space after it.
    let room = room.checked_sub(2 + 3 + 1 + 1)?.max(1);
    let mut parts = vec![
        Span::styled(" / ", fg(THEME.accent).bold()),
        Span::styled(tail(&filter.text, room), fg(THEME.text)),
    ];
    parts.push(if filter.typing {
        Span::styled("█", fg(THEME.accent))
    } else {
        Span::raw(" ")
    });
    parts.push(Span::raw(" "));
    Some(Line::from(tag(parts)))
}

/// What a list says when the filter lets nothing through.
const NO_MATCH: &str = "nothing matches the filter  ·  Esc clears it";

/// The warning line above a table, if there is one; what is left of `area` for the table.
fn banner(f: &mut Frame, area: Rect, warning: Option<&str>) -> Rect {
    let Some(warning) = warning else { return area };
    let [line, table] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(format!("⚠ {warning}")).style(fg(THEME.warn)),
        line,
    );
    table
}

/// How old a transaction is, in the colours of the Oldest Xact graph.
fn age_colour(secs: f64) -> Color {
    match spec("oldest_xact") {
        Some(spec) if secs >= 1.0 => spec.tone(secs),
        _ => THEME.dim,
    }
}

/// What a session's locks make of it: waiting for another (`← 8841`) and making others wait
/// (`→ 2`). The arrow points the way the waiting goes.
fn lock_marks(blocked_by: &[u64], waiters: usize) -> Vec<Span<'static>> {
    let mut marks = Vec::new();
    if let Some(first) = blocked_by.first() {
        let more = match blocked_by.len() {
            1 => String::new(),
            n => format!("+{}", n - 1),
        };
        marks.push(Span::styled(format!("← {first}{more}"), fg(THEME.warn)));
    }
    if waiters > 0 {
        if !marks.is_empty() {
            marks.push(Span::raw(" "));
        }
        marks.push(Span::styled(format!("→ {waiters}"), fg(THEME.crit).bold()));
    }
    marks
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

fn running_table(
    f: &mut Frame,
    area: Rect,
    sample: &Sample,
    shown: &[&RunningQuery],
    state: &mut TableState,
) {
    if shown.is_empty() {
        let text = if sample.running.is_empty() {
            "no running queries"
        } else {
            NO_MATCH
        };
        return note(f, area, text);
    }
    let waiters = waiter_counts(sample);
    let rows = shown.iter().map(|q| {
        let age = match q.secs {
            s if s >= 60.0 => THEME.crit,
            s if s >= 10.0 => THEME.warn,
            s if s >= 1.0 => THEME.text,
            _ => THEME.dim,
        };
        // The marks come first: a state such as "Waiting for table metadata lock" is wider than
        // its column, and what shows who waits for whom must not be what is cut off.
        let mut state = lock_marks(&q.blocked_by, waiters.get(&q.id).copied().unwrap_or(0));
        if !state.is_empty() {
            state.push(Span::raw(" "));
        }
        state.push(Span::raw(q.state.as_str()));
        Row::new([
            Cell::from(q.id.to_string()),
            Cell::from(q.user.as_str()),
            Cell::from(q.db.as_str()),
            Cell::from(fmt_secs(q.secs)).style(fg(age)),
            Cell::from(Line::from(state)),
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

/// What a session is doing in its transaction, in a word.
fn trx_state(t: &OpenTransaction) -> (&'static str, Color) {
    if t.state.contains("aborted") {
        ("aborted", THEME.crit)
    } else if t.idle {
        // Idle with something held, or for long, is the one to look at.
        let holding = t.rows_locked.is_some_and(|n| n > 0) || t.secs >= 10.0;
        ("idle", if holding { THEME.warn } else { THEME.dim })
    } else if t.state.eq_ignore_ascii_case("lock wait") || !t.blocked_by.is_empty() {
        // Postgres calls a session that waits for a lock `active` too.
        ("lock wait", THEME.warn)
    } else {
        ("running", THEME.text)
    }
}

fn transactions_table(
    f: &mut Frame,
    area: Rect,
    sample: &Sample,
    shown: &[&OpenTransaction],
    state: &mut TableState,
) {
    if sample.transactions.is_empty() {
        return note(
            f,
            area,
            sample
                .transactions_note
                .as_deref()
                .unwrap_or("no open transactions"),
        );
    }
    if shown.is_empty() {
        return note(f, area, NO_MATCH);
    }
    let waiters = waiter_counts(sample);
    let rows = shown.iter().map(|t| {
        let (word, colour) = trx_state(t);
        let locks = t
            .rows_locked
            .map_or("-".to_string(), |n| fmt_count(n as f64));
        let locked = t.rows_locked.is_some_and(|n| n > 0);
        Row::new([
            Cell::from(t.id.to_string()),
            Cell::from(t.user.as_str()),
            Cell::from(fmt_secs(t.secs)).style(fg(age_colour(t.secs))),
            Cell::from(word).style(fg(colour)),
            Cell::from(locks).style(fg(if locked { THEME.text } else { THEME.dim })),
            Cell::from(Line::from(lock_marks(
                &t.blocked_by,
                waiters.get(&t.id).copied().unwrap_or(0),
            ))),
            Cell::from(one_line(&t.sql)),
        ])
    });
    let widths = [
        Constraint::Length(7),
        Constraint::Length(14),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(11),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header([
            "ID", "USER", "AGE", "STATE", "LOCKS", "WAIT", "QUERY",
        ]))
        .row_highlight_style(selected())
        .column_spacing(2);
    f.render_stateful_widget(table, area, state);
}

fn slow_table(
    f: &mut Frame,
    area: Rect,
    sample: &Sample,
    shown: &[&SlowQuery],
    state: &mut TableState,
    sort: SlowSort,
) {
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
    if shown.is_empty() {
        return note(f, area, NO_MATCH);
    }
    let rows = shown.iter().map(|q| {
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
    // The column the list is ranked by stands out.
    let head = [
        "DB",
        "CALLS",
        "TOTAL",
        "AVG",
        "MAX",
        "ROWS EXAM",
        "NOIDX",
        "QUERY",
    ]
    .map(|name| {
        let style = if name == sort.column() {
            fg(THEME.accent).bold()
        } else {
            fg(THEME.dim).bold()
        };
        Cell::from(name).style(style)
    });
    let table = Table::new(rows, widths)
        .header(Row::new(head))
        .row_highlight_style(selected())
        .column_spacing(2);
    f.render_stateful_widget(table, area, state);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    // The ones that cannot be guessed come first: a narrow terminal cuts the line at its end.
    let keys = if app.filter.typing {
        vec![("Enter", "keep"), ("Esc", "clear"), ("↑↓", "move")]
    } else {
        let mut keys = vec![
            ("q", "quit"),
            ("?", "help"),
            ("Enter", "details"),
            (
                "/",
                if app.filter.is_on() {
                    "edit filter"
                } else {
                    "filter"
                },
            ),
            ("Space", if app.paused() { "resume" } else { "pause" }),
        ];
        if app.filter.is_on() {
            keys.push(("Esc", "clear"));
        }
        keys.extend([("Tab", "switch"), ("↑↓", "move")]);
        keys
    };
    let mut hints = Vec::new();
    for (i, (k, what)) in keys.into_iter().enumerate() {
        hints.push(Span::raw(if i == 0 { " " } else { "   " }));
        hints.push(Span::styled(k, fg(THEME.accent).bold()));
        hints.push(Span::styled(format!(" {what}"), fg(THEME.dim)));
    }
    let hints = match app.notice() {
        Some(notice) => Line::styled(format!(" {}", notice.text), notice_style(notice.failed)),
        None => Line::from(hints),
    };
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
            spans.push(match app.paused_since {
                // Stale on purpose, so it must be hard to miss.
                Some(since) => {
                    let held = since.elapsed().as_secs();
                    let held = if held < 60 {
                        format!("{held}s")
                    } else {
                        fmt_secs(held as f64)
                    };
                    Span::styled(format!(" · PAUSED {held} "), fg(THEME.warn).bold())
                }
                None => Span::styled(
                    format!(" · {:.1}s ago ", at.elapsed().as_secs_f64()),
                    fg(THEME.dim),
                ),
            });
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

/// (keys, what they do). Rows with no keys are headings; with neither, a gap.
const HELP: &[(&str, &str)] = &[
    (
        "↑↓ j k  PgUp PgDn  g G",
        "move the cursor, or scroll the details",
    ),
    ("Tab  Shift+Tab  1 2 3", "switch between the lists"),
    ("Enter", "open the details: full SQL, info and plan"),
    ("Esc  q  Enter", "close the details"),
    ("/", "filter: every word must match, Esc clears"),
    ("s", "Slow list: rank by the next column"),
    ("c", "copy the SQL (of the row or the details)"),
    ("Space  p", "pause and resume the screen"),
    ("q  Ctrl+C", "quit"),
    ("", ""),
    ("", "Locks, in the Running and Transactions lists"),
    ("← 8841", "waits for session 8841"),
    ("→ 2", "two sessions wait for this one"),
];

fn draw_help(f: &mut Frame, area: Rect) {
    let keys_width = HELP.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    let wide = HELP
        .iter()
        .filter(|(keys, _)| !keys.is_empty())
        .map(|(_, what)| keys_width + 3 + what.width())
        .max()
        .unwrap_or(0);
    let width = u16::try_from(wide + 6).unwrap_or(u16::MAX).min(area.width);
    let height = u16::try_from(HELP.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let popup = area
        .centered_horizontally(Constraint::Length(width))
        .centered_vertically(Constraint::Length(height));
    f.buffer_mut()
        .set_style(area, Style::new().add_modifier(Modifier::DIM));
    f.render_widget(Clear, popup);

    let block = panel(tag(vec![Span::styled(" help ", fg(THEME.accent).bold())]))
        .border_style(fg(THEME.accent))
        .title_bottom(Line::from(tag(vec![Span::styled(
            " any key closes ",
            fg(THEME.dim),
        )])));
    let lines: Vec<Line> = HELP
        .iter()
        .map(|&(keys, what)| match (keys, what) {
            ("", heading) => Line::styled(heading, fg(THEME.accent).bold()),
            _ => Line::from(vec![
                Span::styled(format!("{keys:<keys_width$}   "), fg(THEME.accent)),
                Span::styled(what, fg(THEME.text)),
            ]),
        })
        .collect();
    let inner = block.inner(popup).inner(Margin::new(2, 0));
    f.render_widget(block, popup);
    f.render_widget(Paragraph::new(lines), inner);
}

fn notice_style(failed: bool) -> Style {
    fg(if failed { THEME.warn } else { THEME.accent })
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let notice = app.notice().map(|n| (n.text.clone(), n.failed));
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
        Subject::Transaction { trx, .. } => format!(" transaction #{} ", trx.id),
    };
    let block = panel(tag(vec![Span::styled(title, fg(THEME.accent).bold())]))
        .border_style(fg(THEME.accent));
    let inner = block.inner(popup).inner(Margin::new(1, 0));

    let lines = detail_lines(detail, usize::from(inner.width));
    let max_scroll = lines.len().saturating_sub(usize::from(inner.height));
    detail.scroll = detail.scroll.min(max_scroll);

    let mut hints = " ↑↓ scroll   g/G top/end   c copy SQL   Esc close ".to_string();
    if max_scroll > 0 {
        let first = detail.scroll + 1;
        let last = (detail.scroll + usize::from(inner.height)).min(lines.len());
        hints = format!(" lines {first}-{last} of {} │{hints}", lines.len());
    }
    // What became of a copy is told where the eyes are, in place of the keys.
    let bottom = match notice {
        Some((text, failed)) => Span::styled(format!(" {text} "), notice_style(failed)),
        None => Span::styled(hints, fg(THEME.dim)),
    };
    let block = block.title_bottom(Line::from(tag(vec![bottom])));

    f.render_widget(block, popup);
    let scroll = u16::try_from(detail.scroll).unwrap_or(u16::MAX);
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

fn detail_lines(detail: &Detail, width: usize) -> Vec<Line<'static>> {
    let label = fg(THEME.dim);
    let value = fg(THEME.text);
    let mut out: Vec<Line<'static>> = Vec::new();

    match &detail.subject {
        Subject::Running { finished: true, .. } => out.push(Line::styled(
            "finished: this statement is no longer running",
            fg(THEME.warn),
        )),
        Subject::Transaction { finished: true, .. } => out.push(Line::styled(
            "finished: this transaction has ended",
            fg(THEME.warn),
        )),
        _ => {}
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
        Subject::Transaction { trx, .. } => (trx.sql.as_str(), trx.truncated),
    };
    if truncated {
        out.push(Line::styled(
            "⚠ the server cut this text; it is not the full statement",
            fg(THEME.warn),
        ));
    }
    if let Subject::Transaction { trx, .. } = &detail.subject {
        if trx.sql.is_empty() {
            out.push(Line::styled("not known", label));
        } else if trx.idle {
            out.push(Line::styled(
                "the last statement this session ran: the locks may come from an earlier one",
                label,
            ));
        }
    }
    out.extend(highlight(sql, width));

    // A transaction is not a statement: there is no plan to ask for.
    if matches!(detail.plan, PlanState::NotAsked) {
        return out;
    }
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
        PlanState::NotAsked => {}
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
        Subject::Transaction {
            trx: t, blocking, ..
        } => {
            let ids = |ids: &[u64]| {
                ids.iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut rows = vec![
                ("user".to_string(), format!("{}@{}", t.user, t.host)),
                ("db".to_string(), t.db.clone()),
                ("open for".to_string(), fmt_secs(t.secs)),
                (
                    "session".to_string(),
                    if t.idle {
                        "idle: between statements"
                    } else {
                        "running a statement"
                    }
                    .to_string(),
                ),
                ("state".to_string(), t.state.clone()),
            ];
            if let (Some(locked), Some(modified)) = (t.rows_locked, t.rows_modified) {
                rows.push((
                    "rows".to_string(),
                    format!("{locked} locked, {modified} modified"),
                ));
            }
            if !t.blocked_by.is_empty() {
                rows.push(("waits for".to_string(), ids(&t.blocked_by)));
            }
            if !blocking.is_empty() {
                rows.push((
                    "others wait".to_string(),
                    format!("{} ({})", blocking.len(), ids(blocking)),
                ));
            }
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
    use crate::model::{Raw, Reading, RunningQuery, Sample, SlowQuery};

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
    fn the_column_the_slow_list_is_ranked_by_stands_out() {
        let (mut app, _) = loaded();
        press(&mut app, KeyCode::Char('2'));
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let colour_of = |app: &mut App, terminal: &mut Terminal<TestBackend>, name: &str| {
            terminal.draw(|f| draw(f, app)).unwrap();
            let buffer = terminal.backend().buffer();
            let (x, y) = find(buffer, name).unwrap_or_else(|| panic!("no {name} header"));
            buffer[(x, y)].fg
        };
        assert_eq!(colour_of(&mut app, &mut terminal, "TOTAL"), THEME.accent);
        assert_eq!(colour_of(&mut app, &mut terminal, "AVG"), THEME.dim);

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(colour_of(&mut app, &mut terminal, "AVG"), THEME.accent);
        assert_eq!(colour_of(&mut app, &mut terminal, "TOTAL"), THEME.dim);
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

    fn open_transaction(
        id: u64,
        user: &str,
        secs: f64,
        idle: bool,
        locked: u64,
        sql: &str,
    ) -> OpenTransaction {
        OpenTransaction {
            id,
            user: user.into(),
            host: "10.0.0.8".into(),
            db: "settlement".into(),
            secs,
            idle,
            state: "RUNNING".into(),
            rows_locked: Some(locked),
            rows_modified: Some(locked),
            sql: sql.into(),
            truncated: false,
            blocked_by: vec![],
        }
    }

    fn running_query(id: u64, secs: f64, sql: &str, blocked_by: Vec<u64>) -> RunningQuery {
        RunningQuery {
            id,
            user: "app".into(),
            host: "10.0.0.9".into(),
            db: "settlement".into(),
            state: "updating".into(),
            secs,
            sql: sql.into(),
            truncated: false,
            details: vec![],
            blocked_by,
        }
    }

    /// A session idle in a transaction that holds rows, another idle one that holds nothing, and
    /// an update that waits for the first.
    fn lock_scene(fake: &mut Fake) -> Sample {
        let mut sample = fake.poll().unwrap();
        let mut waiter =
            open_transaction(8800, "app", 30.0, false, 0, "UPDATE settlement SET a = 1");
        waiter.state = "LOCK WAIT".into();
        waiter.blocked_by = vec![7711];
        sample.running = vec![running_query(
            8800,
            30.0,
            "UPDATE settlement SET a = 1",
            vec![7711],
        )];
        sample.transactions = vec![
            open_transaction(
                7711,
                "batch",
                125.0,
                true,
                1204,
                "UPDATE settlement SET status = 'HOLD' WHERE batch = 7731",
            ),
            waiter,
            open_transaction(7712, "reader", 3.0, true, 0, "SELECT 1"),
        ];
        sample
    }

    /// The table row that starts with `id`.
    fn row_of<'a>(screen: &'a str, id: &str) -> &'a str {
        screen
            .lines()
            .find(|l| {
                l.trim_start_matches('"')
                    .trim_start_matches('│')
                    .trim_start()
                    .starts_with(id)
            })
            .unwrap_or_else(|| panic!("no row for {id} in:\n{screen}"))
    }

    #[test]
    fn the_transactions_tab_lists_the_oldest_first_and_says_who_waits_for_whom() {
        let (mut app, mut fake) = loaded();
        let sample = lock_scene(&mut fake);
        send(&mut app, sample);
        press(&mut app, KeyCode::Char('3'));
        let screen = screen(&mut app, 150, 40);

        assert!(screen.contains(" 3 transactions 3 "), "{screen}");
        let at = |id: &str| screen.find(row_of(&screen, id)).unwrap();
        assert!(
            at("7711") < at("8800") && at("8800") < at("7712"),
            "oldest first"
        );

        let holder = row_of(&screen, "7711");
        assert!(
            holder.contains("batch") && holder.contains("idle") && holder.contains("1.2k"),
            "{holder}"
        );
        assert!(holder.contains("→ 1"), "one session waits for it: {holder}");
        let waiter = row_of(&screen, "8800");
        assert!(
            waiter.contains("lock wait") && waiter.contains("← 7711"),
            "{waiter}"
        );
        let quiet = row_of(&screen, "7712");
        assert!(!quiet.contains('→') && !quiet.contains('←'), "{quiet}");
    }

    #[test]
    fn a_session_that_waits_for_another_is_in_lock_wait_whatever_the_server_calls_it() {
        let mut t = open_transaction(1, "u", 5.0, false, 0, "x");
        t.state = "active".into();
        assert_eq!(trx_state(&t), ("running", THEME.text));
        t.blocked_by = vec![9];
        assert_eq!(trx_state(&t), ("lock wait", THEME.warn));

        let mut idle = open_transaction(2, "u", 50.0, true, 0, "x");
        assert_eq!(trx_state(&idle), ("idle", THEME.warn), "idle for long");
        idle.secs = 3.0;
        assert_eq!(
            trx_state(&idle),
            ("idle", THEME.dim),
            "idle for a moment, holding nothing"
        );
        idle.rows_locked = Some(5);
        assert_eq!(
            trx_state(&idle),
            ("idle", THEME.warn),
            "idle and holding rows"
        );
        idle.state = "idle in transaction (aborted)".into();
        assert_eq!(trx_state(&idle), ("aborted", THEME.crit));
    }

    #[test]
    fn the_running_list_marks_a_waiter_and_whoever_makes_it_wait() {
        let (mut app, mut fake) = loaded();
        let mut sample = lock_scene(&mut fake);
        sample.running = vec![
            running_query(8805, 40.0, "UPDATE a SET x = 1", vec![]),
            running_query(8806, 20.0, "UPDATE a SET x = 2", vec![8805]),
            running_query(8807, 10.0, "UPDATE a SET x = 3", vec![8805, 8806]),
        ];
        sample.transactions.clear();
        send(&mut app, sample);
        let screen = screen(&mut app, 150, 40);

        let blocker = row_of(&screen, "8805");
        assert!(
            blocker.contains("→ 2") && !blocker.contains('←'),
            "{blocker}"
        );
        let waiter = row_of(&screen, "8806");
        assert!(
            waiter.contains("← 8805 → 1 updating"),
            "it waits for one and is waited for by one: {waiter}"
        );
        let two = row_of(&screen, "8807");
        assert!(two.contains("← 8805+1 updating"), "{two}");
    }

    #[test]
    fn an_old_transaction_is_yellow_on_its_row_and_on_the_tab_that_holds_it() {
        let (mut app, mut fake) = loaded();
        let sample = lock_scene(&mut fake);
        send(&mut app, sample);

        // The Running tab is open: the third tab still warns, as the Oldest Xact graph does.
        let mut terminal = Terminal::new(TestBackend::new(150, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let (x, y) = find(terminal.backend().buffer(), " 3 transactions").unwrap();
        assert_eq!(terminal.backend().buffer()[(x + 2, y)].fg, THEME.warn);

        press(&mut app, KeyCode::Char('3'));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = find(buffer, "2m05s").expect("the holder's age");
        assert_eq!(buffer[(x, y)].fg, THEME.warn, "125 s");
        let (x, y) = find(buffer, "30.0s").expect("the waiter's age");
        assert_eq!(
            buffer[(x, y)].fg,
            THEME.text,
            "under a minute is not alarming"
        );
    }

    #[test]
    fn an_empty_transactions_tab_says_so_and_a_note_says_why() {
        let (mut app, mut fake) = loaded();
        press(&mut app, KeyCode::Char('3'));
        let mut sample = fake.poll().unwrap();
        sample.transactions.clear();
        send(&mut app, sample.clone());
        assert!(screen(&mut app, 120, 40).contains("no open transactions"));

        sample.transactions_note = Some("cannot read InnoDB transactions: denied".into());
        send(&mut app, sample);
        let shown = screen(&mut app, 120, 40);
        assert!(
            shown.contains("cannot read InnoDB transactions: denied"),
            "{shown}"
        );
        assert!(!shown.contains("no open transactions"));

        // With a list the note is a warning above it.
        let mut with_list = lock_scene(&mut fake);
        with_list.transactions_note = Some("what an idle session ran last is not known".into());
        send(&mut app, with_list);
        let shown = screen(&mut app, 150, 40);
        assert!(
            shown.contains("⚠ what an idle session ran last is not known"),
            "{shown}"
        );
        assert!(shown.contains("7711"));
    }

    #[test]
    fn a_transactions_details_show_its_session_and_whoever_waits_and_no_plan() {
        let (mut app, mut fake) = loaded();
        let sample = lock_scene(&mut fake);
        send(&mut app, sample);
        press(&mut app, KeyCode::Char('3'));
        let asked = press(&mut app, KeyCode::Enter);
        assert!(asked.try_recv().is_err());

        let text = plain(&detail_lines(app.detail.as_ref().unwrap(), 120)).join("\n");
        for expected in [
            "batch@10.0.0.8",
            "2m05s",
            "idle: between statements",
            "1204 locked, 1204 modified",
            "others wait  1 (#8800)",
            "── SQL",
            "the last statement this session ran",
            "UPDATE settlement SET status = 'HOLD'",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(
            !text.contains("── Plan"),
            "a transaction has no plan:\n{text}"
        );

        // The one that waits says what for.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        let text = plain(&detail_lines(app.detail.as_ref().unwrap(), 120)).join("\n");
        assert!(
            text.contains("waits for") && text.contains("#7711"),
            "{text}"
        );
        assert!(text.contains("running a statement"), "{text}");
    }

    #[test]
    fn a_statement_that_is_not_known_says_so() {
        let (mut app, mut fake) = loaded();
        let mut sample = lock_scene(&mut fake);
        sample.transactions[0].sql = String::new();
        send(&mut app, sample);
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Enter);
        let text = plain(&detail_lines(app.detail.as_ref().unwrap(), 120)).join("\n");
        assert!(text.contains("not known"), "{text}");
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

    fn type_into_filter(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn the_filter_sits_in_the_tables_border_and_the_tabs_count_what_it_lets_through() {
        let (mut app, mut fake) = loaded();
        let sample = lock_scene(&mut fake);
        send(&mut app, sample);
        press(&mut app, KeyCode::Char('/'));
        type_into_filter(&mut app, "settlement update");

        let typing = screen(&mut app, 150, 40);
        let border = typing.lines().find(|l| l.contains("1 running")).unwrap();
        assert!(border.contains("/ settlement update█"), "{border}");
        let footer = typing.lines().last().unwrap();
        assert!(
            footer.contains("Enter keep") && footer.contains("Esc clear"),
            "{footer}"
        );

        press(&mut app, KeyCode::Enter);
        let kept = screen(&mut app, 150, 40);
        let border = kept.lines().find(|l| l.contains("1 running")).unwrap();
        assert!(
            border.contains("/ settlement update") && !border.contains('█'),
            "no cursor once typing is over: {border}"
        );
        assert!(
            border.contains(" 3 transactions 2/3 "),
            "the one that does not match is left out of the count: {border}"
        );
        let footer = kept.lines().last().unwrap();
        assert!(
            footer.contains("edit filter") && footer.contains("Esc clear"),
            "{footer}"
        );
    }

    #[test]
    fn a_filter_that_lets_nothing_through_says_so_and_how_to_get_the_rows_back() {
        let (mut app, _) = loaded();
        press(&mut app, KeyCode::Char('/'));
        type_into_filter(&mut app, "no such thing anywhere");
        press(&mut app, KeyCode::Enter);
        for tab in [Tab::Running, Tab::Slow, Tab::Transactions] {
            app.tab = tab;
            let shown = screen(&mut app, 120, 40);
            assert!(
                shown.contains("nothing matches the filter  ·  Esc clears it"),
                "{shown}"
            );
        }
    }

    #[test]
    fn a_session_that_the_filter_hides_still_counts_as_waiting() {
        let (mut app, mut fake) = loaded();
        let sample = lock_scene(&mut fake);
        send(&mut app, sample);
        press(&mut app, KeyCode::Char('/'));
        type_into_filter(&mut app, "batch");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('3'));

        let screen = screen(&mut app, 150, 40);
        assert!(!screen.contains("8800"), "the waiter is filtered out");
        assert!(
            row_of(&screen, "7711").contains("→ 1"),
            "but the holder still says someone waits for it:
{screen}"
        );
    }

    #[test]
    fn the_help_lists_every_key_and_fits_the_smallest_screen() {
        let (mut app, _) = loaded();
        press(&mut app, KeyCode::Char('?'));
        for (width, height) in [(80, 20), (120, 40)] {
            let shown = screen(&mut app, width, height);
            for (keys, what) in HELP {
                assert!(
                    shown.contains(keys) && shown.contains(what),
                    "{width}x{height}: {keys:?} {what:?} in\n{shown}"
                );
            }
        }
        let shown = screen(&mut app, 80, 20);
        assert!(shown.contains("any key closes"), "{shown}");
        assert!(
            shown.lines().all(|l| !l.contains("1 running")),
            "the help is on top of the lists, which are dimmed behind it:\n{shown}"
        );
    }

    #[test]
    fn what_became_of_a_copy_replaces_the_hints_in_the_details_and_in_the_footer() {
        use crate::app::ToCopy;
        use crate::clipboard::Copied;
        let (mut app, _) = loaded();
        let copy = ToCopy {
            sql: "SELECT 1".into(),
            cut: false,
        };

        press(&mut app, KeyCode::Enter);
        assert!(screen(&mut app, 120, 40).contains("c copy SQL"));

        app.copied(&copy, Ok(Copied::Clipboard));
        let shown = screen(&mut app, 120, 40);
        assert!(
            shown.contains("copied the SQL (8 characters)") && !shown.contains("c copy SQL"),
            "{shown}"
        );

        press(&mut app, KeyCode::Esc);
        let shown = screen(&mut app, 120, 40);
        let footer = shown.lines().last().unwrap();
        assert!(footer.contains("copied the SQL (8 characters)"), "{footer}");
        assert!(!footer.contains("Space pause"), "{footer}");
    }

    #[test]
    fn the_footer_says_when_the_screen_is_held_still_and_which_key_lets_it_go() {
        let (mut app, _) = loaded();
        let last_line = |app: &mut App| screen(app, 120, 40).lines().last().unwrap().to_string();
        let footer = last_line(&mut app);
        assert!(
            footer.contains("Space pause") && footer.contains("s ago"),
            "{footer}"
        );

        press(&mut app, KeyCode::Char(' '));
        let footer = last_line(&mut app);
        assert!(
            footer.contains("PAUSED 0s") && footer.contains("Space resume"),
            "{footer}"
        );
        assert!(!footer.contains("s ago"), "{footer}");

        press(&mut app, KeyCode::Char(' '));
        assert!(!last_line(&mut app).contains("PAUSED"));
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
