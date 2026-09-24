mod ollama;
mod update;

use ollama::{fetch_usage, pct, KeyEntry, Usage};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
    Terminal,
};
use tui_piechart::{LegendAlignment, LegendLayout, LegendPosition, PieChart, PieSlice};
use std::io;
use std::sync::mpsc::{Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// Auto refresh interval.
const AUTO_SECS: u64 = 60;

/// Pie slice palette.
const PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
];

const SEL_BG: Color = Color::Rgb(28, 34, 46);

fn tier(left: f64) -> (&'static str, Color) {
    if left >= 50.0 {
        ("OK", Color::Green)
    } else if left >= 20.0 {
        ("LOW", Color::Yellow)
    } else {
        ("CRIT", Color::Red)
    }
}

enum Msg {
    Usage(usize, Usage),
}

struct App {
    keys: Vec<KeyEntry>,
    usages: Vec<Usage>,
    /// which keys currently have a fetch in flight
    inflight: Vec<bool>,
    selected: usize,
    rx: std::sync::mpsc::Receiver<Msg>,
    outstanding: usize,
    fetching: bool,
    last_fetch: Option<Instant>,
    countdown: u64,
    editing: Option<String>,
    startup_warning: Option<String>,
    /// false = overview (all keys), true = drill-in on selected key
    view: bool,
    quit: bool,
    /// frame is only redrawn when something actually changed
    dirty: bool,
}

/// Braille-ish spinner frames for in-flight keys.
const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

impl App {
    fn new() -> (Self, Sender<Msg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (keys, warning) = ollama::load_keys();
        let n = keys.len();
        let mut app = App {
            keys,
            usages: vec![Usage::default(); n],
            inflight: vec![false; n],
            selected: 0,
            rx,
            outstanding: 0,
            fetching: false,
            last_fetch: None,
            countdown: AUTO_SECS,
            editing: None,
            startup_warning: warning,
            view: false,
            quit: false,
            dirty: true,
        };
        app.usages = vec![Usage::default(); n];
        (app, tx)
    }

    /// Refresh: keeps old data on screen; only clears entries being refetched.
    fn start_fetch(&mut self, tx: &Sender<Msg>) {
        if self.fetching || self.keys.is_empty() {
            return;
        }
        self.fetching = true;
        self.outstanding = self.keys.len();
        // mark all as in-flight but KEEP previous data visible until replaced
        self.inflight = vec![true; self.keys.len()];
        for (i, k) in self.keys.iter().enumerate() {
            let key = k.key.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let u = fetch_usage(&key);
                let _ = tx.send(Msg::Usage(i, u));
            });
        }
        self.dirty = true;
    }

    fn drain(&mut self) {
        if !self.fetching {
            return;
        }
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Usage(i, u)) => {
                    if i < self.usages.len() {
                        self.usages[i] = u; // atomically replace one key's data
                    }
                    if i < self.inflight.len() {
                        self.inflight[i] = false;
                    }
                    self.outstanding = self.outstanding.saturating_sub(1);
                    self.dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if self.outstanding == 0 {
            self.fetching = false;
            self.last_fetch = Some(Instant::now());
            self.dirty = true;
        }
    }

    fn save_keys(&self) {
        ollama::save_keys(&self.keys);
    }
}

