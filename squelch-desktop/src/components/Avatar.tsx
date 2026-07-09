// A small circular sender avatar: initials over a deterministic, theme-aware
// background. LOCAL-ONLY — no network avatar service is ever contacted (privacy:
// remote avatars/favicons leak the correspondent graph). Color + initials are
// derived purely from the sender string. Known contacts may get a subtle ring.

import { initialsFor, avatarSlot } from "../lib/avatar";

export interface AvatarProps {
  sender: string;
  /** Draw a subtle accent ring (e.g. a known contact). */
  known?: boolean;
  /** px diameter; rows use ~22. */
  size?: number;
}

export function Avatar({ sender, known = false, size = 22 }: AvatarProps) {
  const slot = avatarSlot(sender);
  const initials = initialsFor(sender);
  return (
    <span
      className={`avatar avatar-${slot}${known ? " known" : ""}`}
      style={{ width: size, height: size, fontSize: Math.round(size * 0.42) }}
      aria-hidden="true"
      title={sender}
    >
      {initials}
    </span>
  );
}
