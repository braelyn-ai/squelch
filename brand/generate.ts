// Source of truth for the Passband mark. Everything downstream — the app icon,
// the site favicon, the hero — is generated from here, so the geometry exists
// in exactly one place and a tweak reaches all of it.
//
// The mark is the hovered download button on the site, frozen: a spectrum
// bounded by its own bandpass curve. Bars are the signal, the curve is the
// filter, and the flat runs off both edges are the squelch line continuing
// past the frame.
//
// Run via ./build.sh, which also rasterizes and installs.

// ---- geometry ---------------------------------------------------------
const S = 1024; // Icon Composer's canvas
// The squelch line's height. A squircle with a ~22.4% corner radius starts
// curving in around y=795, so any lower and the line stops short of the edges
// instead of running off them.
const BASE = 772;
// Hump height. Capped by balance rather than taste: the band under the line is
// fixed at 252px, and a taller hump leaves the mark visibly floating.
const MAXH = 545;
const BARS = 13;
const TAKE = 5.1; // the frozen moment of the noise
const BAR_RATIO = 0.74; // bar width as a fraction of its slot
const BANDW = 0.3; // envelope half-width, in canvas fractions
const EXPO = 3; // envelope exponent; 3 is "softened" — rounded, not flat-topped
const STROKE = 12;
const OVER = 12; // how far the line is drawn past each edge

// Deterministic stand-in for the button's per-bar drift: golden-ratio stepping
// reads as random but renders identically every run.
const ph = (i: number) => (i * 2.399963) % (Math.PI * 2);
const sp = (i: number) => 0.7 + ((i * 0.6180339887) % 1) * 1.9;
const noise = (i: number, t: number) =>
  0.5 + 0.5 * Math.sin(t * sp(i) + ph(i)) * Math.cos(t * sp(i) * 0.61 + ph(i) * 1.7);

const env = (x: number) => Math.exp(-Math.pow(Math.abs(x - 0.5) / BANDW, EXPO));
// Bar spread solved so the outermost bar always lands at 20% of the envelope,
// rather than being hand-tuned and silently culled when other knobs move.
const SPREAD = (2 * (BANDW * Math.pow(-Math.log(0.2), 1 / EXPO))) / (1 - 1 / BARS);

type Bar = { cx: number; w: number; h: number; e: number; i: number };
const BARSET: Bar[] = (() => {
  const barSpan = S * SPREAD;
  const step = barSpan / BARS;
  const w = step * BAR_RATIO;
  const fill01 = Array.from({ length: BARS }, (_, i) => 0.62 + 0.38 * noise(i, TAKE));
  const peak = Math.max(...fill01);
  const out: Bar[] = [];
  for (let i = 0; i < BARS; i++) {
    const cx = (S - barSpan) / 2 + (i + 0.5) * step;
    // A bar has width, so sampling the envelope at its centre lets the outer
    // half rise above a curve that has already dropped. The minimum across the
    // bar is the only ceiling that holds on a slope.
    const e = Math.min(env((cx - w / 2) / S), env((cx + w / 2) / S));
    const h = e * (fill01[i] / peak) * MAXH;
    if (h < w) continue;
    // Tint still keys off the centre: where in the band a bar sits is a
    // different question from how tall it is allowed to be.
    out.push({ cx, w, h, e: env(cx / S), i });
  }
  return out;
})();

const CURVE = (() => {
  const pts: string[] = [];
  for (let px = -OVER; px <= S + OVER; px += 3)
    pts.push(`${pts.length === 0 ? "M" : "L"}${px.toFixed(1)} ${(BASE - env(px / S) * MAXH).toFixed(1)}`);
  return pts.join(" ");
})();

// ---- colour -----------------------------------------------------------
const hexv = (c: string) => [
  parseInt(c.slice(1, 3), 16),
  parseInt(c.slice(3, 5), 16),
  parseInt(c.slice(5, 7), 16),
];
const mix = (a: string, b: string, t: number) => {
  const [r1, g1, b1] = hexv(a);
  const [r2, g2, b2] = hexv(b);
  const v = (x: number, y: number) => Math.round(x + (y - x) * t);
  return `#${[v(r1, r2), v(g1, g2), v(b1, b2)].map((n) => n.toString(16).padStart(2, "0")).join("")}`;
};

