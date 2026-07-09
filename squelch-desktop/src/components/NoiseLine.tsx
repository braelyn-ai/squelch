// The collapsed noise divider: "◌ N filtered out · [a]ll mail · [T]rules". The
// visible noise line at the bottom of the sitrep. Clicking "all mail" opens
// browse-all where the noise-filter knob lives. When auth mail is waiting, a
// compact pill sits here too (a login code arriving is worth a glance) and opens
// the Auth tab.

export interface NoiseLineProps {
  noiseCount: number;
  authCount: number;
  onBrowse: () => void;
  onRules: () => void;
  onOpenAuth: () => void;
}

export function NoiseLine({
  noiseCount,
  authCount,
  onBrowse,
  onRules,
  onOpenAuth,
}: NoiseLineProps) {
  return (
    <div className="noise-line num">
      <span>{noiseCount} filtered out below the line</span>
      {authCount > 0 && (
        <button
          type="button"
          className="auth-pill"
          onClick={onOpenAuth}
          title="login codes, password resets & sign-in alerts"
        >
          🔑 {authCount} auth {authCount === 1 ? "message" : "messages"}{" "}
          <kbd>g</kbd>
        </button>
      )}
      <span className="knob">
        <button onClick={onBrowse}>
          All mail <kbd>a</kbd>
        </button>
        <button onClick={onRules}>
          Rules <kbd>T</kbd>
        </button>
      </span>
    </div>
  );
}
