// Audit log: what the agent (/mcp door) and this app (/client door) have done.
// GET /client/audit, newest first, read-only; j/k selection expands a row.
// Mail-derived sender/subject render as text only, never markup. Sealed
// sender/subject are deliberately shown here (as on the Auth page), sealed
// content never is — see docs/SECURITY.md. An undo lands as a new audit row.

import SwiftUI

struct AuditView: View {
    @Environment(AppStore.self) private var store

    @State private var auditState: Loadable<[AuditEntry]> = .loading
    @State private var index = 0

    private static let inboxLabel = "INBOX"

    /// Newest first by ts, falling back to id — never trust server ordering.
    private var rows: [AuditEntry] {
        (auditState.value ?? []).sorted { a, b in
            let ta = Fmt.date(a.ts)
            let tb = Fmt.date(b.ts)
            if let ta, let tb, ta != tb { return ta > tb }
            return a.id > b.id
        }
    }

    var body: some View {
        Group {
            if auditState.isLoading {
                BandNote("loading audit…")
            } else if let error = auditState.error {
                BandNote(error)
            } else if rows.isEmpty {
                BandNote("No agent or app actions recorded yet.")
            } else {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 1) {
                            ForEach(Array(rows.enumerated()), id: \.element.id) { i, entry in
                                AuditRow(
                                    entry: entry, selected: i == index,
                                    undo: Self.undoFor(entry),
                                    onSelect: { index = i },
                                    onUndo: { Task { await performUndo(entry) } })
                                .id(entry.id)
                            }
                        }
                        .padding(.horizontal, 18)
                        .padding(.vertical, 10)
                    }
                    .onChange(of: index) { _, i in
                        guard let entry = rows[safe: i] else { return }
                        withAnimation(Motion.scrollFollow) {
                            proxy.scrollTo(entry.id, anchor: .center)
                        }
                    }
                }
                KeyHintBar(hints: [KeyHint(["j", "k"], "select")])
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .keyBindings(.modal, [
            KeyBinding("j", "next") { index = min(rows.count - 1, index + 1) },
            KeyBinding("k", "prev") { index = max(0, index - 1) },
        ])
        .task { await load() }
        .onChange(of: rows.count) { _, count in
            index = max(0, min(index, max(0, count - 1)))
        }
    }

    private func load() async {
        await $auditState.load("audit failed") {
            try await APIClient.shared.getAudit(limit: 200)
        }
    }

    private func performUndo(_ entry: AuditEntry) async {
        guard let spec = Self.undoFor(entry) else { return }
        do {
            try await spec.run()
            store.pushToast("undone: \(Self.actionVerb(entry))", .info)
            // The undo lands as its own audit row; re-pull so it shows.
            await load()
        } catch {
            store.pushToast(errText(error, "undo failed"), .error)
        }
    }

    // MARK: - readable entries

    /// Actors rendered as "the agent"; several spellings tolerated because the
    /// agent door's actor string isn't pinned.
    static func actorChip(_ actor: String) -> Chip {
        let lower = actor.lowercased()
        if ["agent", "mcp", "assistant", "ai"].contains(where: lower.hasPrefix) {
            return Chip(text: "Agent", tone: Palette.lock, filled: true)
        }
        if ["client-api", "client", "app", "user"].contains(lower) {
            return Chip(text: "You", tone: Palette.accent, filled: true)
        }
        // Unknown actor: show it verbatim rather than mislabeling.
        return Chip(text: actor.isEmpty ? "?" : actor, tone: Palette.inkFaint, filled: true)
    }

    /// Both dotted and underscore slug spellings, so a rename on either side
    /// degrades gracefully. set_status is detail-driven.
    private static let actionVerbs: [String: String] = [
        "archive": "archived",
        "label": "relabeled a message",
        "send.echo": "filed the sent copy into the thread",
        "reveal_sealed": "revealed auth message",
        "reveal": "revealed auth message",
        "unsubscribe": "opened unsubscribe",
        "unsub_resolution": "resolved unsubscribe prompt",
        "rule.create": "created a sender rule",
        "create_rule": "created a sender rule",
        "rule.update": "updated a sender rule",
        "update_rule": "updated a sender rule",
        "rule.delete": "deleted a sender rule",
        "delete_rule": "deleted a sender rule",
    ]

    static func actionVerb(_ e: AuditEntry) -> String {
        if e.action == "send" {
            // A reply audits with the parent message id as its target; a fresh
            // message has no target, and calling it a reply misreports the ledger.
            return (e.target ?? "").isEmpty ? "sent a message" : "sent a reply"
        }
        if e.action == "set_status" {
            switch (e.detail ?? "").lowercased() {
            case "done": return "marked done"
            case "open": return "reopened"
            case "new": return "reset to new"
            default: return "changed status"
            }
        }
        if let verb = actionVerbs[e.action] { return verb }
        // Tolerate namespaced variants like "rule.set.v2" → match on the prefix.
        if let dot = e.action.firstIndex(of: "."), String(e.action[..<dot]) == "rule" {
            return "changed a sender rule"
        }
        return e.action.isEmpty ? "did something" : e.action
    }

    struct UndoSpec {
        var label: String
        var run: () async throws -> Void
    }

    /// Strict decimal parse, mirroring the server's SQLite CAST: a permissive
    /// parse accepts hex/exponent forms CAST maps to 0, so an undo could fire
    /// against a different id than the row displayed.
    static func parseAuditId(_ raw: String?) -> Int? {
        guard let raw, !raw.isEmpty,
            raw.allSatisfy({ $0.isASCII && $0.isNumber }),
            let id = Int(raw), id > 0
        else {
            return nil
        }
        return id
    }

    /// The safe inverse for a row, or nil — only successful, reversible actions.
    static func undoFor(_ e: AuditEntry) -> UndoSpec? {
        if e.action == "archive", e.detail == "ok", let id = parseAuditId(e.target) {
            return UndoSpec(label: "restore") {
                try await APIClient.shared.actionLabel(id, add: [inboxLabel])
            }
        }
        if e.action == "set_status", e.detail == "done", let id = parseAuditId(e.target) {
            return UndoSpec(label: "reopen") {
                try await APIClient.shared.setStatus(id, .open)
            }
        }
        if e.action == "rule.create" || e.action == "create_rule" {
            // The new rule id arrives in `detail`; `target` is the pattern.
            if let id = parseAuditId(e.detail) {
                return UndoSpec(label: "delete rule") {
                    try await APIClient.shared.deleteRule(id)
                }
            }
        }
        return nil
    }
}

