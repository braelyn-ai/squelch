//! squelch-tui: a deliberately minimal local debug/setup viewer.
//!
//! This is NOT the product surface — it's a local operator's window into the
//! store. It is the ONLY place sealed messages are ever shown (via the
//! local-only `Store::sealed_messages`), and even here their bodies stay hidden
//! until the operator explicitly reveals them.
//!
//! Layout is a single screen:
//!   - a ranked list split by a visible horizontal "squelch line"
//!   - PastDue (red) + Deadline tiers pinned at the top
//!   - Signal above the line
//!   - Noise below the line, collapsed to a count until toggled with `s`
//!   - Sealed messages listed at the very bottom as lock glyphs, bodies hidden
//!     until `r`
//!
//! Keys: j/k move, Enter drill into a (stub) thread detail pane, s toggle noise,
//! r reveal sealed content, q quit.

use std::io;

use anyhow::Result;
use chrono::{Duration, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use squelch_core::store::{SealedMessage, SqliteStore, Store};
use squelch_core::types::{AccountId, Sensitivity, Tier, Update};

/// A single rendered row in the list. Either a ranked update or a sealed stub.
enum Row {
    Update(Update),
    Sealed(SealedMessage),
    /// Rendered position of the squelch line (not selectable).
    SquelchLine,
    /// Collapsed noise summary (not selectable when collapsed).
    NoiseSummary(usize),
    /// Section header (not selectable).
    Header(String),
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(self, Row::Update(_) | Row::Sealed(_))
    }
}

struct App {
    updates: Vec<Update>,
    sealed: Vec<SealedMessage>,
    /// Index into the flattened selectable rows.
    selected: usize,
    /// Show noise tier below the squelch line.
    show_noise: bool,
    /// Reveal sealed content (bodies/codes). Defaults to hidden.
    reveal_sealed: bool,
    /// When Some, we're in the (stub) thread detail pane for this thread id.
    detail: Option<String>,
    quit: bool,
}

impl App {
    fn new(updates: Vec<Update>, sealed: Vec<SealedMessage>) -> Self {
        Self {
            updates,
            sealed,
            selected: 0,
            show_noise: false,
            reveal_sealed: false,
            detail: None,
            quit: false,
        }
    }

    fn signal_count(&self) -> usize {
        self.updates
            .iter()
            .filter(|u| matches!(u.tier, Tier::Signal | Tier::PastDue | Tier::Deadline))
            .count()
    }

    fn noise_count(&self) -> usize {
        self.updates
            .iter()
            .filter(|u| matches!(u.tier, Tier::Noise))
            .count()
    }

    /// Build the flattened, ordered list of rows for the current view state.
    /// Order: PastDue -> Deadline -> Signal -> [squelch line] -> Noise
    /// (collapsed or expanded) -> [sealed section].
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();

        let by_tier = |t: Tier| -> Vec<&Update> {
            self.updates.iter().filter(|u| u.tier == t).collect()
        };

        for u in by_tier(Tier::PastDue) {
            rows.push(Row::Update(u.clone()));
        }
        for u in by_tier(Tier::Deadline) {
            rows.push(Row::Update(u.clone()));
        }
        for u in by_tier(Tier::Signal) {
            rows.push(Row::Update(u.clone()));
        }

        rows.push(Row::SquelchLine);

        let noise = by_tier(Tier::Noise);
        if self.show_noise {
            for u in noise {
                rows.push(Row::Update(u.clone()));
            }
        } else if !noise.is_empty() {
            rows.push(Row::NoiseSummary(noise.len()));
        }

        if !self.sealed.is_empty() {
            rows.push(Row::Header(format!(
                "\u{1f512} sealed ({}) — local-only, invisible to MCP",
                self.sealed.len()
            )));
            for s in &self.sealed {
                rows.push(Row::Sealed(s.clone()));
            }
        }

        rows
    }

    /// Indices (into `rows()`) that are selectable.
    fn selectable_indices(rows: &[Row]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.selectable())
            .map(|(i, _)| i)
            .collect()
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        let sel = Self::selectable_indices(&rows);
        if sel.is_empty() {
            return;
        }
        let n = sel.len() as isize;
        let cur = self.selected as isize;
        self.selected = ((cur + delta).rem_euclid(n)) as usize;
    }

    /// The currently-selected concrete row (Update or Sealed), if any.
    fn selected_row_kind(&self) -> Option<SelectedKind> {
        let rows = self.rows();
        let sel = Self::selectable_indices(&rows);
        let idx = *sel.get(self.selected)?;
        match &rows[idx] {
            Row::Update(u) => Some(SelectedKind::Thread(u.thread_id.clone())),
            Row::Sealed(_) => Some(SelectedKind::Sealed),
            _ => None,
        }
    }
}

enum SelectedKind {
    Thread(String),
    Sealed,
}

