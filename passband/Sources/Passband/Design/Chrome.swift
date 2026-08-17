// The chrome vocabulary: the rule, the chip, the key-hint bar, the section card
// and the segmented control every surface is assembled from. A shape that shows
// up on more than one screen belongs here, where it can only drift once.
//
// Everything in this file is a LEAF — a rule, a pill, a label pair. None of it
// wraps a stateful subtree, so swapping a hand-rolled copy for one of these can
// never change the view identity of the row or card around it.

import SwiftUI

// MARK: - the top bar

/// The strip a page's own header occupies, measured from the TRUE top of the
/// window.
///
/// macOS keeps roughly the first 32pt of a window clear for the traffic lights
/// and SwiftUI insets the whole app below it, which used to leave every page
/// wearing a band of empty backdrop above its own title. The header goes up
/// there instead — level with the buttons, the way a browser puts its tab strip
/// — and the RAIL is what yields: it starts at this line rather than at the
/// window's edge, so nothing but the dots is ever in the strip.
///
/// ONE height for every page, because the rail's top edge is cut to it: a page
/// whose header ran taller or shorter would leave the rail's edge hanging in
/// mid-air beside that page's rule instead of continuing it.
enum TopBar {
    /// Deep enough for the wordmark line the sitrep sets in 19pt serif, and
    /// close enough to the buttons' own centre (y 16) that a title beside them
    /// reads as being ON their line rather than under it.
    static let height: CGFloat = 40
}

// MARK: - rules

/// The 0.5pt rule dividing a header from what it heads. Ink-side (see
/// `Palette.hairline`) so it reads on a glass pane's own tint.
struct Hairline: View {
    var body: some View {
        Rectangle().fill(Palette.hairline).frame(height: 0.5)
    }
}

// MARK: - chips

/// Chip metrics. The hand-rolled copies these replaced drifted across 6/7/8/9
/// horizontal and 2.5/3/4 vertical; 7/3 was the most common of each.
private enum ChipMetrics {
    static let horizontal: CGFloat = 7
    static let vertical: CGFloat = 3
}

/// A glass chip action — the app's one small-button shape. Padding is fixed
/// here on purpose: a chip that sets its own padding is a chip that drifts.
///
/// `tone` is the chip's ink; a label that colours its own parts (the newsletter
/// rule chip) simply overrides it further down.
struct ChromeChip<Content: View>: View {
    private let tone: Color
    private let help: String?
    private let action: () -> Void
    private let label: Content

    init(
        tone: Color = Palette.inkFaint, help: String? = nil,
        action: @escaping () -> Void, @ViewBuilder label: () -> Content
    ) {
        self.tone = tone
        self.help = help
        self.action = action
        self.label = label()
    }

    var body: some View {
        Button(action: action) {
            label
                .padding(.horizontal, ChipMetrics.horizontal)
                .padding(.vertical, ChipMetrics.vertical)
        }
        .buttonStyle(.glass)
        .foregroundStyle(tone)
        .help(ifPresent: help)
    }
}

/// A chip's standard label: an SF Symbol, a short string, or both.
struct ChipLabel: View {
    var text: String?
    var icon: String?
    var font: Font = Typo.micro

    var body: some View {
        Group {
            if let text, let icon {
                Label(text, systemImage: icon)
            } else if let icon {
                Image(systemName: icon)
            } else if let text {
                Text(text)
            }
        }
        .font(font)
    }
}

extension ChromeChip where Content == ChipLabel {
    /// The common chip — icon, text, or both.
    init(
        text: String? = nil, icon: String? = nil, font: Font = Typo.micro,
        tone: Color = Palette.inkFaint, help: String? = nil,
        action: @escaping () -> Void
    ) {
        self.init(tone: tone, help: help, action: action) {
            ChipLabel(text: text, icon: icon, font: font)
        }
    }
}

// MARK: - key hints

/// One key hint: the key chip(s) and the verb they fire.
struct KeyHint: View {
    let keys: [String]
    let label: String

    init(_ key: String, _ label: String) {
        self.keys = [key]
        self.label = label
    }

    init(_ keys: [String], _ label: String) {
        self.keys = keys
        self.label = label
    }

