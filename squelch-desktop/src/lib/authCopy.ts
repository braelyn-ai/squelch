// User-facing copy for auth-related mail (the /client/sealed metadata). "Sealed"
// is internal jargon and must never reach the UI — this maps the wire-level
// `sealed_kind` strings to auth-centric labels the user actually understands.

import {
  KeyRound,
  LockKeyhole,
  MailCheck,
  ShieldAlert,
  BadgeCheck,
  type LucideIcon,
} from "lucide-react";

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

/** Per-kind lucide icon (currentColor, so it inherits the surrounding tone). */
export function authKindIcon(kind: string | null | undefined): LucideIcon {
  switch (kind) {
    case "otp":
      return KeyRound; // login code
    case "password_reset":
      return LockKeyhole; // password reset
    case "magic_link":
      return MailCheck; // sign-in link
    case "login_alert":
      return ShieldAlert; // sign-in alert
    case "verification":
      return BadgeCheck; // verification
    default:
      return KeyRound;
  }
}