type Ramp = readonly [string, string, string];
/** Ice as ink: for light grounds. Saturated and taken deep, never just darkened. */
const INK = {
  ctr: ["#54a8ce", "#1f7099", "#0c4260"] as Ramp,
  edge: ["#83a8ba", "#5b8298", "#33505f"] as Ramp,
  line: ["#82b2c8", "#2f7ca0", "#0b3b57"] as Ramp,
};
/** Ice as light: for dark grounds. */
const LIT = {
  ctr: ["#dff4ff", "#7fcdf0", "#2f7fa8"] as Ramp,
  edge: ["#6ea3bd", "#4a7f9c", "#234b60"] as Ramp,
  line: ["#2b5f78", "#6fb4d4", "#e6f8ff"] as Ramp,
};

/** Mist with dawn in it: cool overhead, turning at the line and cooling again
 *  below. The warm stop sits at .754 — exactly where the squelch line is — so
 *  the temperature change reads as light on a horizon rather than as a
 *  gradient someone added. It is barely a colour: a rose-grey shift, not
 *  peach, which is what keeps it in Mist's character. */
const DAWN = `<linearGradient id="ground" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#c9d3db"/>
      <stop offset=".4" stop-color="#dbe1e4"/>
      <stop offset=".66" stop-color="#e3dcdc"/>
      <stop offset=".754" stop-color="#ece0dc"/>
      <stop offset=".87" stop-color="#d5d5d6"/>
      <stop offset="1" stop-color="#b8c0c6"/>
    </linearGradient>`;
/** Plain mist, no event in it. What dawn collapses to at tiny sizes. */
const MIST = `<linearGradient id="ground" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#d6dce0"/><stop offset="1" stop-color="#bac2c8"/>
    </linearGradient>`;
/** Everything under the line, carried to the bottom of the box. */
const BELOW = `${CURVE} L${S + OVER} ${S + OVER} L${-OVER} ${S + OVER} Z`;

/** Plain night. What the horizon collapses to at tiny sizes. */
const NIGHT = {
  defs: `<linearGradient id="ground" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#232329"/><stop offset="1" stop-color="#0b0b0d"/>
    </linearGradient>`,
  body: `  <rect width="${S}" height="${S}" fill="url(#ground)"/>`,
};

/** Ice horizon: the squelch line divides two materials rather than being drawn
 *  on one. Near-black sky above; below it — and inside the hump — a lifted
 *  deep-ice panel. The split is a hard edge, not a gradient, so unlike dawn it
 *  survives being shrunk. */
const HORIZON = {
  defs: `<linearGradient id="ground" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#1a1b21"/><stop offset="1" stop-color="#0a0a0c"/>
    </linearGradient>
    <linearGradient id="under" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#2c3a44"/><stop offset="1" stop-color="#060d12"/>
    </linearGradient>`,
  body: `  <rect width="${S}" height="${S}" fill="url(#ground)"/>
  <path d="${BELOW}" fill="url(#under)"/>`,
};

const light = (defs: string) => ({
  defs,
  body: `  <rect width="${S}" height="${S}" fill="url(#ground)"/>`,
});

function body(pal: typeof INK) {
  const barDefs = BARSET.map((b) => {
    const k = 1 - b.e; // 0 at the centre of the band, 1 out on the skirt
    return `<linearGradient id="b${b.i}" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${mix(pal.ctr[0], pal.edge[0], k)}"/>
      <stop offset=".5" stop-color="${mix(pal.ctr[1], pal.edge[1], k)}"/>
      <stop offset="1" stop-color="${mix(pal.ctr[2], pal.edge[2], k)}"/>
    </linearGradient>`;
  }).join("\n    ");

  const lineDef = `<linearGradient id="line" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="${pal.line[0]}"/>
      <stop offset=".22" stop-color="${pal.line[1]}"/>
      <stop offset=".5" stop-color="${pal.line[2]}"/>
      <stop offset=".78" stop-color="${pal.line[1]}"/>
      <stop offset="1" stop-color="${pal.line[0]}"/>
    </linearGradient>`;

  const rects = BARSET.map(
    (b) =>
      `    <rect x="${(b.cx - b.w / 2).toFixed(1)}" y="${(BASE - b.h).toFixed(1)}" width="${b.w.toFixed(1)}" height="${b.h.toFixed(1)}" rx="${(b.w / 2).toFixed(1)}" fill="url(#b${b.i})"/>`,
  ).join("\n");

  return {
    defs: `${barDefs}\n    ${lineDef}`,
    art: `  <g>\n${rects}\n  </g>\n  <path d="${CURVE}" fill="none" stroke="url(#line)" stroke-width="${STROKE}" stroke-linecap="round" stroke-linejoin="round"/>`,
  };
}

/** Full-bleed square: an app icon. The platform applies its own mask. */
function icon(ground: { defs: string; body: string }, pal: typeof INK) {
  const { defs, art } = body(pal);
  return `<svg viewBox="0 0 ${S} ${S}" xmlns="http://www.w3.org/2000/svg">
  <defs>
    ${ground.defs}
    ${defs}
  </defs>
${ground.body}
${art}
</svg>
`;
}