fn main() -> io::Result<()> {
    // CLI: `ollama-shepherd update [--check]` — handled outside the TUI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(cmd) = args.first() {
        if cmd == "update" {
            let check_only = args.iter().any(|a| a == "--check" || a == "-n");
            return match update::run(check_only) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("✗ {e}");
                    std::process::exit(1);
                }
            };
        }
        if cmd == "--version" || cmd == "-V" {
            println!("ollama-shepherd v{}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        eprintln!("unknown command: {cmd}");
        eprintln!("usage: ollama-shepherd [update [--check] | --version]");
        std::process::exit(2);
    }

    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (mut app, tx) = App::new();
    app.start_fetch(&tx);

    let res = run_app(&mut terminal, &mut app, &tx);

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    res
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    tx: &Sender<Msg>,
) -> io::Result<()> {
    let mut spinner_frame: usize = 0;

    while !app.quit {
        app.drain();

        // auto refresh countdown
        if !app.fetching {
            if let Some(last) = app.last_fetch {
                let elapsed = last.elapsed().as_secs();
                let new_countdown = AUTO_SECS.saturating_sub(elapsed);
                if new_countdown != app.countdown {
                    app.countdown = new_countdown;
                    app.dirty = true;
                }
                if app.countdown == 0 {
                    app.start_fetch(tx);
                }
            }
        }

        // redraw ONLY when something changed (no idle flicker)
        if app.dirty {
            app.dirty = false;
            terminal.draw(|f| ui(f, app, spinner_frame))?;
        }

        // wait for the next event; a short tick keeps the spinner alive mid-fetch
        let timeout = if app.fetching {
            Duration::from_millis(120) // spinner animation only while fetching
        } else {
            Duration::from_millis(500)
        };
        if crossterm::event::poll(timeout)? {
            match crossterm::event::read()? {
                crossterm::event::Event::Key(key)
                    if key.kind == crossterm::event::KeyEventKind::Press =>
                {
                    handle_key(app, key, tx)
                }
                crossterm::event::Event::Paste(s) => {
                    if let Some(e) = app.editing.as_mut() {
                        e.push_str(&s);
                    }
                }
                _ => {}
            }
        }

        if app.fetching {
            spinner_frame = (spinner_frame + 1) % SPINNER.len();
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent, tx: &Sender<Msg>) {
    // Ctrl+C always quits.
    if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == crossterm::event::KeyCode::Char('c')
    {
        app.quit = true;
        return;
    }

    if app.editing.is_some() {
        match key.code {
            crossterm::event::KeyCode::Esc => {
                app.editing = None;
                app.dirty = true;
            }
            crossterm::event::KeyCode::Enter => {
                let input = app.editing.take().unwrap_or_default();
                app.dirty = true;
                let input = input.trim().to_string();
                if !input.is_empty() {
                    let (label, k) = match input.split_once(',') {
                        Some((l, k)) => (l.trim().to_string(), k.trim().to_string()),
                        None => (String::new(), input),
                    };
                    if !k.is_empty() {
                        app.keys.push(KeyEntry { label, key: k });
                        app.usages.push(Usage::default());
                        app.inflight.push(false);
                        app.selected = app.keys.len() - 1;
                        app.save_keys();
                        app.start_fetch(tx);
                    }
                }
            }
            crossterm::event::KeyCode::Backspace => {
                app.editing.as_mut().unwrap().pop();
                app.dirty = true;
            }
            crossterm::event::KeyCode::Char(c) => {
                app.editing.as_mut().unwrap().push(c);
                app.dirty = true;
            }
            _ => {}
        }
        return;
    }

    match key.code {
        crossterm::event::KeyCode::Char('q') => app.quit = true,
        crossterm::event::KeyCode::Esc => {
            if app.view {
                app.view = false;
                app.dirty = true;
            } else {
                app.quit = true;
            }
        }
        crossterm::event::KeyCode::Char('r') => app.start_fetch(tx),
        crossterm::event::KeyCode::Char('a') => {
            app.editing = Some(String::new());
            app.dirty = true;
        }
        crossterm::event::KeyCode::Char('d') => {
            if !app.keys.is_empty() {
                let i = app.selected.min(app.keys.len() - 1);
                app.keys.remove(i);
                app.usages.remove(i);
                app.inflight.remove(i);
                if app.selected >= app.keys.len() {
                    app.selected = app.selected.saturating_sub(1);
                }
                app.save_keys();
                if app.keys.is_empty() {
                    app.view = false;
                }
                app.dirty = true;
            }
        }
        crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Tab => {
            if !app.keys.is_empty() {
                app.selected = (app.selected + 1) % app.keys.len();
                app.dirty = true;
            }
        }
        crossterm::event::KeyCode::Up => {
            if !app.keys.is_empty() {
                app.selected = (app.selected + app.keys.len() - 1) % app.keys.len();
                app.dirty = true;
            }
        }
        crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Right => {
            if !app.keys.is_empty() {
                app.view = true;
                app.dirty = true;
            }
        }
        crossterm::event::KeyCode::Left => {
            app.view = false;
            app.dirty = true;
        }
        crossterm::event::KeyCode::Char('v') => {
            app.view = !app.view;
            app.dirty = true;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- UI ----

fn ui(f: &mut ratatui::Frame, app: &App, spinner_frame: usize) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // content
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    header(f, app, outer[0], spinner_frame);

    if let Some(w) = &app.startup_warning {
        let warn = Paragraph::new(Line::from(Span::styled(
            format!(" ⚠ {w}"),
            Style::new().fg(Color::Yellow),
        )))
        .block(Block::bordered());
        let area = Rect { height: 3.min(outer[1].height), ..outer[1] };
        f.render_widget(warn, area);
    }

    if app.keys.is_empty() {
        empty_state(f, outer[1]);
    } else if app.view {
        detail(f, app, outer[1]);
    } else {
        overview(f, app, outer[1], spinner_frame);
    }

    footer(f, outer[2]);

    if let Some(input) = &app.editing {
        edit_popup(f, input, outer[1]);
    }
}

fn header(f: &mut ratatui::Frame, app: &App, area: Rect, spinner_frame: usize) {
    let status = if app.fetching {
        Span::styled(
            format!("{} fetching…", SPINNER[spinner_frame % SPINNER.len()]),
            Style::new().fg(Color::Yellow),
        )
    } else if app.last_fetch.is_some() {
        Span::styled(format!("✓ auto-refresh in {}s", app.countdown), Style::new().fg(Color::Green))
    } else {
        Span::styled("· idle", Style::new().fg(Color::DarkGray))
    };
    let view = if app.view { "key detail" } else { "overview" };
    let line = Line::from(vec![
        Span::styled("🐑 Ollama Shepherd", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  {view} · {} keys", app.keys.len()),
            Style::new().fg(Color::DarkGray),
        ),
        Span::styled("   ", Style::new()),
        status,
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::bordered().title(" Ollama Cloud Usage ")),
        area,
    );
}

fn empty_state(f: &mut ratatui::Frame, area: Rect) {
    let txt = Paragraph::new(vec![
        Line::from(""),
        Line::from("No keys yet. Press a to add one."),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Format: ", Style::new().fg(Color::DarkGray)),
            Span::styled("label,ok-xxxx", Style::new().fg(Color::Cyan)),
            Span::styled("  or just the key.", Style::new().fg(Color::DarkGray)),
        ]),
        Line::from(" Get keys: https://ollama.com/settings/keys"),
    ])
    .block(Block::bordered().title(" Ollama Shepherd "))
    .alignment(Alignment::Center);
    f.render_widget(txt, area);
}

/// Overview: one table with every key × every window + global model pie.
fn overview(f: &mut ratatui::Frame, app: &App, area: Rect, spinner_frame: usize) {
    // split: table on top, global pie below (pie hidden if too short)
    let show_pie = area.height >= 16;
    let parts = if show_pie {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(13)])
            .split(area);
        vec![split[0], split[1]]
    } else {
        vec![area]
    };

    let col_names = ollama::all_windows(app.usages.iter());
    let rows = app.keys.iter().zip(&app.usages).enumerate().map(|(i, (k, u))| {
        let sel = i == app.selected;
        let inflight = app.inflight.get(i).copied().unwrap_or(false);
        let mut cells: Vec<Cell> = Vec::new();
        // label cell carries the per-key spinner while in flight
        let label_cell = if inflight {
            format!("{} {}", SPINNER[spinner_frame % SPINNER.len()], display_label(k))
        } else {
            display_label(k)
        };
        cells.push(
            Cell::from(label_cell).style(
                Style::new()
                    .fg(if sel { Color::Cyan } else { Color::White })
                    .add_modifier(if sel { Modifier::BOLD } else { Modifier::empty() }),
            ),
        );
        if let Some(err) = &u.error {
            cells.push(Cell::from(format!("⚠ {err}")).style(Style::new().fg(Color::Red)));
        } else if u.windows.is_empty() {
            cells.push(Cell::from("…".to_string()).style(Style::new().fg(Color::DarkGray)));
        } else {
            for w in &col_names {
                let (_, left) = u
                    .windows
                    .iter()
                    .find(|(name, _)| name == w)
                    .map(|(_, v)| pct(*v))
                    .unwrap_or((0.0, 100.0));
                let col = tier(left).1;
                // keep last value visible; dim it slightly while refetching
                let mut style = Style::new().fg(col).add_modifier(Modifier::BOLD);
                if inflight {
                    style = style.add_modifier(Modifier::DIM);
                }
                cells.push(Cell::from(format!("{left:>5.1}%")).style(style));
            }
        }
        Row::new(cells).style(if sel { Style::new().bg(SEL_BG) } else { Style::new() })
    });

    let mut constraints = vec![Constraint::Percentage(22)];
    let rest = 78u16 / (col_names.len().max(1) as u16);
    for _ in &col_names {
        constraints.push(Constraint::Percentage(rest));
    }
    let header_cells = {
        let mut v = vec!["Key".to_string()];
        v.extend(col_names.iter().cloned());
        v
    };
    let header_row = Row::new(header_cells).style(Style::new().fg(Color::DarkGray));

    let table = Table::new(rows, constraints)
        .header(header_row)
        .column_spacing(2)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" All keys · % left per window "),
        );
    f.render_widget(table, parts[0]);

    if show_pie {
        let models = ollama::aggregate_models(app.usages.iter());
        let src = app
            .usages
            .iter()
            .filter_map(|u| u.models_window.as_deref())
            .map(|w| w.to_string())
            .next()
            .unwrap_or_default();
        render_pie(f, &models, parts[1], &format!(" Requests by model · all keys ({src}) "));
    }
}

