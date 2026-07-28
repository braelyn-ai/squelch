// AUDIT LOG. Human review of what the AI agent (over the /mcp door) and this
// app (the /client door) have done. GET /client/audit, newest first. Read-only
// scrollback with j/k selection: the selected row expands its full target +
// detail; everything else truncates.
//
// READABLE ENTRIES: action slugs map to verb phrases and, when the server
// resolved the target to a message, we render "{sender} · {subject}" (text
// only — mail-derived, so never as markup) in place of the raw numeric target.
// Sealed sender/subject appearing here is a deliberate accepted decision (the
// Auth page already shows those to the human); sealed CONTENT never reaches
// this surface.
//
// UNDO: rows whose action has a safe inverse AND whose detail marks success get
// a small undo button. The undo itself lands as a NEW audit row, which is
// correct and desired.
//
// Ported from squelch-desktop/src/components/AuditView.tsx.

import SwiftUI

struct AuditView: View {
    @Environment(AppStore.self) private var store
    @Namespace private var auditGlass

    @State private var entries: [AuditEntry] = []
    @State private var error: String?
    @State private var loading = true
    @State private var index = 0

    private static let inboxLabel = "INBOX"

    /// Newest first — sort defensively by ts (falling back to id) so we don't
    /// depend on the server's ordering.
    private var rows: [AuditEntry] {
        entries.sorted { a, b in
            let ta = Fmt.date(a.ts)
            let tb = Fmt.date(b.ts)
            if let ta, let tb, ta != tb { return ta > tb }
            return a.id > b.id
        }
    }

    var body: some View {
        Group {
            if loading {
                BandNote("loading audit…")
            } else if let error {
                BandNote(error)
            } else if rows.isEmpty {
                BandNote("No agent or app actions recorded yet.")
            } else {
                ScrollViewReader { proxy in
                    ScrollView {
                        GlassEffectContainer(spacing: 2) {
                        LazyVStack(spacing: 1) {
                            ForEach(Array(rows.enumerated()), id: \.element.id) { i, entry in
                                AuditRow(
                                    glassNamespace: auditGlass,
                                    entry: entry, selected: i == index,
                                    undo: Self.undoFor(entry),
                                    onSelect: { index = i },
                                    onUndo: { Task { await performUndo(entry) } })
                                .id(entry.id)
                            }
                        }
                        }
                        .padding(.horizontal, 18)
                        .padding(.vertical, 10)
                    }
                    .onChange(of: index) { _, i in
                        guard let entry = rows[safe: i] else { return }
                        withAnimation(.easeOut(duration: 0.12)) {
                            proxy.scrollTo(entry.id, anchor: .center)
                        }
                    }
                }
                HStack(spacing: 4) {
                    Kbd("j"); Kbd("k")
                    Text("select").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Spacer()
                }
                .padding(.horizontal, 22).padding(.vertical, 10)
                .overlay(alignment: .top) { Rectangle().fill(Palette.hairline).frame(height: 0.5) }
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
        loading = true
        defer { loading = false }
        do {
            entries = try await APIClient.shared.getAudit(limit: 200)
            error = nil
        } catch {
            self.error = errText(error, "audit failed")
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

    struct ActorChip {
        var label: String
        var tone: Color
    }

    /// Actors we treat as "the agent" (visually distinct). The server-side agent
    /// door is still landing, so tolerate a few likely spellings.
    static func actorChip(_ actor: String) -> ActorChip {
        let lower = actor.lowercased()
        if ["agent", "mcp", "assistant", "ai"].contains(where: lower.hasPrefix) {
            return ActorChip(label: "Agent", tone: Palette.lock)
        }
        if ["client-api", "client", "app", "user"].contains(lower) {
            return ActorChip(label: "You", tone: Palette.accent)
        }
        // Unknown actor: show it verbatim rather than mislabeling.
        return ActorChip(label: actor.isEmpty ? "?" : actor, tone: Palette.inkFaint)
    }

    /// Map raw action slugs to a readable verb phrase. Covers the dotted server
    /// slugs and their underscore variants so a rename on either side degrades
    /// gracefully. set_status is detail-driven.
    private static let actionVerbs: [String: String] = [
        "archive": "archived",
        "label": "relabeled a message",
        "send": "sent a reply",
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

    /// An available undo: a human label + the exact inverse call.
    struct UndoSpec {
        var label: String
        var run: () async throws -> Void
    }

    /// Strict decimal id parse for audit targets — mirrors the server's SQLite
    /// CAST semantics. A permissive parse would accept hex/exponent forms that
    /// CAST maps to 0, so an undo could fire against a DIFFERENT id than the row
    /// displayed. Digits-only, positive, in-range.
    static func parseAuditId(_ raw: String?) -> Int? {
        guard let raw, !raw.isEmpty,
            raw.allSatisfy({ $0.isASCII && $0.isNumber }),
            let id = Int(raw), id > 0
        else {
            return nil
        }
        return id
    }

    /// The safe inverse for a row, or nil. Only SUCCESSFUL rows with a
    /// reversible action qualify.
    static func undoFor(_ e: AuditEntry) -> UndoSpec? {
        if e.action == "archive", e.detail == "ok", let id = parseAuditId(e.target) {
            // Same inverse the 5s archive undo toast fires.
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
            // handlers.rs stores the created rule id in `detail` (target is the
            // pattern). Only offer undo when that id is actually parseable.
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
    let glassNamespace: Namespace.ID
    let entry: AuditEntry
    let selected: Bool
    let undo: AuditView.UndoSpec?
    let onSelect: () -> Void
    let onUndo: () -> Void

    @State private var hovering = false

    private var hasResolved: Bool {
        !(entry.target_sender ?? "").isEmpty || !(entry.target_subject ?? "").isEmpty
    }

    var body: some View {
        let chip = AuditView.actorChip(entry.actor)
        Button(action: onSelect) {
            HStack(alignment: .top, spacing: 10) {
                Chip(text: chip.label, tone: chip.tone, filled: true)
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
            .padding(.horizontal, 11)
            .padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .selectionGlass(
            selected, hovering: hovering, cornerRadius: 8,
            id: "audit-selection", in: glassNamespace)
        .onHover { hovering = $0 }
    }
}
