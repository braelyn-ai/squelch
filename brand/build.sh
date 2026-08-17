#!/usr/bin/env bash
# Regenerates the brand assets and installs them where they are consumed.
# Rasterizing needs macOS: qlmanage is the SVG renderer, which is fine because
# this repo already only builds the clients on a Mac.
#
#   ./brand/build.sh
#
set -euo pipefail
cd "$(dirname "$0")"

command -v bun >/dev/null || { echo "bun not on PATH" >&2; exit 1; }
command -v qlmanage >/dev/null || { echo "qlmanage not found (macOS only)" >&2; exit 1; }

bun run generate.ts

# ---- rasterize -------------------------------------------------------------
# qlmanage renders the vector at each target size rather than downsampling one
# big render, which keeps the small icons crisp instead of mushy. It also
# composites transparency onto white, so only the opaque icons go to PNG — the
# groundless marks stay SVG, which is the right format for them anyway.
rm -rf png && mkdir -p png
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for size in 16 32 48 64 128 180 256 512 1024; do
  # Below 64 the dawn gradient is a couple of pixel rows and reads as noise, so
  # the small end ships the flat ground it would have collapsed to anyway. The
  # dark suite's horizon is a hard edge rather than a gradient, so it holds all
  # the way down and only drops to flat at the two smallest sizes.
  src=svg/icon.svg
  [ "$size" -lt 64 ] && src=svg/icon-flat.svg
  qlmanage -t -s "$size" -o "$tmp" "$src" >/dev/null 2>&1
  mv "$tmp/$(basename "$src").png" "png/icon-${size}.png"

  dsrc=svg/icon-dark.svg
  [ "$size" -lt 32 ] && dsrc=svg/icon-dark-flat.svg
  qlmanage -t -s "$size" -o "$tmp" "$dsrc" >/dev/null 2>&1
  mv "$tmp/$(basename "$dsrc").png" "png/icon-dark-${size}.png"
done

# ---- transparent marks -----------------------------------------------------
# qlmanage flattens alpha onto white, so the groundless marks go through Chrome
# instead, which honours --default-background-color. The marks are 1024x557, so
# the window has to match that ratio or the render gets letterboxed.
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
if [ -x "$CHROME" ]; then
  for name in mark mark-light mark-line mark-line-light mark-mono; do
    for w in 256 512 1024; do
      h=$(( (w * 557 + 512) / 1024 ))
      "$CHROME" --headless --disable-gpu --force-device-scale-factor=1 \
        --default-background-color=00000000 --window-size="$w,$h" \
        --screenshot="png/${name}-${w}.png" "file://$PWD/svg/${name}.svg" >/dev/null 2>&1
    done
  done
else
  echo "warning: Chrome not found; skipped transparent mark PNGs (SVGs still written)" >&2
fi

# ---- install ---------------------------------------------------------------
# The site's Docker context is passband-site/, so its assets have to live
# inside that directory rather than being referenced out of here.
cp svg/icon.svg            ../passband-site/icon.svg
cp svg/icon-dark.svg       ../passband-site/icon-dark.svg
# The landing page shows the mark on its own dark field, not the tile.
cp svg/mark-light.svg      ../passband-site/mark.svg
# Raster copy for the invite email: no mail client renders SVG. Ink ramps,
# because a mail body is white.
cp png/mark-512.png        ../passband-site/mark.png
cp png/icon-180.png        ../passband-site/icon-180.png
cp png/icon-512.png        ../passband-site/icon-512.png

# Icon Composer takes a full-bleed square and applies the platform mask itself.
cp png/icon-1024.png       ../passband/Passband.icon/Assets/signal.png

echo
echo "installed:"
echo "  passband-site/icon.svg, icon-180.png, icon-512.png"
echo "  passband/Passband.icon/Assets/signal.png"
ls -la png | tail -n +2 | awk '{printf "  %-22s %s\n", $9, $5}'
