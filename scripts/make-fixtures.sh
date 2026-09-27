#!/usr/bin/env bash
# regenerates tests/fixtures. Every fixture carries "MARKER" in the places
# metadata hides (EXIF, XMP, comments, text chunks, ICC description) and
# ends with appended ZIP bytes, so tests can prove re-encoding removed them.
set -euo pipefail
cd "$(dirname "$0")/../tests/fixtures"
rm -f ./*.zip ./*.png ./*.jpg ./*.gif ./*.webp

uv run --no-project --with pillow python - <<'PY'
import io, zipfile
from PIL import Image, ImageCms, PngImagePlugin

def pattern(w, h):
    img = Image.new("RGB", (w, h))
    img.putdata([((x * 37) % 256, (y * 53) % 256, ((x + y) * 11) % 256) for y in range(h) for x in range(w)])
    return img

prof = ImageCms.createProfile("sRGB")
icc_bytes = ImageCms.ImageCmsProfile(prof).tobytes()
# overwrite the description in place with a same-length marker; it may be stored as ASCII or UTF-16
for enc in ("ascii", "utf-16-be"):
    icc_bytes = icc_bytes.replace("sRGB built-in".encode(enc), "MARKER-ICC-XX".encode(enc))
assert b"MARKER" in icc_bytes or "MARKER".encode("utf-16-be") in icc_bytes

img = pattern(24, 16)
info = PngImagePlugin.PngInfo()
info.add_text("Comment", "MARKER-TEXT")
info.add_itxt("Description", "MARKER-ITXT")
img.save("in.png", pnginfo=info, icc_profile=icc_bytes)
img.save("in.jpg", quality=95, icc_profile=icc_bytes, comment=b"MARKER-COM")
img.save("in.webp", lossless=True, icc_profile=icc_bytes)
img.convert("P", palette=Image.ADAPTIVE, colors=64).save("in.gif", comment=b"MARKER-GIFCOMMENT")

frames = [pattern(12, 12).rotate(90 * i) for i in range(3)]
frames[0].convert("P", palette=Image.ADAPTIVE, colors=64).save(
    "anim.gif", save_all=True, append_images=[f.convert("P", palette=Image.ADAPTIVE, colors=64) for f in frames[1:]],
    duration=100, loop=0, comment=b"MARKER-ANIM")
frames[0].save("anim.webp", save_all=True, append_images=frames[1:], duration=100, loop=0, lossless=True)

# 3 wide, 2 tall, stored sideways with EXIF orientation 6 (rotate 90 clockwise to display)
sideways = pattern(2, 3)
exif = Image.Exif(); exif[0x0112] = 6
sideways.save("rotated.jpg", quality=100, exif=exif.tobytes())

with zipfile.ZipFile("trailer.zip", "w") as z:
    z.writestr("MARKER-ZIP.txt", "hidden")
PY

exiftool -q -overwrite_original -GPSLatitude=51.5 -GPSLatitudeRef=N -GPSLongitude=0.12 -GPSLongitudeRef=W \
  -Artist=MARKER-EXIF -XMP:Creator=MARKER-XMP in.png in.jpg in.webp
exiftool -q -overwrite_original -XMP:Creator=MARKER-XMP in.gif

# appended last, because exiftool refuses to edit files with trailing data
for f in in.png in.jpg in.webp in.gif; do cat trailer.zip >> "$f"; done
rm trailer.zip
ls -l
