#!/usr/bin/env bash
# re-encodes each fixture, then checks with exiftool (an independent parser)
# that no metadata tag survives and with Pillow that the output decodes.
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

cargo build -q --release --example process
for f in in.png in.jpg in.webp in.gif anim.gif rotated.jpg; do
  target/release/examples/process "tests/fixtures/$f" "$out/$f"
done

left=$(exiftool -q -q -s -s -s -GPS:all -XMP:all -EXIF:all -ICC_Profile:all -Comment -Artist -Creator "$out"/* || true)
if [ -n "$left" ]; then echo "metadata survived:"; echo "$left"; exit 1; fi

uv run --no-project --with pillow python - "$out" <<'PY'
import sys
from PIL import Image
for n in ["in.png", "in.jpg", "in.webp", "in.gif", "anim.gif", "rotated.jpg"]:
    im = Image.open(f"{sys.argv[1]}/{n}")
    im.load()
    assert not im.info.get("icc_profile") and not im.info.get("exif"), n
print("fixture outputs decode and carry no EXIF or ICC")
PY