    var body: some View {
        HStack(spacing: 4) {
            ForEach(keys, id: \.self) { Kbd($0) }
            Text(label).font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        }
    }
}

/// The bottom bar a list surface teaches its keymap with: dot-separated key
/// hints under a top hairline.
struct KeyHintBar: View {
    let hints: [KeyHint]

    var body: some View {
        HStack(spacing: 4) {
            ForEach(Array(hints.enumerated()), id: \.offset) { i, hint in
                if i > 0 { Text("·").foregroundStyle(Palette.inkFaintest) }
                hint
            }
            Spacer()
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 10)
        .overlay(alignment: .top) { Hairline() }
    }
}

// MARK: - status

/// A state dot with its label — "connected · saved", "3 live · 1 awaiting you".
struct StatusDot: View {
    let color: Color
    let label: String
    /// Label ink; defaults to the dot's own colour.
    var tone: Color?
    var size: CGFloat = 6

    var body: some View {
        HStack(spacing: 5) {
            Circle().fill(color).frame(width: size, height: size)
            Text(label)
                .font(Typo.micro)
                .foregroundStyle(tone ?? color)
        }
    }
}

// MARK: - section card

/// One engraved section on a single glass pane. `note` is the right-aligned
/// annotation beside the label (a model name, a provider); with none, the
/// heading is exactly the bare label it was before the slot existed.
struct SectionCard<Content: View>: View {
    let label: String
    var note: String?
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(label)
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.accent)
                    .textCase(.uppercase)
                if let note {
                    Spacer()
                    Text(note)
                        .font(Typo.mono(10))
                        .foregroundStyle(Palette.inkFaintest)
                }
            }
            content
        }
        .zonePadding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
    }
}

// MARK: - segmented control

/// The control's coordinate space. FILE-SCOPE for the same reason as the
/// rail's: the buttons read it inside `onGeometryChange`'s Sendable closure.
/// One name serves every instance — a lookup resolves the nearest ancestor.
private let segmentedSpace = "glass-segmented"

/// A sliding segmented control, mechanics borrowed from the sidebar rail: the
/// options are bare text, and ONE long-lived pane behind them slides between
/// slots the buttons report as they lay out — moved by geometry, not identity,
/// which is what lets the travel animate.
struct GlassSegmented<T: Hashable>: View {
    let options: [(value: T, label: String)]
    @Binding var selection: T

    /// Where each option sits in the control's own space.
    @State private var slots: [T: CGRect] = [:]

    /// Same crossing time as the rail, so the two selectors read as one motion
    /// vocabulary.
    private static var travel: Animation { .smooth(duration: 0.32) }

    var body: some View {
        HStack(spacing: 2) {
            ForEach(options, id: \.value) { option in
                Button {
                    selection = option.value
                } label: {
                    Text(option.label)
                        .font(Typo.chip)
                        .foregroundStyle(selection == option.value ? .white : Palette.ink)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 5)
                        .contentShape(Capsule())
                        .onGeometryChange(for: CGRect.self) {
                            $0.frame(in: .named(segmentedSpace))
                        } action: { slots[option.value] = $0 }
                }
                .buttonStyle(.plain)
            }
        }
        .coordinateSpace(.named(segmentedSpace))
        .background(alignment: .topLeading) { selector }
        .animation(Self.travel, value: selection)
        .background(Capsule().fill(Palette.hairline.opacity(0.5)))
    }

    /// The travelling pane: absent only until the first layout pass reports a
    /// slot, then one view for the life of the control.
    @ViewBuilder
    private var selector: some View {
        if let slot = slots[selection] {
            Color.clear
                .frame(width: slot.width, height: slot.height)
                .glassEffect(.regular.tint(Palette.accent.opacity(0.55)), in: Capsule())
                // Sits over the active button — an interactive material would
                // eat that option's clicks.
                .allowsHitTesting(false)
                .offset(x: slot.minX, y: slot.minY)
        }
    }
}

// MARK: - helpers

extension View {
    /// `.help` only when there is something to say — an empty tooltip string
    /// still registers a tooltip region.
    @ViewBuilder fileprivate func help(ifPresent text: String?) -> some View {
        if let text { help(text) } else { self }
    }
}
