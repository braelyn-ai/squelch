// The collapsed noise divider: "◌ N squelched · [a]ll mail · [T]rules". The
// visible squelch line at the bottom of the sitrep. Clicking "all mail" opens
// browse-all where the squelch knob lives.

export interface NoiseLineProps {
  noiseCount: number;
  onBrowse: () => void;
  onRules: () => void;
}

export function NoiseLine({ noiseCount, onBrowse, onRules }: NoiseLineProps) {
  return (
    <div className="noise-line num">
      <span>◌ {noiseCount} squelched</span>
      <span className="knob">
        <button onClick={onBrowse}>[a] all mail</button>
        <button onClick={onRules}>[T] rules</button>
      </span>
    </div>
  );
}
