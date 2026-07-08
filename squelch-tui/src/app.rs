//! App state, data loading, and pure view logic for the squelch TUI.
//!
//! This is a LOCAL debug/setup viewer over `squelch-core`'s `Store`. It only
//! ever reads mail; the single write it performs is `Store::set_sender_rule`.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};

use squelch_core::store::{SealedMessage, SqliteStore, Store};
use squelch_core::types::{AccountId, Disposition, SenderRule, ThreadView, Tier, Update};

/// How far back to look when querying ranked updates. Local debug default.
const LOOKBACK_DAYS: i64 = 30;

/// Which modal/overlay (if any) is active on top of the list.
pub enum Mode {
    /// The plain ranked list.
    List,
    /// Thread detail drill-in. Holds the fetched view (or None if unavailable)
    /// plus the scroll offset inside the pane.
    Detail { view: Option<ThreadView>, scroll: u16 },
    /// Sender rule editor for a selected message.
    RuleEdit(RuleEditor),
    /// Read-only audit list of all existing sender rules.
    RuleList { rules: Vec<SenderRule>, scroll: u16 },
}

/// The active field in the rule editor (for line editing / cursor rendering).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RuleField {
    Pattern,
    Want,
    Disposition,
}

/// State for the sender-rule editor pane.
pub struct RuleEditor {
    pub pattern: TextInput,
    pub want: TextInput,
    pub disposition: Disposition,
    pub field: RuleField,
    /// Non-fatal status line (e.g. "saved" or an error) shown in the pane.
    pub status: Option<String>,
}

impl RuleEditor {
    /// Build an editor pre-filled from a sender address.
    pub fn for_sender(sender: &str) -> Self {
        Self {
            pattern: TextInput::from(&derive_pattern(sender)),
            want: TextInput::from(""),
            disposition: Disposition::Surface,
            field: RuleField::Pattern,
            status: None,
        }
    }

    /// Cycle the focused field (Tab).
    pub fn next_field(&mut self) {
        self.field = match self.field {
            RuleField::Pattern => RuleField::Want,
            RuleField::Want => RuleField::Disposition,
            RuleField::Disposition => RuleField::Pattern,
        };
    }

    /// Cycle disposition Surface -> Squelch -> Filtered -> Surface.
    pub fn cycle_disposition(&mut self) {
        self.disposition = match self.disposition {
            Disposition::Surface => Disposition::Squelch,
            Disposition::Squelch => Disposition::Filtered,
            Disposition::Filtered => Disposition::Surface,
        };
    }
}

/// A minimal single-line text editor (chars, backspace, left/right).
#[derive(Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    pub fn from(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.chars.remove(self.cursor - 1);
            self.cursor -= 1;
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }
}

/// A single rendered row in the list. Either a ranked update or a sealed stub.
pub enum Row {
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
    pub fn selectable(&self) -> bool {
        matches!(self, Row::Update(_) | Row::Sealed(_))
    }
}

/// Stable identity of the current selection, so it survives refreshes.
#[derive(Clone, PartialEq, Eq)]
pub enum SelId {
    Update(i64),
    Sealed(i64),
}

pub struct App {
    store: Arc<SqliteStore>,
    account: AccountId,
    pub updates: Vec<Update>,
    pub sealed: Vec<SealedMessage>,
    /// True when there is genuinely nothing in the store (empty-state pane).
    pub empty: bool,
    /// Index into the flattened selectable rows.
    pub selected: usize,
    /// Show noise tier below the squelch line.
    pub show_noise: bool,
    /// Reveal sealed content (subjects). Defaults to hidden.
    pub reveal_sealed: bool,
    /// In-session squelch threshold. Items below fall under the line.
    /// TODO(persist): persist to config instead of resetting each session.
    pub min_importance: u8,
    pub mode: Mode,
    pub quit: bool,
}

impl App {
    pub fn new(store: Arc<SqliteStore>, account: AccountId, min_importance: u8) -> Result<Self> {
        let mut app = Self {
            store,
            account,
            updates: Vec::new(),
            sealed: Vec::new(),
            empty: true,
            selected: 0,
            show_noise: false,
            reveal_sealed: false,
            min_importance,
            mode: Mode::List,
            quit: false,
        };
        app.refresh()?;
        Ok(app)
    }

    /// Re-query the store, preserving selection identity across the refresh.
    pub fn refresh(&mut self) -> Result<()> {
        let prev = self.selected_id();

        let since = Utc::now() - Duration::days(LOOKBACK_DAYS);
        self.updates = self.store.ranked_updates(self.account, since, None)?;
        self.sealed = self.store.sealed_messages(self.account)?;
        self.empty = self.updates.is_empty() && self.sealed.is_empty();

        // Re-anchor selection to the same message/sealed id if it still exists.
        if let Some(prev) = prev {
            let rows = self.rows();
            let sel = Self::selectable_indices(&rows);
            if let Some(new_pos) = sel.iter().enumerate().find_map(|(pos, &ri)| {
                (row_id(&rows[ri]) == Some(prev.clone())).then_some(pos)
            }) {
                self.selected = new_pos;
            } else {
                self.clamp_selection();
            }
        } else {
            self.clamp_selection();
        }
        Ok(())
    }

