#!/usr/bin/env bash
# Regenerate docs/media/pgwhy-promo.mp4 from the real app.
#
#   scripts/promo.sh
#
# Requires: Docker, Postgres 16 client tools, ffmpeg, Pillow (`python3 -c "import PIL"`).
# The frames come from the app's own driver (`e2e/specs/promo.e2e.ts`), so no screen-recording
# permission is needed and nothing of the surrounding desktop is captured.
set -euo pipefail
cd "$(dirname "$0")/.."

frames="${PROMO_FRAMES:-/tmp/pgwhy-promo/frames}"
out="${PROMO_OUT:-docs/media/pgwhy-promo.mp4}"

# 1. The e2e build. This matters: `cargo test` / `cargo clippy` rebuild target/debug/pgwhy
#    *without* --features e2e, and the embedded WebDriver is then missing.
npm --prefix e2e run build:app

# 2. Drive the app and capture one screenshot per settled beat.
rm -rf "$frames" && mkdir -p "$frames"
(cd e2e && E2E_PROMO=1 E2E_PROMO_DIR="$frames" npx wdio run ./wdio.conf.ts --spec ./specs/promo.e2e.ts)

# 3. Cut: replay the recorded motion on its real timing, retime each beat to its slot,
#    interpolate the continuous-motion beats, lay captions on top, join with hard cuts.
python3 scripts/promo.py "$frames" "$out"

# 4. A poster frame for listings/thumbnails.
ffmpeg -v error -y -ss 11 -i "$out" -frames:v 1 -q:v 3 "${out%.mp4}-poster.jpg"
echo "wrote $out"
