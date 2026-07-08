//! Rendering for the squelch TUI. Pure draw functions over `App` state.

use chrono::Utc;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use squelch_core::types::{Disposition, Tier};

use crate::app::{App, Mode, RuleField, Row};

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(1),    // list
            Constraint::Length(1), // footer / keys
        ])
        .split(f.area());

    render_header(f, app, chunks[0]);
    if app.empty {
        render_empty_state(f, chunks[1]);
    } else {
        render_list(f, app, chunks[1]);
    }
    render_footer(f, app, chunks[2]);

    match &app.mode {
        Mode::List => {}
        Mode::Detail { view, scroll } => render_detail(f, view.as_ref(), *scroll),
        Mode::RuleEdit(_) => render_rule_editor(f, app),
        Mode::RuleList { rules, scroll } => render_rule_list(f, rules, *scroll),
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
        Span::styled(format!("signal {signal}"), Style::default().fg(Color::Green)),
        Span::raw(" / "),
        Span::styled(format!("noise {noise}"), Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled(
            format!("sealed {}", app.sealed.len()),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("   "),
        Span::styled(
            format!("[squelch: {}]", app.min_importance),
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
    ]);
    let p = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" local debug viewer "),
    );
    f.render_widget(p, area);
}

fn render_empty_state(f: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No mail in the local store yet.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Get the sync daemon going:"),
        Line::from(Span::styled(
            "    1. squelchd auth     (authorize your Gmail account)",
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(
            "    2. squelchd run      (start the sync + triage daemon)",
            Style::default().fg(Color::Cyan),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  This viewer live-refreshes; mail appears here as it syncs.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  Or run with --demo to see seeded sample data.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" empty "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    let sel = App::selectable_indices(&rows);
    let selected_row_idx = sel.get(app.selected).copied();
    let width = area.width.saturating_sub(2) as usize;
    let now = Utc::now();

    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let is_selected = Some(i) == selected_row_idx;
        lines.push(render_row(row, is_selected, app.reveal_sealed, width, now));
    }

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_row(
    row: &Row,
    selected: bool,
    reveal_sealed: bool,
    width: usize,
    now: chrono::DateTime<Utc>,
) -> Line<'static> {
    let cursor = if selected { "> " } else { "  " };
    match row {
        Row::Update(u) => {
            let (glyph, color) = match u.tier {
                Tier::PastDue => ("!", Color::Red),
                Tier::Deadline => ("\u{25b2}", Color::Yellow),
                Tier::Signal => ("\u{2022}", Color::Green),
                Tier::Noise => ("\u{00b7}", Color::DarkGray),
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
            Line::from(Span::styled(crate::truncate(text, width), style))
        }
        Row::Sealed(s) => {
            let kind = s.sealed_kind.as_deref().unwrap_or("sealed");
            let body = if reveal_sealed {
                // Even revealed, the store hands the TUI metadata only
                // (sealed_messages is metadata-only), so we show the subject as
                // the most sensitive thing available and mark it revealed.
                // TODO(core): a local-only sealed body accessor would let us
                //   surface the actual code/body here.
                format!("REVEALED subject: {}", s.subject)
            } else {
                "•••••• (press r to reveal)".to_string()
            };
            let mut style = Style::default().fg(Color::Magenta);
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let rel = crate::app::relative_time(s.received_at, now);
            let text = format!("{cursor}\u{1f512} {} [{}] {} ({rel})", s.from_addr, kind, body);
            Line::from(Span::styled(crate::truncate(text, width), style))
        }
        Row::SquelchLine => {
            let dashes = "\u{2500}".repeat(width.saturating_sub(15).max(3));
            Line::from(Span::styled(
                format!("  \u{2500}\u{2500} squelch {dashes}"),
                Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ))
        }
        Row::NoiseSummary(n) => Line::from(Span::styled(
            format!("  \u{00b7} {n} below the line (press s to show)"),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )),
        Row::Header(h) => Line::from(Span::styled(
            format!("  {h}"),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )),
    }
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.mode {
        Mode::List => {
            " j/k move  Enter thread  t rule  T rules  +/- squelch  g refresh  s noise  r reveal  q quit "
        }
        Mode::Detail { .. } => " j/k scroll  Esc/q back ",
        Mode::RuleEdit(_) => " Tab field  Ctrl-S save  Esc cancel ",
        Mode::RuleList { .. } => " j/k scroll  Esc/q back ",
    };
    let line = Line::from(vec![Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_detail(f: &mut Frame, view: Option<&squelch_core::types::ThreadView>, scroll: u16) {
    let area = centered_rect(78, 70, f.area());
    f.render_widget(Clear, area);

    let mut text: Vec<Line> = Vec::new();
    match view {
        Some(v) => {
            text.push(Line::from(Span::styled(
                v.subject.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            text.push(Line::from(Span::styled(
                format!("thread {}", v.thread_id),
                Style::default().fg(Color::DarkGray),
            )));
            let now = Utc::now();
            for m in &v.messages {
                text.push(Line::from(""));
                let who = m.from_name.clone().unwrap_or_else(|| m.from_addr.clone());
                let rel = crate::app::relative_time(m.received_at, now);
                text.push(Line::from(Span::styled(
                    format!("{who}  <{}>  {rel} ago", m.from_addr),
                    Style::default().fg(Color::Cyan),
                )));
                for l in m.content.lines() {
                    text.push(Line::from(l.to_string()));
                }
            }
        }
        None => {
            text.push(Line::from(Span::styled(
                "no thread detail available",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" thread detail "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn render_rule_editor(f: &mut Frame, app: &App) {
    let Mode::RuleEdit(ed) = &app.mode else {
        return;
    };
    let area = centered_rect(70, 50, f.area());
    f.render_widget(Clear, area);

    let field_line = |label: &str, input: &crate::app::TextInput, active: bool| -> Line<'static> {
        let val = input.value();
        let mut spans = vec![Span::styled(
            format!("{label:>9}: "),
            Style::default().fg(if active { Color::Yellow } else { Color::DarkGray }),
        )];
        if active {
            // Render a simple block cursor by splitting the value at the cursor.
            let chars: Vec<char> = val.chars().collect();
            let c = input.cursor().min(chars.len());
            let before: String = chars[..c].iter().collect();
            let at = chars.get(c).copied().unwrap_or(' ');
            let after: String = if c < chars.len() {
                chars[c + 1..].iter().collect()
            } else {
                String::new()
            };
            spans.push(Span::raw(before));
            spans.push(Span::styled(
                at.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::raw(after));
        } else {
            spans.push(Span::raw(val));
        }
        Line::from(spans)
    };

    let disp_str = match ed.disposition {
        Disposition::Surface => "Surface",
        Disposition::Squelch => "Squelch",
        Disposition::Filtered => "Filtered",
    };
    let disp_active = ed.field == RuleField::Disposition;

    let mut text = vec![
        Line::from(Span::styled(
            "squelch profile — sender rule",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        field_line("pattern", &ed.pattern, ed.field == RuleField::Pattern),
        field_line("want", &ed.want, ed.field == RuleField::Want),
        Line::from(vec![
            Span::styled(
                "   disp: ",
                Style::default().fg(if disp_active { Color::Yellow } else { Color::DarkGray }),
            ),
            Span::styled(
                format!("[{disp_str}]"),
                if disp_active {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                },
            ),
            Span::styled("  (Tab to focus, Tab again cycles)", Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Ctrl-S save   Esc cancel   Tab next field",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(status) = &ed.status {
        text.push(Line::from(""));
        text.push(Line::from(Span::styled(
            status.clone(),
            Style::default().fg(Color::Green),
        )));
    }

    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" rule editor "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_rule_list(f: &mut Frame, rules: &[squelch_core::types::SenderRule], scroll: u16) {
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);

    let mut text: Vec<Line> = Vec::new();
    if rules.is_empty() {
        text.push(Line::from(Span::styled(
            "no sender rules yet",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for r in rules {
            let disp = match r.disposition {
                Disposition::Surface => ("Surface", Color::Green),
                Disposition::Squelch => ("Squelch", Color::Yellow),
                Disposition::Filtered => ("Filtered", Color::Red),
            };
            text.push(Line::from(vec![
                Span::styled(
                    format!("{:<8} ", disp.0),
                    Style::default().fg(disp.1).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<28} ", r.match_pattern),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(r.want_text.clone()),
            ]));
        }
    }

    let p = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" all sender rules (read-only audit) "),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);
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