    pub fn signal_count(&self) -> usize {
        self.updates
            .iter()
            .filter(|u| self.above_line(u))
            .count()
    }

    pub fn noise_count(&self) -> usize {
        self.updates
            .iter()
            .filter(|u| !self.above_line(u))
            .count()
    }

    /// Whether an update sits ABOVE the squelch line. PastDue and Deadline
    /// tiers ALWAYS stay above (they bypass the threshold by design); everything
    /// else must clear the in-session `min_importance`.
    pub fn above_line(&self, u: &Update) -> bool {
        above_line(u, self.min_importance)
    }

    /// Build the flattened, ordered list of rows for the current view state.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();

        // Pinned tiers first, in priority order, always above the line.
        let pinned = |t: Tier| self.updates.iter().filter(move |u| u.tier == t);
        for u in pinned(Tier::PastDue) {
            rows.push(Row::Update(u.clone()));
        }
        for u in pinned(Tier::Deadline) {
            rows.push(Row::Update(u.clone()));
        }
        // Remaining above-line updates (Signal/Noise that clear the threshold),
        // most important first.
        let mut above: Vec<&Update> = self
            .updates
            .iter()
            .filter(|u| !matches!(u.tier, Tier::PastDue | Tier::Deadline) && self.above_line(u))
            .collect();
        above.sort_by(|a, b| b.importance.cmp(&a.importance));
        for u in above {
            rows.push(Row::Update(u.clone()));
        }

        rows.push(Row::SquelchLine);

        // Below-line updates (fell under the threshold).
        let mut below: Vec<&Update> = self
            .updates
            .iter()
            .filter(|u| !matches!(u.tier, Tier::PastDue | Tier::Deadline) && !self.above_line(u))
            .collect();
        below.sort_by(|a, b| b.importance.cmp(&a.importance));

        if self.show_noise {
            for u in below {
                rows.push(Row::Update(u.clone()));
            }
        } else if !below.is_empty() {
            rows.push(Row::NoiseSummary(below.len()));
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

    pub fn selectable_indices(rows: &[Row]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.selectable())
            .map(|(i, _)| i)
            .collect()
    }

    pub fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        let sel = Self::selectable_indices(&rows);
        if sel.is_empty() {
            self.selected = 0;
            return;
        }
        let n = sel.len() as isize;
        let cur = self.selected as isize;
        self.selected = ((cur + delta).rem_euclid(n)) as usize;
    }

    fn clamp_selection(&mut self) {
        let rows = self.rows();
        let n = Self::selectable_indices(&rows).len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// Stable id of the current selection (for refresh re-anchoring).
    pub fn selected_id(&self) -> Option<SelId> {
        let rows = self.rows();
        let sel = Self::selectable_indices(&rows);
        let idx = *sel.get(self.selected)?;
        row_id(&rows[idx])
    }

    /// The selected update, if the selection is a (non-sealed) update.
    pub fn selected_update(&self) -> Option<Update> {
        let rows = self.rows();
        let sel = Self::selectable_indices(&rows);
        let idx = *sel.get(self.selected)?;
        match &rows[idx] {
            Row::Update(u) => Some(u.clone()),
            _ => None,
        }
    }

    /// True when the current selection is a sealed row.
    pub fn selected_is_sealed(&self) -> bool {
        let rows = self.rows();
        let sel = Self::selectable_indices(&rows);
        matches!(sel.get(self.selected).map(|&i| &rows[i]), Some(Row::Sealed(_)))
    }

    pub fn adjust_threshold(&mut self, delta: i16) {
        let next = (self.min_importance as i16 + delta).clamp(0, 100) as u8;
        self.min_importance = next;
        self.move_selection(0);
    }

    // --- actions that touch the store -------------------------------------

    /// Open the thread detail pane for the selected update via `thread_view`.
    /// NotFound (also what sealed threads return) degrades to an empty pane.
    pub fn open_detail(&mut self) {
        let Some(u) = self.selected_update() else {
            return;
        };
        // NotFound (also what sealed threads return) / any error: no detail.
        let view = self.store.thread_view(self.account, &u.thread_id).ok();
        self.mode = Mode::Detail { view, scroll: 0 };
    }

    /// Open the rule editor pre-filled from the selected update's sender.
    pub fn open_rule_editor(&mut self) {
        if let Some(u) = self.selected_update() {
            self.mode = Mode::RuleEdit(RuleEditor::for_sender(&u.sender));
        }
    }

    /// Open the read-only rules audit list.
    pub fn open_rule_list(&mut self) {
        let rules = self.store.list_sender_rules(self.account).unwrap_or_default();
        self.mode = Mode::RuleList { rules, scroll: 0 };
    }

    /// Persist the current rule editor via `set_sender_rule` (the one write).
    pub fn save_rule(&mut self) {
        let Mode::RuleEdit(ed) = &mut self.mode else {
            return;
        };
        let pattern = ed.pattern.value();
        if pattern.trim().is_empty() {
            ed.status = Some("match pattern cannot be empty".to_string());
            return;
        }
        let want = ed.want.value();
        let disposition = ed.disposition;
        match self
            .store
            .set_sender_rule(self.account, pattern.trim(), &want, disposition)
        {
            Ok(_) => {
                if let Mode::RuleEdit(ed) = &mut self.mode {
                    ed.status = Some(format!("saved rule for {}", pattern.trim()));
                }
            }
            Err(e) => {
                if let Mode::RuleEdit(ed) = &mut self.mode {
                    ed.status = Some(format!("save failed: {e}"));
                }
            }
        }
    }
}

