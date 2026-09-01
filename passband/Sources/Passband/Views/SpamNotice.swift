// The one sentence the spam page owes the reader: WHO DECIDED THIS.
//
// Every other list in Passband is the product's own opinion, and the whole
// premise is that those opinions are inspectable — a row carries a one-liner, a
// tier, a "why", and a `v` to argue with it. Not one of those exists here.
// Nothing in Passband read this mail; Gmail filtered it before the daemon ever
// saw it, and the daemon fetched the folder without triaging a word of it.
//
// So the page has to say so, plainly, or it reads as a verdict Passband reached
// and cannot explain — which is worse than not showing the folder at all. The
// noise page sits one segment away and looks nearly identical, and the words
// "spam" and "noise" mean roughly the same thing in English; the only thing
// keeping the two apart in the reader's head is this line.
//
// INFORMATIONAL, NOT A WARNING. No amber, no exclamation triangle: the folder
// working correctly is the normal case, and dressing it as a problem would push
// people to "fix" mail that is exactly where it belongs. Compare
// `GmailDisconnectedBanner`, which is amber because something is actually
// broken.
//
// NOT DISMISSIBLE, and cheap enough to leave up: it is a caption on a page
// nobody visits twice in a row, and its whole value is being there the moment
// somebody finds a real message in the list and wonders who put it there.

import SwiftUI

struct SpamNotice: View {
    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            Image(systemName: "tray.and.arrow.down")
                .foregroundStyle(Palette.inkFaint)
                .font(.system(size: 12, weight: .medium))
                .padding(.top, 1)

            // TWO SENTENCES, and the second one is the one that matters. The
            // first is provenance; the second is what to do about it, because
            // the only reason to read this page is that something real might be
            // in it, and finding it is useless without a way out.
            Text(
                "Your email provider filtered this mail before Passband saw it. "
                    + "Nothing here was triaged, and none of it sent you a notification. "
                    + "If something belongs in your inbox, mark it not spam and it moves back."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkDim)
            .fixedSize(horizontal: false, vertical: true)

            Spacer(minLength: 0)
        }
        .padding(.horizontal, 13)
        .padding(.vertical, 9)
        // Outline, no fill. Every filled card in this app is filled in a
        // SEMANTIC colour (warnSoft, dangerSoft, lockSoft) and means something
        // is wrong or sealed; a neutral fill would be the only one of its kind,
        // and a semantic one would be a lie. A hairline says "this is a caption
        // about the page" and stops there.
        .overlay(
            RoundedRectangle(cornerRadius: 9)
                .strokeBorder(Palette.hairlineStrong, lineWidth: 1)
        )
        .accessibilityElement(children: .combine)
    }
}
