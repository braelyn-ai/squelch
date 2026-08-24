// The what's-new card: one modal, once per version, listing what the release
// brought and which half of Passband brought it.
//
// A MODAL rather than the update card's bottom-centre float, and the difference
// is what each one wants. UpdateAlert waits beside your work for an answer you
// may not want to give yet; this is a thing to READ, arriving exactly once,
// and a card competing with a live inbox for attention gets neither read nor
// dismissed. Same scrim and glass as the shortcuts sheet, so it reads as part
// of the app rather than as an announcement bolted to the front of it.
//
// It carries a version stamp per release, not a single "what's new" heading:
// somebody who skipped two updates is owed the boundaries between them, since
// that is the only way to tell which release broke or fixed the thing they came
// looking for.

import SwiftUI

struct WhatsNewCard: View {
    @Environment(AppStore.self) private var store

    private var notes: [ReleaseNote] { store.whatsNew.notes }

    var body: some View {
        OverlayScrim(onDismiss: dismiss) {
            ModalCard(width: 560) {
                header

                // A ceiling, not a height: one release is a short card, and
                // three is a scroll rather than a sheet taller than the window.
                ScrollView {
                    VStack(alignment: .leading, spacing: 20) {
                        ForEach(notes) { note in
                            release(note)
                        }
                    }
                    .padding(.trailing, 2)
                }
                .frame(maxHeight: 380)
                // A single short release must not be padded out to the ceiling.
                .fixedSize(horizontal: false, vertical: true)

                footer
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close what's new") { dismiss() },
            KeyBinding("Enter", "close what's new") { dismiss() },
        ])
    }

    private func dismiss() { store.whatsNew.dismiss() }

    // MARK: - chrome

    private var header: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("What's new")
                .font(Typo.serif(22, weight: .medium))
                .foregroundStyle(Palette.ink)
            // Plural only when it is: somebody who updates every week should
            // not be told they missed a stack of releases.
            Text(
                notes.count == 1
                    ? "In this version of Passband."
                    : "In the \(notes.count) versions since you last looked."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkFaint)
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            // WHERE THE REST LIVES. The card carries only what is unread; the
            // full history is a document, and Settings is where somebody goes
            // looking for it a second time.
            Text("Settings has this again under General.")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            Spacer(minLength: 10)
            Button("Got it") { dismiss() }
                .buttonStyle(.glassProminent)
                .font(.system(size: 12, weight: .semibold))
                .keyboardShortcut(.defaultAction)
        }
    }

    // MARK: - one release

    private func release(_ note: ReleaseNote) -> some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(note.version)
                    .font(Typo.num(12, weight: .semibold))
                    .foregroundStyle(Palette.accent)
                Text(note.date)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .monospacedDigit()
            }

            Text(note.headline)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            // Grouped by surface rather than interleaved, because the two
            // answer different questions: what changed on this machine, and
            // what changed in the thing still running when it is asleep. A
            // surface with nothing in it draws no heading.
            ForEach(ReleaseSurface.allCases, id: \.self) { surface in
                let items = note.items(on: surface)
                if !items.isEmpty {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(surface.label)
                            .font(Typo.sectionLabel)
                            .foregroundStyle(Palette.inkFaint)
                            .textCase(.uppercase)
                        ForEach(items.indices, id: \.self) { index in
                            bullet(items[index].text)
                        }
                    }
                    .padding(.top, 2)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func bullet(_ text: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            // Aligned to the first LINE of the text, not its centre, so a
            // three-line item still reads as one bullet.
            Circle()
                .fill(Palette.inkFaintest)
                .frame(width: 3, height: 3)
                .padding(.top, 6)
            Text(text)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