/// Stable identity of a selectable row.
fn row_id(row: &Row) -> Option<SelId> {
    match row {
        Row::Update(u) => Some(SelId::Update(u.id)),
        Row::Sealed(s) => Some(SelId::Sealed(s.id)),
        _ => None,
    }
}

/// Pure: whether an update sits above the squelch line for a given threshold.
/// PastDue/Deadline always stay above regardless of threshold.
pub fn above_line(u: &Update, min_importance: u8) -> bool {
    matches!(u.tier, Tier::PastDue | Tier::Deadline) || u.importance >= min_importance
}

/// Derive a match pattern from a sender address: `*@domain` when possible,
/// otherwise the raw address (already-a-pattern strings pass through).
pub fn derive_pattern(sender: &str) -> String {
    // Strip a display-name wrapper like "Alice <alice@x.com>" if present.
    let addr = sender
        .rsplit_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .map(str::trim)
        .unwrap_or(sender)
        .trim();
    match addr.rsplit_once('@') {
        Some((_, domain)) if !domain.is_empty() => format!("*@{domain}"),
        _ => addr.to_string(),
    }
}

/// Format an instant as a compact relative age: `now`, `5m`, `2h`, `3d`, `4w`.
pub fn relative_time(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    if secs < 0 {
        return "now".to_string();
    }
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    let weeks = days / 7;
    if mins < 1 {
        "now".to_string()
    } else if mins < 60 {
        format!("{mins}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else if days < 7 {
        format!("{days}d")
    } else {
        format!("{weeks}w")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(id: i64, tier: Tier, importance: u8) -> Update {
        Update {
            id,
            thread_id: format!("t{id}"),
            tier,
            importance,
            sender: "x@y.com".to_string(),
            one_line: "line".to_string(),
            reason: "r".to_string(),
            deadline: None,
            matched_rule: None,
        }
    }

    #[test]
    fn past_due_and_deadline_stay_above_line_regardless_of_threshold() {
        let past = upd(1, Tier::PastDue, 0);
        let dead = upd(2, Tier::Deadline, 3);
        // Even with the threshold maxed out, pinned tiers stay above.
        assert!(above_line(&past, 100));
        assert!(above_line(&dead, 100));
    }

    #[test]
    fn signal_partitions_on_threshold() {
        let hi = upd(3, Tier::Signal, 70);
        let lo = upd(4, Tier::Signal, 40);
        assert!(above_line(&hi, 50));
        assert!(!above_line(&lo, 50));
        // Exactly at the threshold counts as above.
        assert!(above_line(&upd(5, Tier::Signal, 50), 50));
    }

    #[test]
    fn derive_pattern_from_plain_addr() {
        assert_eq!(derive_pattern("alice@example.com"), "*@example.com");
    }

    #[test]
    fn derive_pattern_from_display_name_wrapper() {
        assert_eq!(
            derive_pattern("Alice Smith <alice@theirdomain.com>"),
            "*@theirdomain.com"
        );
    }

    #[test]
    fn derive_pattern_without_at_passes_through() {
        assert_eq!(derive_pattern("weirdsender"), "weirdsender");
    }

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(now - Duration::minutes(5), now), "5m");
        assert_eq!(relative_time(now - Duration::hours(2), now), "2h");
        assert_eq!(relative_time(now - Duration::days(3), now), "3d");
        assert_eq!(relative_time(now - Duration::days(21), now), "3w");
    }

    #[test]
    fn text_input_editing() {
        let mut t = TextInput::from("ab");
        t.insert('c'); // "abc", cursor at 3
        assert_eq!(t.value(), "abc");
        t.left();
        t.insert('X'); // "abXc"
        assert_eq!(t.value(), "abXc");
        t.backspace(); // "abc"
        assert_eq!(t.value(), "abc");
        assert_eq!(t.cursor(), 2);
    }
}
