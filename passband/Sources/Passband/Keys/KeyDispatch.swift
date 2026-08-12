// The app's single keymap registry: views register bindings scoped to a
// KeyContext, and the active context is a STACK (last pushed wins). The
// layering semantics are load-bearing:
//
//   * The active context composes with "global", so 1..5 view nav and ⌘K keep
//     firing from inside a modal.
//   * The LAST registered set in a context wins, which is how a nested
//     overlay's Escape beats the surface underneath.
//   * Two passes: an EXACT (case-sensitive) match beats a case-folded one, so
//     "A" (audit) and "a" (browse) coexist while a shifted letter with no
//     exact-case binding still falls back to its lowercase sibling.
//   * meta (⌘) is matched EXACTLY both ways, which keeps ⌘[ / ⌘] from
//     colliding with bare "[" / "]".
//   * A handler returning false DECLINES the key and dispatch continues to the
//     next candidate.

import SwiftUI

/// Which layer is receiving keys. "thread" (the fullscreen viewer) sits ABOVE
/// "modal", so a thread opened from a search panel gates that panel's keys
/// until the viewer closes.
enum KeyContext: String, Sendable, Hashable {
    case list
    case sitrep
    case modal
    case thread
    case input
    case global
}

/// One bound key. `handler` returns false to DECLINE (dispatch continues).
struct KeyBinding: Identifiable {
    /// The base key: a character ("j"), or a named key ("Escape", "ArrowDown",
    /// "Enter", "Space", "Tab"). Modifiers are expressed by the flags below,
    /// never baked into this string — except "shift+" on NAMED keys.
    var key: String
    var description: String
    /// Require the ⌘ modifier. Matched exactly (see file header).
    var meta: Bool = false
    /// Fire even when a text field has focus (chords and Escape opt in).
    var allowInInput: Bool = false
    var handler: () -> Bool

    let id = UUID()

    init(
        _ key: String, _ description: String, meta: Bool = false, allowInInput: Bool = false,
        handler: @escaping () -> Void
    ) {
        self.key = key
        self.description = description
        self.meta = meta
        self.allowInInput = allowInInput
        self.handler = {
            handler()
            return true
        }
    }

    /// Declining variant: the handler returns false to pass the key on.
    init(
        declining key: String, _ description: String, meta: Bool = false,
        allowInInput: Bool = false, handler: @escaping () -> Bool
    ) {
        self.key = key
        self.description = description
        self.meta = meta
        self.allowInInput = allowInInput
        self.handler = handler
    }
}

/// A minimal view of the keyboard event the pure core needs.
struct KeyEventLike {
    var key: String
    var control = false
    var option = false
    var command = false
    var shift = false
}

struct DispatchOutcome {
    var handled: Bool
    var firedKey: String?
    var firedContext: KeyContext?

    static let unhandled = DispatchOutcome(handled: false, firedKey: nil, firedContext: nil)
}

@MainActor
@Observable
final class KeyRegistry {
    static let shared = KeyRegistry()

    /// A live, mutable binding list. Views hand the registry a BOX rather than
    /// an array so a re-render can swap in closures that capture fresh state
    /// WITHOUT re-registering — re-registering would move the set to the top of
    /// the order and let a stale surface steal keys from an overlay above it.
    @MainActor
    final class BindingsBox {
        var bindings: [KeyBinding]
        init(_ bindings: [KeyBinding]) { self.bindings = bindings }
    }

    private struct RegisteredSet {
        var id: UUID
        var seq: Int
        var context: KeyContext
        var box: BindingsBox
    }

    private var sets: [RegisteredSet] = []
    /// Context stack: the topmost context is the only one that receives keys
    /// (plus "global", which always sees them last). Views push/pop as they
    /// mount. Entries carry a token so an out-of-order pop removes the right one.
    private var contextStack: [(token: UUID, context: KeyContext)] = [
        (UUID(), .list)
    ]
    private var seq = 0

    private init() {}

    // MARK: - registry API