// Groundless marks are cropped to the art with half a stroke of bleed, so the
// line's round cap is not clipped at the top of the hump.
const VIEW = `0 ${(BASE - MAXH - STROKE / 2).toFixed(1)} ${S} ${(MAXH + STROKE).toFixed(1)}`;

/** No ground, trimmed to the art: a logo to drop on any surface. */
function mark(pal: typeof INK) {
  const { defs, art } = body(pal);
  return `<svg viewBox="${VIEW}" xmlns="http://www.w3.org/2000/svg">
  <defs>
    ${defs}
  </defs>
${art}
</svg>
`;
}

/** The bounding curve alone — no bars, no ground. The quietest form of the
 *  mark, for places where the spectrum would be noise: small lockups, and
 *  anywhere the bars already appear nearby. */
function line(pal: typeof INK) {
  return `<svg viewBox="${VIEW}" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <linearGradient id="line" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="${pal.line[0]}"/>
      <stop offset=".22" stop-color="${pal.line[1]}"/>
      <stop offset=".5" stop-color="${pal.line[2]}"/>
      <stop offset=".78" stop-color="${pal.line[1]}"/>
      <stop offset="1" stop-color="${pal.line[0]}"/>
    </linearGradient>
  </defs>
  <path d="${CURVE}" fill="none" stroke="url(#line)" stroke-width="${STROKE}" stroke-linecap="round" stroke-linejoin="round"/>
</svg>
`;
}

/** One colour, no gradients, painted with `currentColor` so an inline copy
 *  inherits whatever the surrounding text is. Standalone it renders black,
 *  which is the usual one-colour logo anyway. */
function mono() {
  const rects = BARSET.map(
    (b) =>
      `    <rect x="${(b.cx - b.w / 2).toFixed(1)}" y="${(BASE - b.h).toFixed(1)}" width="${b.w.toFixed(1)}" height="${b.h.toFixed(1)}" rx="${(b.w / 2).toFixed(1)}"/>`,
  ).join("\n");
  return `<svg viewBox="${VIEW}" xmlns="http://www.w3.org/2000/svg" fill="currentColor">
  <g>
${rects}
  </g>
  <path d="${CURVE}" fill="none" stroke="currentColor" stroke-width="${STROKE}" stroke-linecap="round" stroke-linejoin="round"/>
</svg>
`;
}

const here = new URL(".", import.meta.url).pathname;
const files: Array<[string, string]> = [
  // The icon, as chosen: ice on mist with dawn in the ground.
  ["svg/icon.svg", icon(light(DAWN), INK)],
  // The same without dawn. What the gradient collapses to below ~64px anyway,
  // and the one to use anywhere flat is wanted.
  ["svg/icon-flat.svg", icon(light(MIST), INK)],
  // The dark suite: ice horizon, for dark UI surfaces and anywhere the light
  // tile glares. Flat night is its small-size fallback.
  ["svg/icon-dark.svg", icon(HORIZON, LIT)],
  ["svg/icon-dark-flat.svg", icon(NIGHT, LIT)],
  // Groundless marks. Named for the surface they go on, not the ink they use:
  // `mark` for light surfaces, `mark-light` for dark ones.
  ["svg/mark.svg", mark(INK)],
  ["svg/mark-light.svg", mark(LIT)],
  // The curve alone, no spectrum.
  ["svg/mark-line.svg", line(INK)],
  ["svg/mark-line-light.svg", line(LIT)],
  // One colour, inherits `currentColor` when inlined.
  ["svg/mark-mono.svg", mono()],
];
for (const [rel, content] of files) await Bun.write(here + rel, content);

console.log(`geometry: ${BARSET.length} bars, spread ${(SPREAD * 100).toFixed(1)}% of canvas`);

// The bug that kept coming back: bar heights sampled at the centre let the
// outer half breach the curve on a shoulder. Fail the build rather than ship it.
let worst = Infinity;
for (const b of BARSET) {
  const top = BASE - b.h;
  for (let x = b.cx - b.w / 2; x <= b.cx + b.w / 2; x += 0.5)
    worst = Math.min(worst, top - (BASE - env(x / S) * MAXH));
}
if (worst < -0.01) throw new Error(`a bar breaches the squelch line by ${(-worst).toFixed(2)}px`);
console.log(`clearance: no bar breaches the line; tightest gap ${worst.toFixed(2)}px (0 = kissing)`);

console.log(`wrote ${files.length} svgs`);
