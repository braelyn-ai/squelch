// The passband palette. Every color is defined for BOTH appearances: on glass,
// ink has to be darker in light mode and lighter in dark than it would be on an
// opaque fill, because the material lets the backdrop's luminance through.
// The accent marks state, never decoration. Tier colors are fixed and never
// reused for chrome: coral overdue, amber deadline, green signal, lock auth.

import SwiftUI

enum Palette {
    // MARK: - the accent

    /// The passband blue. Saturated on purpose.
    static let accent = Color(
        light: Color(hex: 0x2B7FD4), dark: Color(hex: 0x4E9BEA))
    /// Deeper blue for amounts, links and pressed states.
    static let accentInk = Color(
        light: Color(hex: 0x1C63AE), dark: Color(hex: 0x82BAF5))
    /// The wash behind a selected row / CTA fill.
    static let accentSoft = Color(
        light: Color(hex: 0x2B7FD4).opacity(0.14),
        dark: Color(hex: 0x4E9BEA).opacity(0.20))

    /// The glass tint the app's own chrome uses. Alpha stays low: a tint this
    /// saturated stops being glass and becomes a blue box.
    static let glassTint = Color(hex: 0x2B7FD4).opacity(0.10)
    /// A stronger tint for surfaces that must read as "passband's own".
    static let glassTintStrong = Color(hex: 0x2B7FD4).opacity(0.18)

    // MARK: - tier semantics

    /// past_due / overdue.
    static let danger = Color(light: Color(hex: 0xD6412F), dark: Color(hex: 0xFF7A68))
    static let dangerSoft = Color(hex: 0xD6412F).opacity(0.16)
    /// deadline.
    static let warn = Color(light: Color(hex: 0xB5761A), dark: Color(hex: 0xE8AC4C))
    static let warnSoft = Color(hex: 0xB5761A).opacity(0.16)
    /// signal / positive / synced.
    static let positive = Color(light: Color(hex: 0x1F7A4D), dark: Color(hex: 0x4FC08A))
    static let positiveSoft = Color(hex: 0x1F7A4D).opacity(0.16)
    /// sealed / auth.
    static let lock = Color(light: Color(hex: 0x5D4BD6), dark: Color(hex: 0x9B8CF5))
    static let lockSoft = Color(hex: 0x5D4BD6).opacity(0.16)

    static func tierColor(_ tier: Tier) -> Color {
        switch tier {
        case .pastDue: danger
        case .deadline: warn
        case .signal: positive
        case .noise: inkFaintest
        }
    }

    /// Importance -> color bucket. High importance leans hot.
    static func importanceColor(_ n: Int) -> Color {
        if n >= 85 { return danger }
        if n >= 70 { return warn }
        if n >= 40 { return ink }
        return inkDim
    }

    // MARK: - ink ladder

    static let ink = Color(light: Color(hex: 0x141C26), dark: Color(hex: 0xE9EEF5))
    static let inkDim = Color(light: Color(hex: 0x455465), dark: Color(hex: 0xA8B4C4))
    static let inkFaint = Color(light: Color(hex: 0x6B7A8B), dark: Color(hex: 0x808E9F))
    static let inkFaintest = Color(light: Color(hex: 0x94A2B2), dark: Color(hex: 0x62707F))

    // MARK: - lines

    /// A hairline INSIDE a glass pane — ink-side, so it reads on the pane's own
    /// tint instead of dissolving into the specular edge.
    static let hairline = Color(
        light: Color(hex: 0x1E3246).opacity(0.12), dark: Color(hex: 0xFFFFFF).opacity(0.10))
    static let hairlineStrong = Color(
        light: Color(hex: 0x1E3246).opacity(0.20), dark: Color(hex: 0xFFFFFF).opacity(0.18))

    // MARK: - reading surface

    /// Long-form mail is a reading PAGE, not a glass panel: body copy must never
    /// sit on a busy wallpaper, so message cards stay solid.
    static let readerBackground = Color(
        light: Color(hex: 0xF7FAFD), dark: Color(hex: 0x131A24))

    /// The stand-in canvas when glass has nothing to sample.
    static let canvas = Color(light: Color(hex: 0xEDF3FA), dark: Color(hex: 0x0E141D))

    // MARK: - avatar palette

    /// Ten theme-aware pairs, picked deterministically by sender address.
    static let avatarPalette: [(bg: Color, fg: Color)] = [
        (Color(light: 0xD8E6F8, dark: 0x24384F), Color(light: 0x1C4E82, dark: 0xA9CBF0)),
        (Color(light: 0xDCE9E1, dark: 0x22392E), Color(light: 0x246148, dark: 0x9AD3B6)),
        (Color(light: 0xF3E2DC, dark: 0x442C26), Color(light: 0x8E4231, dark: 0xEBA894)),
        (Color(light: 0xE6E1F5, dark: 0x2F2A4A), Color(light: 0x4C3E9C, dark: 0xB8ACF2)),
        (Color(light: 0xF5EBD5, dark: 0x3E2E1B), Color(light: 0x7E5A16, dark: 0xE2C078)),
        (Color(light: 0xDCE7EE, dark: 0x25333D), Color(light: 0x2C5568, dark: 0x9EC4D8)),
        (Color(light: 0xEFDEE9, dark: 0x3E2839), Color(light: 0x83366A, dark: 0xE0A6CD)),
        (Color(light: 0xE0EAD8, dark: 0x2A3726), Color(light: 0x466A2E, dark: 0xB4D69A)),
        (Color(light: 0xE9E3DA, dark: 0x37312A), Color(light: 0x6A5540, dark: 0xD3C0A6)),
        (Color(light: 0xDDE0F0, dark: 0x282C45), Color(light: 0x3B4490, dark: 0xAAB2EE)),
    ]

    static func avatarColors(for sender: String) -> (bg: Color, fg: Color) {
        avatarPalette[SenderID.slot(sender) % avatarPalette.count]
    }
}

// MARK: - color helpers

extension Color {
    /// Build a color from a 24-bit RGB literal.
    init(hex: UInt32) {
        self.init(
            .sRGB,
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: 1)
    }

    /// A dynamic color that resolves per appearance.
    init(light: Color, dark: Color) {
        self = Platform.dynamicColor(light: light, dark: dark)
    }

    init(light: UInt32, dark: UInt32) {
        self.init(light: Color(hex: light), dark: Color(hex: dark))
    }
}
