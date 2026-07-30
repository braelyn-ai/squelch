// Two voices: SF for the interface, and Newsreader — the bundled editorial
// serif — for the brand, in exactly one place, the sitrep hero headline.
// Spread further, a display serif becomes wallpaper.
//
// Numbers are always monospaced-digit so columns of amounts, importances and
// token counts line up rather than jittering as they update.

import AppKit
import SwiftUI

enum Typo {
    /// The bundled variable serif. Falls back to New York (the system serif) if
    /// the resource is missing, which keeps the editorial voice either way.
    static let serifFamily = "Newsreader"

    /// Register the bundled fonts. `ATSApplicationFontsPath` in Info.plist
    /// covers the normal case; this is the fallback when the resources landed
    /// somewhere unexpected in the bundle.
    static func registerBundledFonts() {
        guard NSFont(name: serifFamily, size: 12) == nil else { return }
        for name in ["newsreader-var", "newsreader-italic-var"] {
            guard let url = Bundle.main.url(forResource: name, withExtension: "ttf") else {
                continue
            }
            CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
        }
    }

    /// The editorial headline. Newsreader when available, New York otherwise.
    static func hero(_ size: CGFloat = 40) -> Font {
        if NSFont(name: serifFamily, size: size) != nil {
            return .custom(serifFamily, size: size).weight(.medium)
        }
        return .system(size: size, weight: .medium, design: .serif)
    }

    /// A smaller serif moment (empty states, the connect card's wordmark).
    static func serif(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        if NSFont(name: serifFamily, size: size) != nil {
            return .custom(serifFamily, size: size).weight(weight)
        }
        return .system(size: size, weight: weight, design: .serif)
    }

    // MARK: - interface scale

    /// Engraved uppercase section label ("CONNECTION", "For your eyes").
    static let sectionLabel = Font.system(size: 11, weight: .semibold).width(.expanded)
    /// Zone heading.
    static let zoneTitle = Font.system(size: 13, weight: .semibold)
    /// Dense row text.
    static let row = Font.system(size: 13)
    /// A row's secondary line.
    static let rowSub = Font.system(size: 12)
    /// Chips, tags, counts.
    static let chip = Font.system(size: 11, weight: .medium)
    /// The smallest legible metadata.
    static let micro = Font.system(size: 10, weight: .medium)

    /// Numbers — monospaced digits so columns don't jitter.
    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .monospaced)
    }
    static func num(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight).monospacedDigit()
    }
}

// MARK: - kbd chip

/// The key-hint chip the whole app uses to teach its keymap in place.
struct Kbd: View {
    let label: String
    init(_ label: String) { self.label = label }

    var body: some View {
        Text(label)
            .font(Typo.mono(10, weight: .medium))
            .foregroundStyle(Palette.inkFaint)
            .padding(.horizontal, 5)
            .padding(.vertical, 1.5)
            .background(
                RoundedRectangle(cornerRadius: 4, style: .continuous)
                    .fill(Palette.hairline.opacity(0.6))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 4, style: .continuous)
                    .strokeBorder(Palette.hairline, lineWidth: 0.5)
            )
            .fixedSize()
    }
}