    /// Push a context onto the stack; returns the token to pop with.
    @discardableResult
    func pushContext(_ ctx: KeyContext) -> UUID {
        let token = UUID()
        contextStack.append((token, ctx))
        return token
    }

    func popContext(_ token: UUID) {
        // Never pop the base "list" entry at index 0.
        if let idx = contextStack.lastIndex(where: { $0.token == token }), idx > 0 {
            contextStack.remove(at: idx)
        }
    }

    /// The context currently receiving keys.
    var currentContext: KeyContext { contextStack.last?.context ?? .list }

    /// Register a live binding box for a context; returns the token to
    /// unregister with.
    @discardableResult
    func register(_ context: KeyContext, box: BindingsBox) -> UUID {
        let id = UUID()
        seq += 1
        sets.append(RegisteredSet(id: id, seq: seq, context: context, box: box))
        return id
    }

    func unregister(_ id: UUID) {
        sets.removeAll { $0.id == id }
    }

    /// All currently-registered bindings, for the help overlay.
    func allBindings() -> [(context: KeyContext, binding: KeyBinding)] {
        sets.flatMap { set in set.box.bindings.map { (set.context, $0) } }
    }

    // MARK: - dispatch

    /// The pure dispatch algorithm. Walks the active context (then global),
    /// LATEST registration first, and fires the first matching,
    /// non-input-suppressed binding.
    func dispatch(_ event: KeyEventLike, editing: Bool) -> DispatchOutcome {
        let eventStr = Self.normalize(event)
        let metaHeld = event.command
        let ctx = currentContext
        let order: [KeyContext] = ctx == .global ? [.global] : [ctx, .global]

        let matchers: [(String, String) -> Bool] = [
            { $0 == $1 },                                    // EXACT wins
            { $0.lowercased() == $1.lowercased() },          // case-folded fallback
        ]

        for matcher in matchers {
            for context in order {
                for set in sets.sorted(by: { $0.seq > $1.seq }) where set.context == context {
                    for b in set.box.bindings {
                        guard b.meta == metaHeld else { continue }
                        guard matcher(b.key, eventStr) else { continue }
                        // Escape is NEVER input-suppressed: "Esc closes this" is
                        // the app's universal contract, so a surface that
                        // focuses a text field must not become a trap.
                        if editing && !b.allowInInput && b.key != "Escape" { continue }
                        if b.handler() {
                            return DispatchOutcome(
                                handled: true, firedKey: b.key, firedContext: context)
                        }
                    }
                }
            }
        }
        return .unhandled
    }

    /// Build the normalized key string. meta (⌘) is intentionally NOT baked in:
    /// it is matched separately via each binding's `meta` flag, so a bare-key
    /// binding never fires under ⌘ and a meta chord fires only under ⌘.
    static func normalize(_ e: KeyEventLike) -> String {
        var parts: [String] = []
        if e.control { parts.append("ctrl") }
        if e.option { parts.append("alt") }
        var k = e.key
        if k == " " { k = "Space" }
        // Shift is only expressed for NAMED keys; for letters the case of `key`
        // already carries it.
        if e.shift && k.count > 1 { parts.append("shift") }
        parts.append(k)
        return parts.joined(separator: "+")
    }
}

