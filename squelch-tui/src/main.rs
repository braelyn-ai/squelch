//! squelch-tui: a local debug/setup viewer over the real store (the same
//! SQLite db the sync daemon and MCP server use), live-refreshed on a tick.
//! Read-only toward mail; the single write is `Store::set_sender_rule`.
//! The only surface that shows sealed messages at all — local-only
//! `Store::sealed_messages`, metadata hidden until the operator reveals it.

mod app;
mod input;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use anyhow::Result;
use chrono::{Duration, Utc};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;

use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::AccountId;

use crate::app::App;

/// Starting in-session squelch threshold. TODO(persist): read from config.
const DEFAULT_MIN_IMPORTANCE: u8 = 50;

/// Live-refresh cadence.
const TICK: StdDuration = StdDuration::from_secs(2);

/// Account this viewer operates on. Goes through core config's resolver so
/// every binary points at the same account.
fn account_email() -> String {
    squelch_core::config::resolve_account_email("me@localhost")
}

/// SQLite path from core config, so the TUI, MCP server, and daemon all open
/// the same db.
fn db_path() -> PathBuf {
    squelch_core::config::resolve_db_path()
}

fn main() -> Result<()> {
    let demo = std::env::args().any(|a| a == "--demo");

    let store = if demo {
        let store = SqliteStore::open_in_memory()?;
        let account = store.ensure_account(&account_email())?;
        seed_fake_data(&store, account)?;
        Arc::new(store)
    } else {
        Arc::new(SqliteStore::open(db_path())?)
    };

    let account = store.ensure_account(&account_email())?;
    let mut app = App::new(store, account, DEFAULT_MIN_IMPORTANCE)?;

    let mut terminal = init_terminal()?;
    let res = run(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    res
}

fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let mut last_tick = Instant::now();
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            input::handle_key(app, key);
        }
        if last_tick.elapsed() >= TICK {
            // List view only: refreshing under an open modal yanks its data out.
            if matches!(app.mode, app::Mode::List) {
                let _ = app.refresh();
            }
            last_tick = Instant::now();
        }
    }
    Ok(())
}

/// Truncate to a display width, appending an ellipsis when cut.
pub fn truncate(mut s: String, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() > width {
        s = s.chars().take(width.saturating_sub(1)).collect();
        s.push('\u{2026}');
    }
    s
}

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

/// Seeds fake rows across every tier plus sealed messages so `--demo` renders
/// something without a real store.
fn seed_fake_data(store: &SqliteStore, account: AccountId) -> Result<()> {
    use squelch_core::types::{NewMessage, SealedKind, Sensitivity, Tier};

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
        body: &'static str,
        age_hours: i64,
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
            body: "Your account is past due. Please remit $142.10 immediately to avoid disconnection.",
            age_hours: 2,
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f2",
            thread: "t-deadline",
            from: "no-reply@irs.gov",
            subject: "Estimated tax payment due",
            importance: 40,
            tier: Tier::Deadline,
            one_line: "Quarterly estimated tax due in 4 days",
            reason: "deadline extracted",
            body: "Your quarterly estimated tax payment is due on the 15th.",
            age_hours: 30,
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
            body: "Thursday works! Where were you thinking? I'm flexible after 12.",
            age_hours: 5,
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
            body: "Great chatting earlier. Do you have time for a call next week?",
            age_hours: 26,
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
            body: "Everything is half off today. Shop now!",
            age_hours: 50,
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
            body: "Here's what happened in tech today...",
            age_hours: 100,
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
        },
        Fake {
            gmail: "f7",
            thread: "t-sealed-otp",
            from: "security@bank.com",
            subject: "Your verification code is 481920",
            importance: 95,
            tier: Tier::Noise,
            one_line: "auth",
            reason: "seal detector: otp",
            body: "Your code is 481920.",
            age_hours: 1,
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
            body: "Click here to reset your password.",
            age_hours: 3,
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
            received_at: now - Duration::hours(fk.age_hours),
            snippet: fk.one_line.to_string(),
            body: fk.body.to_string(),
            body_html: None,
            is_sent: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
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