/// Drill-in: stacked gauges per window (weekly/session) + per-key pie + period.
fn detail(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let i = app.selected.min(app.keys.len() - 1);
    let k = &app.keys[i];
    let u = &app.usages[i];

    if let Some(err) = &u.error {
        let p = Paragraph::new(vec![
            Line::from(Span::styled("⚠ fetch failed", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(err.clone(), Style::new().fg(Color::Red))),
            Line::from(""),
            Line::from(Span::styled(
                "check the key at https://ollama.com/settings/keys",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .block(Block::bordered().title(format!(" {} ", display_label(k))));
        f.render_widget(p, area);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);

    // gauge stack, one per window
    if u.windows.is_empty() {
        let p = Paragraph::new(if app.fetching { "fetching…" } else { "no data — press r" })
            .style(Style::new().fg(Color::DarkGray))
            .block(Block::bordered().title(format!(" {} ", display_label(k))));
        f.render_widget(p, cols[0]);
    } else {
        let n = u.windows.len().min(8);
        let stack = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Length(4); n])
            .split(cols[0]);
        for (idx, (name, usage)) in u.windows.iter().take(n).enumerate() {
            let (used, left) = pct(*usage);
            let col = tier(left).1;
            let first = idx == 0;
            let g = Gauge::default()
                .ratio((used / 100.0).clamp(0.0, 1.0))
                .gauge_style(Style::new().fg(col).bg(Color::Black))
                .label(format!(
                    "{}{name}: {used:.1}% used · {left:.1}% left",
                    if first { "" } else { "  " }
                ))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(if first {
                            format!(" {} · usage windows ", display_label(k))
                        } else {
                            String::new()
                        }),
                );
            f.render_widget(g, stack[idx]);
        }
        // activity period below the gauge stack
        if let Some(p) = &u.period {
            if let Some(last) = stack.last() {
                let parea = Rect {
                    y: last.y + last.height,
                    height: 1.min(cols[0].height.saturating_sub(last.y + last.height - cols[0].y)),
                    ..*last
                };
                if parea.height > 0 {
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!(" activity: {p}"),
                            Style::new().fg(Color::DarkGray),
                        ))),
                        parea,
                    );
                }
            }
        }
    }

    let src = u.models_window.as_deref().unwrap_or("");
    render_pie(
        f,
        &u.models,
        cols[1],
        &format!(" Requests by model · {} ({src}) ", display_label(k)),
    );
}