// THE APPKIT EVENT SOURCE. Everything above is pure — the registry, the
// contexts, the dispatch algorithm — and everything below it is SwiftUI, so
// the 21 view files that register bindings compile anywhere. Only the source
// of the events is AppKit, and on a platform without one the registry simply
// never gets asked: bindings register into the void until a twin exists.
#if os(macOS)
    // MARK: - NSEvent bridge

    /// Translate an AppKit key event into the browser-style key names bindings are
    /// written against.
    enum KeyNames {
        static func name(for event: NSEvent) -> String? {
            // Named keys first, by key code — `characters` for these is a private
            // Unicode escape, not something bindings can be written against.
            switch event.keyCode {
            case 53: return "Escape"
            case 36, 76: return "Enter"       // Return, keypad Enter
            case 48: return "Tab"
            case 49: return "Space"
            case 51: return "Backspace"
            case 117: return "Delete"
            case 123: return "ArrowLeft"
            case 124: return "ArrowRight"
            case 125: return "ArrowDown"
            case 126: return "ArrowUp"
            case 115: return "Home"
            case 119: return "End"
            case 116: return "PageUp"
            case 121: return "PageDown"
            default: break
            }
            // Printable characters keep their case — that IS the shift signal for
            // letters. `charactersIgnoringModifiers` so an Option chord still
            // reports the base glyph, then shift casing is re-applied.
            guard let raw = event.charactersIgnoringModifiers, !raw.isEmpty else { return nil }
            if raw.count == 1, let ch = raw.first, ch.isLetter {
                return event.modifierFlags.contains(.shift) ? raw.uppercased() : raw.lowercased()
            }
            // For non-letters, `characters` reflects the shifted glyph ("?" for
            // shift+/), which is what the bindings are written against.
            if let chars = event.characters, !chars.isEmpty, chars.count == 1 { return chars }
            return raw
        }

        static func eventLike(_ event: NSEvent) -> KeyEventLike? {
            guard let key = name(for: event) else { return nil }
            let f = event.modifierFlags
            return KeyEventLike(
                key: key,
                control: f.contains(.control),
                option: f.contains(.option),
                command: f.contains(.command),
                shift: f.contains(.shift))
        }
    }

    /// Installs the ONE global key monitor. Sits at the app level (PassbandApp) so
    /// there is exactly one listener.
    @MainActor
    final class KeyMonitor {
        static let shared = KeyMonitor()
        private var monitor: Any?

        private init() {}

        func install() {
            guard monitor == nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
                guard let like = KeyNames.eventLike(event) else { return event }
                // The input guard: typing into a text field suppresses single-letter
                // bindings unless the binding opts in.
                let editing = Self.isEditing(event.window)
                let outcome = KeyRegistry.shared.dispatch(like, editing: editing)
                // Returning nil consumes the event.
                return outcome.handled ? nil : event
            }
        }

        /// True when first responder is a text-entry view.
        private static func isEditing(_ window: NSWindow?) -> Bool {
            guard let responder = window?.firstResponder else { return false }
            if let textView = responder as? NSTextView { return textView.isEditable }
            return responder is NSTextField
        }
    }
#endif

// MARK: - SwiftUI attachment

private struct KeyBindingsModifier: ViewModifier {
    let context: KeyContext
    let bindings: [KeyBinding]
    @State private var box = KeyRegistry.BindingsBox([])
    @State private var token: UUID?

    func body(content: Content) -> some View {
        // Swap in the freshest closures on EVERY body evaluation — they capture
        // the view's current state, and the box is a plain non-observable
        // reference, so writing to it cannot re-enter the render.
        box.bindings = bindings
        return
            content
            .onAppear {
                if token == nil { token = KeyRegistry.shared.register(context, box: box) }
            }
            .onDisappear {
                if let token { KeyRegistry.shared.unregister(token) }
                token = nil
            }
    }
}

/// Pushes a KeyContext while the view is mounted, so its bindings capture keys
/// above the layer beneath. CRITICAL: attach this only to CONDITIONALLY-MOUNTED
/// views (modals, panels, the thread viewer). An always-mounted push pins the
/// context on the stack forever and permanently gates out the list keymap.
private struct KeyContextModifier: ViewModifier {
    let context: KeyContext
    @State private var token: UUID?

    func body(content: Content) -> some View {
        content
            .onAppear { if token == nil { token = KeyRegistry.shared.pushContext(context) } }
            .onDisappear {
                if let token { KeyRegistry.shared.popContext(token) }
                token = nil
            }
    }
}

extension View {
    /// Register bindings for the lifetime of this view. Handlers are refreshed
    /// on every render, so they always see current state.
    func keyBindings(_ context: KeyContext, _ bindings: [KeyBinding]) -> some View {
        modifier(KeyBindingsModifier(context: context, bindings: bindings))
    }

    /// Push a KeyContext while mounted. Pair with `.keyBindings(context, …)`.
    func keyContext(_ context: KeyContext) -> some View {
        modifier(KeyContextModifier(context: context))
    }
}
