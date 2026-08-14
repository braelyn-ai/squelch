# Passband brand assets

Everything here is generated. `generate.ts` holds the geometry and the palette;
`build.sh` renders it and copies the results to the places that consume them.
Edit the generator, never the output.

```sh
./brand/build.sh
```

Needs `bun` and macOS (`qlmanage` is the SVG rasterizer).

## The mark

The hovered download button on the site, frozen: a spectrum bounded by its own
bandpass curve. Bars are signal, the curve is the filter that admits them, and
the flat runs off both edges are the squelch line continuing past the frame.
Every bar's ceiling *is* the curve, so the line is a constraint the bars are
grazing rather than a decoration laid over them.

Two rules are enforced in `generate.ts` and will fail the build:

- **No bar breaches the line.** A bar has width, so sampling the envelope at its
  centre lets the outer half rise above a curve that has already dropped on a
  shoulder. Heights come from the envelope's *minimum* across the bar's width.
- **Bar spread is solved, not tuned.** The outermost bar always lands at 20% of
  the envelope. Change the curve's shape and the spread follows, instead of the
  outer bars silently shrinking or being culled.

## Palette

Ice on mist, with dawn in the ground.

| role | value | notes |
|---|---|---|
| ink, centre of band | `#1f7099` | bars deep in the passband |
| ink, skirt | `#5b8298` | bars out on the shoulders |
| line, crest | `#0b3b57` | darkest point of the sheen |
| ground, sky | `#c9d3db` | cool, overhead |
| ground, at the line | `#ece0dc` | the warm band, at y=0.754 |
| ground, below | `#b8c0c6` | cools again under the line |

The warm stop sits at `.754` because that is exactly where the squelch line is,
so the temperature change reads as light on a horizon rather than as a gradient
dropped on the square. It is deliberately barely a colour — a rose-grey shift
rather than peach — which is what keeps it in Mist's character instead of
turning the icon into a sunset.

On light grounds the bars use **ink** ramps — the hue held saturated and taken
deep. They are never the light ramp darkened: darkening a hue mechanically walks
it toward mud rather than toward depth. `icon-dark.svg` carries the inverse
(**lit**) ramps for dark surfaces.

## Files

| file | use |
|---|---|
| `svg/icon.svg` | the icon. Full-bleed square; the platform applies its own mask |
| `svg/icon-flat.svg` | same, without the sunset. What it collapses to below ~64px |
| `svg/icon-dark.svg` | for dark surfaces, where the light tile glares |
| `svg/mark.svg` | groundless, trimmed to the art. For arbitrary surfaces |
| `svg/mark-light.svg` | groundless, lit ramps, for dark surfaces |
| `png/icon-*.png` | 16 → 1024. Under 64px these render from `icon-flat` |

PNGs are opaque only. `qlmanage` composites transparency onto white, so the
groundless marks stay SVG — which is the correct format for a logo regardless.

## Installed copies

`build.sh` copies into place; do not edit the copies.

| destination | source | why there |
|---|---|---|
| `passband-site/icon.svg` | `svg/icon.svg` | favicon + hero. The site's Docker context is `passband-site/`, so its assets cannot be referenced out of this directory |
| `squelch-control/src/pages.rs` (`MARK`) | `svg/mark-mono.svg` | the signup masthead, pasted into the Rust source rather than copied as a file. Those pages serve under `default-src 'none'`, so a logo has to be inline markup instead of something the browser fetches. The bars are verbatim; the curve is simplified to 39 points (see the const's own comment), which keeps the no-breach rule this generator enforces |
| `passband-site/icon-180.png` | `png/icon-180.png` | `apple-touch-icon` |
| `passband-site/icon-512.png` | `png/icon-512.png` | link unfurls |
| `passband/Passband.icon/Assets/signal.png` | `png/icon-1024.png` | Icon Composer layer, Mac + iOS |

## Known divergence

`docs/UX-DIRECTIONS.md` still describes the app icon as a machined brass squelch
knob and builds a warm-paper, brass-hairline design language on top of it. The
download button on the live site is brass for the same reason. Neither has been
changed — the icon moved to ice, and the rest of the brand has not followed yet.