fn main() -> Result<()> {
    // v0: load from the sqlite store. Falls back to an in-memory store seeded
    // with fake rows so the screen isn't blank during setup/debugging.
    let (updates, sealed) = load_or_seed()?;

    let mut terminal = init_terminal()?;
    let mut app = App::new(updates, sealed);

    let res = run(&mut terminal, &mut app);

    restore_terminal(&mut terminal)?;
    res
}

fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // In the detail pane, any of Enter/Esc/q returns to the list.
            if app.detail.is_some() {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => app.detail = None,
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
                KeyCode::Char('s') => {
                    app.show_noise = !app.show_noise;
                    // Clamp selection after the row set changes.
                    app.move_selection(0);
                }
                KeyCode::Char('r') => app.reveal_sealed = !app.reveal_sealed,
                KeyCode::Enter => {
                    if let Some(SelectedKind::Thread(id)) = app.selected_row_kind() {
                        app.detail = Some(id);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(1),    // list
            Constraint::Length(1), // footer / keys
        ])
        .split(f.area());

    render_header(f, app, chunks[0]);
    render_list(f, app, chunks[1]);
    render_footer(f, chunks[2]);

    if let Some(thread_id) = &app.detail {
        render_detail(f, app, thread_id);
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let signal = app.signal_count();
    let noise = app.noise_count();
    let line = Line::from(vec![
        Span::styled(
            " squelch ",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(
            format!("signal {signal}"),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" / "),
        Span::styled(
            format!("noise {noise}"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled(
            format!("sealed {}", app.sealed.len()),
            Style::default().fg(Color::Magenta),
        ),
    ]);
    let p = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" local debug viewer "),
    );
    f.render_widget(p, area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    let sel = App::selectable_indices(&rows);
    let selected_row_idx = sel.get(app.selected).copied();
    let width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let is_selected = Some(i) == selected_row_idx;
        lines.push(render_row(row, is_selected, app.reveal_sealed, width));
    }

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_row(row: &Row, selected: bool, reveal_sealed: bool, width: usize) -> Line<'static> {
    let cursor = if selected { "> " } else { "  " };
    match row {
        Row::Update(u) => {
            let (glyph, color) = match u.tier {
                Tier::PastDue => ("!", Color::Red),
                Tier::Deadline => ("\u{25b2}", Color::Yellow), // ▲
                Tier::Signal => ("\u{2022}", Color::Green),    // •
                Tier::Noise => ("\u{00b7}", Color::DarkGray),  // ·
            };
            let mut style = Style::default().fg(color);
            if u.tier == Tier::PastDue {
                style = style.add_modifier(Modifier::BOLD);
            }
            if u.tier == Tier::Noise {
                style = style.add_modifier(Modifier::DIM);
            }
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let text = format!(
                "{cursor}{glyph} [{:>3}] {} — {}",
                u.importance, u.sender, u.one_line
            );
            Line::from(Span::styled(truncate(text, width), style))
        }
        Row::Sealed(s) => {
            let kind = s.sealed_kind.as_deref().unwrap_or("sealed");
            let body = if reveal_sealed {
                // Even revealed, the store doesn't hand the TUI the code/body
                // (sealed_messages is metadata-only), so we show subject as the
                // most sensitive thing available and mark it revealed.
                format!("REVEALED subject: {}", s.subject)
            } else {
                "•••••• (press r to reveal)".to_string()
            };
            let mut style = Style::default().fg(Color::Magenta);
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let text = format!("{cursor}\u{1f512} {} [{}] {}", s.from_addr, kind, body);
            Line::from(Span::styled(truncate(text, width), style))
        }
        Row::SquelchLine => {
            let dashes = "\u{2500}".repeat(width.saturating_sub(15).max(3));
            Line::from(Span::styled(
                format!("  \u{2500}\u{2500} squelch {dashes}"),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))
        }
        Row::NoiseSummary(n) => Line::from(Span::styled(
            format!("  \u{00b7} {n} noise hidden (press s to show)"),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )),
        Row::Header(h) => Line::from(Span::styled(
            format!("  {h}"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )),
    }
}

fn render_footer(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![Span::styled(
        " j/k move  Enter thread  s noise  r reveal sealed  q quit ",
        Style::default().fg(Color::DarkGray),
    )]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_detail(f: &mut Frame, app: &App, thread_id: &str) {
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);

    // STUB: real thread rendering would call Store::thread_view. This is a
    // local debug placeholder pane.
    let subject = app
        .updates
        .iter()
        .find(|u| u.thread_id == thread_id)
        .map(|u| u.one_line.clone())
        .unwrap_or_else(|| "(unknown)".to_string());

    let text = vec![
        Line::from(Span::styled(
            format!("thread {thread_id}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(subject),
        Line::from(""),
        Line::from(Span::styled(
            "(stub) thread_view rendering lands with the real store wiring.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter/Esc/q to return",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let p = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" thread detail "),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn truncate(mut s: String, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() > width {
        s = s.chars().take(width.saturating_sub(1)).collect();
        s.push('\u{2026}'); // …
    }
    s
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

fn load_or_seed() -> Result<(Vec<Update>, Vec<SealedMessage>)> {
    // Use an in-memory store for v0 so the debug viewer is self-contained.
    // TODO(real-wiring): open Config::default().db_path instead of in-memory,
    //   and drop the seed_fake_data call below.
    let store = SqliteStore::open_in_memory()?;
    let account = store.ensure_account("me@example.com")?;

    let since = Utc::now() - Duration::days(30);
    let mut updates = store.ranked_updates(account, since, None)?;
    let mut sealed = store.sealed_messages(account)?;

    if updates.is_empty() && sealed.is_empty() {
        // TODO(remove): seed fake in-memory rows so the screen isn't blank.
        seed_fake_data(&store, account)?;
        updates = store.ranked_updates(account, since, None)?;
        sealed = store.sealed_messages(account)?;
    }

    Ok((updates, sealed))
}

/// TODO(remove): seeds a handful of fake rows across every tier plus sealed
/// messages so the debug viewer renders something during setup.
fn seed_fake_data(store: &SqliteStore, account: AccountId) -> Result<()> {
    use squelch_core::types::{NewMessage, SealedKind};

    let now = Utc::now();

    struct Fake {
        gmail: &'static str,
        thread: &'static str,
        from: &'static str,
        subject: &'static str,
        importance: u8,
        tier: Tier,
        one_line: &'static str,
        reason: &'static str,
        sensitivity: Sensitivity,
        sealed_kind: Option<SealedKind>,
    }

    let fakes = [
        Fake {
            gmail: "f1",
            thread: "t-pastdue",
            from: "billing@power-co.com",
            subject: "FINAL NOTICE: electricity bill overdue",
            importance: 98,
            tier: Tier::PastDue,
            one_line: "Electricity bill is 6 days past due ($142.10)",
            reason: "matched deadline + past_due",
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f2",
            thread: "t-deadline",
            from: "no-reply@irs.gov",
            subject: "Estimated tax payment due",
            importance: 88,
            tier: Tier::Deadline,
            one_line: "Quarterly estimated tax due in 4 days",
            reason: "deadline extracted",
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f3",
            thread: "t-signal-1",
            from: "alice@example.com",
            subject: "Re: lunch thursday?",
            importance: 74,
            tier: Tier::Signal,
            one_line: "Alice confirms lunch Thursday, asks where",
            reason: "known contact, direct reply",
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f4",
            thread: "t-signal-2",
            from: "recruiter@startup.io",
            subject: "Following up on our chat",
            importance: 61,
            tier: Tier::Signal,
            one_line: "Recruiter following up, wants a call next week",
            reason: "addressed to me, question",
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f5",
            thread: "t-noise-1",
            from: "deals@megastore.com",
            subject: "\u{1f525} 50% OFF everything today only",
            importance: 8,
            tier: Tier::Noise,
            one_line: "Marketing blast, 50% off promo",
            reason: "bulk sender, promotional",
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f6",
            thread: "t-noise-2",
            from: "newsletter@techdaily.com",
            subject: "Your Monday digest",
            importance: 5,
            tier: Tier::Noise,
            one_line: "Daily newsletter digest",
            reason: "newsletter, low signal",
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        // Sealed rows — must appear ONLY in the TUI, bodies hidden by default.
        Fake {
            gmail: "f7",
            thread: "t-sealed-otp",
            from: "security@bank.com",
            subject: "Your verification code is 481920",
            importance: 95,
            tier: Tier::Noise,
            one_line: "auth",
            reason: "seal detector: otp",
            sensitivity: Sensitivity::Sealed,
            sealed_kind: Some(SealedKind::Otp),
        },
        Fake {
            gmail: "f8",
            thread: "t-sealed-reset",
            from: "no-reply@github.com",
            subject: "Reset your password",
            importance: 90,
            tier: Tier::Noise,
            one_line: "auth",
            reason: "seal detector: password_reset",
            sensitivity: Sensitivity::Sealed,
            sealed_kind: Some(SealedKind::PasswordReset),
        },
    ];

    for fk in &fakes {
        let id = store.upsert_message(&NewMessage {
            account_id: account,
            gmail_msg_id: fk.gmail.to_string(),
            thread_id: fk.thread.to_string(),
            from_addr: fk.from.to_string(),
            from_name: None,
            subject: fk.subject.to_string(),
            received_at: now - Duration::hours(2),
            snippet: fk.one_line.to_string(),
            body: fk.subject.to_string(),
            is_sent: false,
        })?;
        store.set_triage(
            id,
            account,
            fk.importance,
            fk.tier,
            fk.sensitivity,
            fk.sealed_kind,
            fk.one_line,
            fk.reason,
            None,
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Terminal setup / teardown
// ---------------------------------------------------------------------------

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