private struct AuditRow: View {
    let entry: AuditEntry
    let selected: Bool
    let undo: AuditView.UndoSpec?
    let onSelect: () -> Void
    let onUndo: () -> Void

    private var hasResolved: Bool {
        !(entry.target_sender ?? "").isEmpty || !(entry.target_subject ?? "").isEmpty
    }

    var body: some View {
        ListRow(
            selected: selected, cornerRadius: 8, hPadding: 11, vPadding: 6, action: onSelect
        ) { selected, _ in
            HStack(alignment: .top, spacing: 10) {
                AuditView.actorChip(entry.actor)
                    .frame(width: 58, alignment: .leading)
                Text(AuditView.actionVerb(entry))
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(width: 168, alignment: .leading)
                    .help(entry.action)

                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 5) {
                        if hasResolved {
                            if let sender = entry.target_sender, !sender.isEmpty {
                                Text(sender)
                                    .font(Typo.micro)
                                    .foregroundStyle(Palette.inkDim)
                            }
                            if let subject = entry.target_subject, !subject.isEmpty {
                                Text("·").foregroundStyle(Palette.inkFaintest)
                                Text(subject)
                                    .font(Typo.micro)
                                    .foregroundStyle(Palette.inkFaint)
                            }
                        } else if let target = entry.target {
                            Text(target).font(Typo.mono(10)).foregroundStyle(Palette.inkFaint)
                        }
                    }
                    .lineLimit(selected ? nil : 1)
                    if selected, let detail = entry.detail, !detail.isEmpty {
                        Text("— \(detail)")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                if let undo {
                    Button(undo.label, action: onUndo)
                        .buttonStyle(.glass)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.accent)
                        .help("undo — \(undo.label)")
                }
                Text(Fmt.relAge(entry.ts).isEmpty ? "now" : Fmt.relAge(entry.ts))
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 34, alignment: .trailing)
                    .help(entry.ts)
            }
        }
    }
}
