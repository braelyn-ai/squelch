// User-facing copy for auth-related mail (the /client/sealed metadata). "Sealed"
// is internal jargon and must never reach the UI — this maps the wire-level
// `sealed_kind` strings to auth-centric labels the user actually understands.

/** Friendly label for a sealed_kind value returned by /client/sealed. */
export function authKindLabel(kind: string | null | undefined): string {
  switch (kind) {
    case "otp":
      return "Login code";
    case "password_reset":
      return "Password reset";
    case "magic_link":
      return "Sign-in link";
    case "login_alert":
      return "Sign-in alert";
    case "verification":
      return "Verification";
    default:
      return "Auth message";
  }
}
