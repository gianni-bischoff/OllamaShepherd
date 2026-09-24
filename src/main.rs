mod ollama;
mod update;

use ollama::{fetch_usage, pct, KeyEntry, KeyStore, Usage};
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
    selected: usize,
    rx: std::sync::mpsc::Receiver<Msg>,
    outstanding: usize,
    fetching: bool,
    last_fetch: Option<Instant>,
    countdown: u64,
    editing: Option<String>,
    /// false = overview (all keys), true = drill-in on selected key
    view: bool,
    quit: bool,
}

impl App {
    fn new() -> (Self, Sender<Msg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App {
            keys: KeyStore::load().keys,
            usages: Vec::new(),
            selected: 0,
            rx,
            outstanding: 0,
            fetching: false,
            last_fetch: None,
            countdown: AUTO_SECS,
            editing: None,
            view: false,
            quit: false,
        };
        app.usages = vec![Usage::default(); app.keys.len()];
        (app, tx)
    }

    fn start_fetch(&mut self, tx: &Sender<Msg>) {
        if self.fetching || self.keys.is_empty() {
            return;
        }
        self.fetching = true;
        self.outstanding = self.keys.len();
        self.usages = vec![Usage::default(); self.keys.len()];
        for (i, k) in self.keys.iter().enumerate() {
            let key = k.key.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let u = fetch_usage(&key);
                let _ = tx.send(Msg::Usage(i, u));
            });
        }
    }

    fn drain(&mut self) {
        if !self.fetching {
            return;
        }
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Usage(i, u)) => {
                    if i < self.usages.len() {
                        self.usages[i] = u;
                    }
                    self.outstanding = self.outstanding.saturating_sub(1);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if self.outstanding == 0 {
            self.fetching = false;
            self.last_fetch = Some(Instant::now());
        }
    }

    fn save_keys(&self) {
        KeyStore { keys: self.keys.clone() }.save();
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
    while !app.quit {
        app.drain();

        if !app.fetching {
            if let Some(last) = app.last_fetch {
                app.countdown = AUTO_SECS.saturating_sub(last.elapsed().as_secs());
                if app.countdown == 0 {
                    app.start_fetch(tx);
                }
            }
        }

        terminal.draw(|f| ui(f, app))?;

        if crossterm::event::poll(Duration::from_millis(100))? {
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
            crossterm::event::KeyCode::Esc => app.editing = None,
            crossterm::event::KeyCode::Enter => {
                let input = app.editing.take().unwrap_or_default();
                let input = input.trim().to_string();
                if !input.is_empty() {
                    let (label, k) = match input.split_once(',') {
                        Some((l, k)) => (l.trim().to_string(), k.trim().to_string()),
                        None => (String::new(), input),
                    };
                    if !k.is_empty() {
                        app.keys.push(KeyEntry { label, key: k });
                        app.usages.push(Usage::default());
                        app.selected = app.keys.len() - 1;
                        app.save_keys();
                        app.start_fetch(tx);
                    }
                }
            }
            crossterm::event::KeyCode::Backspace => {
                app.editing.as_mut().unwrap().pop();
            }
            crossterm::event::KeyCode::Char(c) => app.editing.as_mut().unwrap().push(c),
            _ => {}
        }
        return;
    }

    match key.code {
        crossterm::event::KeyCode::Char('q') => app.quit = true,
        crossterm::event::KeyCode::Esc => {
            if app.view {
                app.view = false;
            } else {
                app.quit = true;
            }
        }
        crossterm::event::KeyCode::Char('r') => app.start_fetch(tx),
        crossterm::event::KeyCode::Char('a') => app.editing = Some(String::new()),
        crossterm::event::KeyCode::Char('d') => {
            if !app.keys.is_empty() {
                let i = app.selected.min(app.keys.len() - 1);
                app.keys.remove(i);
                app.usages.remove(i);
                if app.selected >= app.keys.len() {
                    app.selected = app.selected.saturating_sub(1);
                }
                app.save_keys();
                if app.keys.is_empty() {
                    app.view = false;
                }
            }
        }
        crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Tab => {
            if !app.keys.is_empty() {
                app.selected = (app.selected + 1) % app.keys.len();
            }
        }
        crossterm::event::KeyCode::Up => {
            if !app.keys.is_empty() {
                app.selected = (app.selected + app.keys.len() - 1) % app.keys.len();
            }
        }
        crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Right => {
            if !app.keys.is_empty() {
                app.view = true;
            }
        }
        crossterm::event::KeyCode::Left => app.view = false,
        crossterm::event::KeyCode::Char('v') => app.view = !app.view,
        _ => {}
    }
}

// ---------------------------------------------------------------- UI ----

fn ui(f: &mut ratatui::Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // content
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    header(f, app, outer[0]);

    if app.keys.is_empty() {
        empty_state(f, outer[1]);
    } else if app.view {
        detail(f, app, outer[1]);
    } else {
        overview(f, app, outer[1]);
    }

    footer(f, outer[2]);

    if let Some(input) = &app.editing {
        edit_popup(f, input, outer[1]);
    }
}

fn header(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let status = if app.fetching {
        Span::styled("⟳ fetching…", Style::new().fg(Color::Yellow))
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
fn overview(f: &mut ratatui::Frame, app: &App, area: Rect) {
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
        let mut cells: Vec<Cell> = Vec::new();
        cells.push(
            Cell::from(display_label(k)).style(
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
                cells.push(
                    Cell::from(format!("{left:>5.1}%"))
                        .style(Style::new().fg(col).add_modifier(Modifier::BOLD)),
                );
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

/// Drill-in: stacked gauges per window (monthly/weekly/session) + per-key pie.
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