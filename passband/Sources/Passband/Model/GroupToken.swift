// THE ONE PLACE a send group is encoded into a recipient string, and read back
// out of one.
//
// A fan-out group is a single pill, not twelve — so unlike a `to`/`bcc` group,
// which the picker expands into real addresses, there is nothing in the field
// but the group itself. That has to survive a draft round-trip, and the draft
// only stores `to` as text, so the group travels as `#<slug>`.
//
// `#` is chosen because it is UNAMBIGUOUS: no emittable address can start with
// one (`addr_is_emittable` requires exactly one `@` with a local part before
// it), so a token can never be mistaken for a recipient and a recipient can
// never be mistaken for a token. The daemon refuses any `#`-prefixed token that
// reaches the send route, which is what makes a token that failed to resolve
// loud instead of silently dropped.
//
// The token is a CLIENT ENCODING and nothing more. What actually goes on the
// wire is `group_id`; this exists so that closing the composer and reopening it
// does not lose which audience you were writing to.

import Foundation

enum GroupToken {
    static let prefix = "#"

    /// The pill text for a group. Slug rather than display name because the slug
    /// is what resolves: it is already lowercased and whitespace-collapsed by the
    /// daemon, so it round-trips through a draft unchanged.
    static func encode(_ group: SendGroup) -> String { prefix + group.slug }

    /// True when a recipient token names a group rather than a person.
    static func isToken(_ token: String) -> Bool {
        token.trimmed.hasPrefix(prefix)
    }

    /// The slug inside a token, or nil for an ordinary address.
    static func slug(_ token: String) -> String? {
        let trimmed = token.trimmed
        guard trimmed.hasPrefix(prefix) else { return nil }
        let slug = String(trimmed.dropFirst(prefix.count)).trimmed
        return slug.isEmpty ? nil : slug
    }

    /// The first group token in a comma-joined recipient string.
    ///
    /// FIRST, not all: a composition addresses one group. Two would be two
    /// audiences with two modes and no single answer to "how does this go out",
    /// and the picker never produces a second one.
    static func firstSlug(in value: String) -> String? {
        value.split(separator: ",").lazy.compactMap { slug(String($0)) }.first
    }

    /// Re-resolve a token to the group it names, for a draft restored after the
    /// composer that wrote it is long gone.
    ///
    /// Matches on the SLUG exactly, against the daemon's own search. A rename
    /// changes the slug, so a restored draft naming the old one resolves to
    /// nothing — which is correct and is why the composer renders an unresolved
    /// pill as a problem rather than quietly picking the nearest match. Guessing
    /// which audience someone meant is not a thing to do with an irreversible
    /// action.
    static func resolve(_ slug: String) async -> SendGroup? {
        guard let hits = try? await APIClient.shared.searchGroups(slug, limit: 25) else {
            return nil
        }
        return hits.first { $0.slug == slug }
    }
}
