//! Key handling. Translates crossterm key events into `App` state changes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Mode, RuleField};

/// Dispatch a single key press based on the current mode.
pub fn handle_key(app: &mut App, key: KeyEvent) {
    match &app.mode {
        Mode::List => handle_list(app, key),
        Mode::Detail { .. } => handle_scroll_pane(app, key),
        Mode::RuleList { .. } => handle_scroll_pane(app, key),
        Mode::RuleEdit(_) => handle_rule_edit(app, key),
    }
}

fn handle_list(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('s') => {
            app.show_noise = !app.show_noise;
            app.move_selection(0);
        }
        KeyCode::Char('r') => app.reveal_sealed = !app.reveal_sealed,
        KeyCode::Char('g') => {
            let _ = app.refresh();
        }
        KeyCode::Char('+') | KeyCode::Char('=') => app.adjust_threshold(5),
        KeyCode::Char('-') | KeyCode::Char('_') => app.adjust_threshold(-5),
        KeyCode::Char('t') => {
            if !app.selected_is_sealed() {
                app.open_rule_editor();
            }
        }
        KeyCode::Char('T') => app.open_rule_list(),
        KeyCode::Enter => {
            if !app.selected_is_sealed() {
                app.open_detail();
            }
        }
        _ => {}
    }
}

/// Shared handler for scrollable read-only panes (detail, rule list).
fn handle_scroll_pane(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.mode = Mode::List,
        KeyCode::Char('j') | KeyCode::Down => scroll(app, 1),
        KeyCode::Char('k') | KeyCode::Up => scroll(app, -1),
        _ => {}
    }
}

fn scroll(app: &mut App, delta: i16) {
    let s = match &mut app.mode {
        Mode::Detail { scroll, .. } => scroll,
        Mode::RuleList { scroll, .. } => scroll,
        _ => return,
    };
    *s = (*s as i16 + delta).max(0) as u16;
}

fn handle_rule_edit(app: &mut App, key: KeyEvent) {
    // Ctrl-S saves regardless of the focused field.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
    {
        app.save_rule();
        return;
    }

    let Mode::RuleEdit(ed) = &mut app.mode else {
        return;
    };

    match key.code {
        KeyCode::Esc => app.mode = Mode::List,
        KeyCode::Tab => ed.next_field(),
        KeyCode::Enter => {
            // Enter on the disposition field acts as the "save button-line".
            if ed.field == RuleField::Disposition {
                app.save_rule();
            } else {
                ed.next_field();
            }
        }
        _ => match ed.field {
            RuleField::Disposition => {
                // Space or arrows cycle the disposition.
                if matches!(
                    key.code,
                    KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                ) {
                    ed.cycle_disposition();
                }
            }
            RuleField::Pattern | RuleField::Want => {
                let input = if ed.field == RuleField::Pattern {
                    &mut ed.pattern
                } else {
                    &mut ed.want
                };
                match key.code {
                    KeyCode::Char(c) => input.insert(c),
                    KeyCode::Backspace => input.backspace(),
                    KeyCode::Left => input.left(),
                    KeyCode::Right => input.right(),
                    _ => {}
                }
            }
        },
    }
}
