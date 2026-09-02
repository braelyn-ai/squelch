// The word for a piece of auth mail, and NOTHING else.
//
// Split out of AuthCode.swift — which imports SwiftUI for the decisions ledger
// — because the iOS notification service extension has to say the same word.
// That process is woken for every push and killed in seconds, so its source
// list is hand-picked (see project.yml) and everything on it must be pure
// Foundation: no SwiftUI, no app singletons, nothing that draws. A banner whose
// wording depended on which of the two posted it would be a visible seam, and
// the seam would appear exactly on the mail people most want to trust.
//
// The rest of AuthCopy (the SF Symbol per kind) stays beside the extractor: the
// extension has no icons to draw.

import Foundation

/// User-facing copy for auth mail. "Sealed" is internal jargon and must never
/// reach the UI, so wire `sealed_kind` values map to auth-centric labels.
enum AuthCopy {
    static func label(_ kind: SealedKind?) -> String {
        switch kind {
        case .otp: "Login code"
        case .passwordReset: "Password reset"
        case .magicLink: "Sign-in link"
        case .loginAlert: "Sign-in alert"
        case .verification: "Verification"
        // A kind we don't know stays generic — its raw string is never shown.
        case .unknown, nil: "Auth message"
        }
    }
}
