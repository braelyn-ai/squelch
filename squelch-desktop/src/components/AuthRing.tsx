// AUTH COUNTDOWN RING — the 60s sweep drawn over the auth rail icon when a fresh
// auth message arrives (part of the 2FA "present, don't read" flow). One SVG
// ring per active AuthRing in the store; the stroke sweeps via a pure-CSS
// dash-offset animation (see .auth-ring circle in global.css), then the ring
// removes itself onAnimationEnd. Theme-aware: strokes use the --lock accent.
//
// Rendered inside the auth <button className="rail-btn"> (position: relative),
// absolutely centered over the KeyRound glyph. aria-hidden — it's ambient.

import { useStore, RING_MS } from "../state";

const SIZE = 34; // px — a hair larger than the 20px icon so it haloes it
const STROKE = 2;
const R = (SIZE - STROKE) / 2;
const CIRC = 2 * Math.PI * R;

export function AuthRings() {
  const rings = useStore((s) => s.authRings);
  const expireAuthRing = useStore((s) => s.expireAuthRing);
  if (rings.length === 0) return null;

  return (
    <>
      {rings.map((ring) => {
        // If a reload/late-mount lands mid-sweep, start the animation partway so
        // the ring still finishes ~60s after it was armed (never over-runs).
        const elapsed = Math.max(0, Date.now() - ring.startedAt);
        const remaining = Math.max(0, RING_MS - elapsed);
        if (remaining === 0) {
          // Already done (e.g. armed long ago). Drop it on next tick.
          queueMicrotask(() => expireAuthRing(ring.id));
          return null;
        }
        return (
          <svg
            key={ring.id}
            className="auth-ring"
            width={SIZE}
            height={SIZE}
            viewBox={`0 0 ${SIZE} ${SIZE}`}
            aria-hidden="true"
          >
            <circle
              cx={SIZE / 2}
              cy={SIZE / 2}
              r={R}
              fill="none"
              strokeWidth={STROKE}
              strokeLinecap="round"
              strokeDasharray={CIRC}
              style={{
                // Animate from full circumference (full ring) down to 0 offset
                // reversed: we sweep the *drawn* portion away over `remaining`.
                animationDuration: `${remaining}ms`,
                // @ts-expect-error — custom property for the keyframes below
                "--ring-circ": CIRC,
              }}
              onAnimationEnd={() => expireAuthRing(ring.id)}
            />
          </svg>
        );
      })}
    </>
  );
}
