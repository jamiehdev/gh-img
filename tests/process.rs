use std::io::Cursor;

use gh_img::process::{MAX_GIF_FRAMES, MAX_SIDE};
use gh_img::{Kind, ProcessError, process, sniff};
use image::codecs::gif::{GifDecoder, GifEncoder};
use image::{AnimationDecoder, Frame, ImageReader, RgbaImage};
use proptest::prelude::*;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn utf16be(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

fn decode(bytes: &[u8]) -> RgbaImage {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap()
        .to_rgba8()
}

fn gif_frames(bytes: &[u8]) -> Vec<RgbaImage> {
    GifDecoder::new(Cursor::new(bytes))
        .unwrap()
        .into_frames()
        .map(|f| f.unwrap().into_buffer())
        .collect()
}

#[test]
fn sniff_identifies_each_fixture() {
    assert_eq!(sniff(&fixture("in.png")), Some(Kind::Png));
    assert_eq!(sniff(&fixture("in.jpg")), Some(Kind::Jpg));
    assert_eq!(sniff(&fixture("in.gif")), Some(Kind::Gif));
    assert_eq!(sniff(&fixture("in.webp")), Some(Kind::Webp));
}

#[test]
fn sniff_rejects_scripts_and_near_misses() {
    for bad in [
        &b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>"[..],
        b"<!doctype html><script>alert(1)</script>",
        b"PNG\r\n<script>",
        b"\x89PNG\r\n\x1a",
        b"RIFF\0\0\0\0WEB",
        b"",
    ] {
        assert_eq!(sniff(bad), None, "{:?}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn process_rejects_unsupported_input() {
    assert!(matches!(
        process(b"<svg></svg>"),
        Err(ProcessError::Unsupported(_))
    ));
    assert!(matches!(
        process(&fixture("anim.webp")),
        Err(ProcessError::Unsupported(_))
    ));
}

#[test]
fn every_fixture_loses_metadata_icc_and_appended_zip() {
    for name in ["in.png", "in.jpg", "in.webp", "in.gif", "anim.gif"] {
        let input = fixture(name);
        assert!(
            contains(&input, b"MARKER") || contains(&input, &utf16be("MARKER")),
            "{name} fixture has no marker"
        );

        let (out, kind) = process(&input).unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(Some(kind), sniff(&input), "{name} changed format");
        assert_eq!(sniff(&out), Some(kind), "{name} output is not {kind:?}");
        for needle in [
            &b"MARKER"[..],
            &utf16be("MARKER"),
            b"PK\x03\x04",
            b"GPS",
            b"Exif",
            b"iCCP",
            b"ICC_PROFILE",
            b"ICCP",
            b"XMP",
        ] {
            assert!(
                !contains(&out, needle),
                "{name} output still contains {:?}",
                String::from_utf8_lossy(needle)
            );
        }
    }
}

#[test]
fn lossless_formats_keep_exact_pixels() {
    for name in ["in.png", "in.webp"] {
        let input = fixture(name);
        let (out, _) = process(&input).unwrap();
        assert_eq!(decode(&out), decode(&input), "{name}");
    }

    for name in ["in.gif", "anim.gif"] {
        let input = fixture(name);
        let (out, _) = process(&input).unwrap();
        assert_eq!(gif_frames(&out), gif_frames(&input), "{name}");
    }
}

#[test]
fn jpeg_stays_close_to_the_original() {
    let input = fixture("in.jpg");
    let (out, _) = process(&input).unwrap();
    let (a, b) = (decode(&input), decode(&out));
    assert_eq!(a.dimensions(), b.dimensions());

    let total: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum();
    let mean = total as f64 / a.as_raw().len() as f64;
    assert!(mean < 6.0, "mean channel error {mean}");
}

#[test]
fn jpeg_orientation_is_applied_to_the_pixels() {
    let input = fixture("rotated.jpg");
    assert_eq!(decode(&input).dimensions(), (2, 3));

    let (out, _) = process(&input).unwrap();

    // stored 2x3 with orientation 6 (rotate 90 clockwise), so it displays 3x2
    assert_eq!(decode(&out).dimensions(), (3, 2));
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A real PNG whose IHDR claims `w` x `h`, with a valid CRC, so only the
/// dimension check can reject it.
fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    let mut png = fixture("in.png");
    png[16..20].copy_from_slice(&w.to_be_bytes());
    png[20..24].copy_from_slice(&h.to_be_bytes());
    let crc = crc32(&png[12..29]);
    png[29..33].copy_from_slice(&crc.to_be_bytes());
    png
}

#[test]
fn oversized_dimensions_are_rejected_before_decoding() {
    for (w, h) in [
        (MAX_SIDE + 1, 1),
        (1, MAX_SIDE + 1),
        (100_000, 100_000),
        (10_000, 10_000),
    ] {
        let result = process(&png_claiming(w, h));
        assert!(
            matches!(result, Err(ProcessError::Invalid(_))),
            "{w}x{h}: {result:?}"
        );
    }
}

#[test]
fn gif_with_too_many_frames_is_rejected() {
    let mut gif = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut gif);
        let frames = (0..=MAX_GIF_FRAMES).map(|i| {
            Frame::new(RgbaImage::from_pixel(
                1,
                1,
                image::Rgba([i as u8, 0, 0, 255]),
            ))
        });
        enc.encode_frames(frames).unwrap();
    }

    assert!(matches!(process(&gif), Err(ProcessError::Invalid(m)) if m.contains("frames")));
}

#[test]
fn truncated_files_are_rejected() {
    for name in ["in.png", "in.jpg", "in.webp", "in.gif"] {
        let input = fixture(name);
        let result = process(&input[..40]);
        assert!(result.is_err(), "{name} truncated to 40 bytes was accepted");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn damaged_fixtures_never_panic(
        which in 0usize..6,
        cut in any::<prop::sample::Index>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..8),
    ) {
        let names = ["in.png", "in.jpg", "in.webp", "in.gif", "anim.gif", "anim.webp"];
        let mut bytes = fixture(names[which]);
        for (at, value) in flips {
            let i = at.index(bytes.len());
            bytes[i] ^= value;
        }
        let len = cut.index(bytes.len() + 1);

        let _ = process(&bytes[..len]);
    }

    #[test]
    fn arbitrary_bytes_behind_a_valid_signature_never_panic(
        kind in 0usize..4,
        tail in prop::collection::vec(any::<u8>(), 0..4096),
    ) {
        let magic: [&[u8]; 4] = [b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff", b"GIF89a", b"RIFF\x10\0\0\0WEBP"];
        let mut bytes = magic[kind].to_vec();
        bytes.extend(tail);

        let _ = process(&bytes);
    }
}
