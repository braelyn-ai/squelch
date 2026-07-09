// A small circular sender avatar.
//
// HUMAN senders: initials over a deterministic, theme-aware background, derived
// purely from the sender string — LOCAL-ONLY, no network fetch ever (privacy:
// remote avatars leak the human correspondent graph).
//
// ROBOT senders (no-reply@, notifications@, billing@, …): the domain's favicon
// from DuckDuckGo's icon service, fetched at most once per domain (verdict cached
// in localStorage + memory). On error / blank / tiny response we fall back to the
// initials avatar seamlessly. Robot mailboxes name a service, not a person, so
// this leaks nothing about who a human talks to.

import { useState } from "react";
import {
  initialsFor,
  avatarSlot,
  isRobotSender,
  isBrandSender,
  faviconDomain,
  faviconUrl,
  faviconVerdict,
  setFaviconVerdict,
} from "../lib/avatar";

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

  // Only robot / brand senders are ever eligible for a favicon. Humans
  // short-circuit to initials with no network work of any kind.
  const domain =
    isRobotSender(sender) || isBrandSender(sender)
      ? faviconDomain(sender)
      : null;

  // Track whether the favicon failed *this* mount (start from cached verdict so a
  // previously-failed domain never re-fetches).
  const [failed, setFailed] = useState(
    () => !domain || faviconVerdict(domain) === "failed",
  );

  const initialsAvatar = (
    <span
      className={`avatar avatar-${slot}${known ? " known" : ""}`}
      style={{ width: size, height: size, fontSize: Math.round(size * 0.42) }}
      aria-hidden="true"
      title={sender}
    >
      {initials}
    </span>
  );

  if (!domain || failed) return initialsAvatar;

  return (
    <img
      className={`avatar avatar-favicon${known ? " known" : ""}`}
      src={faviconUrl(domain)}
      width={size}
      height={size}
      style={{ width: size, height: size }}
      alt=""
      aria-hidden="true"
      title={sender}
      referrerPolicy="no-referrer"
      onLoad={(e) => {
        // Blank/tiny responses (DDG's fallback) aren't real logos — treat as fail.
        const img = e.currentTarget;
        if (img.naturalWidth <= 1 || img.naturalHeight <= 1) {
          setFaviconVerdict(domain, "failed");
          setFailed(true);
        } else {
          setFaviconVerdict(domain, "ok");
        }
      }}
      onError={() => {
        setFaviconVerdict(domain, "failed");
        setFailed(true);
      }}
    />
  );
}