/// Pie chart via tui-piechart, with legend and percentages.
fn render_pie(f: &mut ratatui::Frame, models: &[(String, u64)], area: Rect, title: &str) {
    if models.is_empty() || models.iter().all(|(_, n)| *n == 0) {
        let p = Paragraph::new("no model usage recorded")
            .style(Style::new().fg(Color::DarkGray))
            .block(Block::bordered().title(title));
        f.render_widget(p, area);
        return;
    }

    // PieSlice::new borrows &str labels, so build owned strings first.
    let labels: Vec<String> = models
        .iter()
        .map(|(m, n)| format!("{m} · {n}"))
        .collect();
    let slices: Vec<PieSlice> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| PieSlice::new(l.as_str(), models[i].1 as f64, PALETTE[i % PALETTE.len()]))
        .collect();

    let pie = PieChart::new(slices)
        .high_resolution(true)
        .show_legend(true)
        .show_percentages(true)
        .legend_position(LegendPosition::Right)
        .legend_layout(LegendLayout::Vertical)
        .legend_alignment(LegendAlignment::Left)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(title),
        );
    f.render_widget(pie, area);
}

fn display_label(k: &KeyEntry) -> String {
    if k.label.trim().is_empty() {
        if k.key.len() > 8 {
            format!("…{}", &k.key[k.key.len() - 4..])
        } else {
            "key".into()
        }
    } else {
        k.label.clone()
    }
}

fn edit_popup(f: &mut ratatui::Frame, input: &str, area: Rect) {
    let w = 62.min(area.width.saturating_sub(4));
    let h = 7;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Yellow))
        .title(" Add API key ");
    f.render_widget(block, popup);

    let inner = popup.inner(Margin::new(1, 1));
    let text = vec![
        Line::from(Span::styled(format!("{input}█"), Style::new().fg(Color::White))),
        Line::from(""),
        Line::from(Span::styled(
            "label,ok-…  ·  Enter=save  ·  Esc=cancel",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(text), inner);
}

fn footer(f: &mut ratatui::Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled("r", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" refresh  ", Style::new().fg(Color::DarkGray)),
        Span::styled("a", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" add key  ", Style::new().fg(Color::DarkGray)),
        Span::styled("↑/↓", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" select  ", Style::new().fg(Color::DarkGray)),
        Span::styled("Enter", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" drill-in  ", Style::new().fg(Color::DarkGray)),
        Span::styled("d", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" delete  ", Style::new().fg(Color::DarkGray)),
        Span::styled("q", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" quit", Style::new().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line).block(Block::bordered()), area);
}