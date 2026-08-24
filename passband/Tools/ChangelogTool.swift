// Renders the release-note table as markdown. Compiled against
// Sources/Passband/Lib/ReleaseNotes.swift, which is the only copy of the
// changelog anybody writes; every other form of it comes out of here.
//
// Two modes, because two audiences read the same notes:
//
//   (no arguments)   the whole history, for docs/CHANGELOG.md and the site
//   <version>        one release, for the body of a GitHub release
//
// It prints to stdout and writes nothing. make-changelog.sh does the placing,
// so a caller who wants the text somewhere else does not have to fight a tool
// that has already decided.

import Foundation

@main
enum ChangelogTool {
    static func main() {
        if let wanted = CommandLine.arguments.dropFirst().first {
            one(wanted)
        } else {
            everything()
        }
    }

    /// ONE RELEASE, exact. A release body is attached to a tag, and answering a
    /// tag with the notes for some neighbouring version is worse than answering
    /// with nothing: nobody re-reads a changelog they were handed once.
    static func one(_ version: String) {
        guard let note = ReleaseNotes.all.first(where: { $0.version == version }) else {
            FileHandle.standardError.write(
                Data("no release note for \(version) in ReleaseNotes.swift\n".utf8))
            exit(1)
        }
        print(body(note))
    }

    /// THE WHOLE HISTORY. The generated-file banner is not decoration: this is
    /// the file somebody lands on from a search result, and an edit made there
    /// vanishes on the next release.
    static func everything() {
        print(
            """
            # Changelog

            <!-- GENERATED FILE. Every note here is written in
                 passband/Sources/Passband/Lib/ReleaseNotes.swift, which is what
                 the app's own What's New card reads. Edit that table and re-run
                 passband/make-changelog.sh; an edit made here is lost on the
                 next release. -->

            What each version of Passband brought, in the app and in the daemon
            behind it. The two ship separately: the app updates itself, and the
            daemon is rolled onto hosted accounts or pulled as an image on a
            self-host box, so every note says which one it landed in.

            """)
        for note in ReleaseNotes.all {
            print("## \(note.version) (\(note.date))")
            print("")
            print(body(note))
        }
    }

    /// One release's bullets, grouped under the surface that shipped them.
    /// Shared by both modes so a release body and the changelog can never
    /// describe the same version differently.
    static func body(_ note: ReleaseNote) -> String {
        var out = [note.headline, ""]
        for surface in ReleaseSurface.allCases {
            let items = note.items(on: surface)
            guard !items.isEmpty else { continue }
            out.append("### \(surface.label)")
            out.append("")
            out.append(contentsOf: items.map { "- \($0.text)" })
            out.append("")
        }
        return out.joined(separator: "\n")
    }
}
