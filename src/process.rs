//! Decode an upload and encode it again, so the stored file holds only
//! pixels. Re-encoding drops EXIF, XMP, ICC profiles, text and private
//! chunks, embedded sub-images and anything appended after the image data,
//! which chunk stripping cannot do reliably.

use std::fmt;
use std::io::Cursor;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::{WebPDecoder, WebPEncoder};
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder, ImageReader, Limits};

pub const MAX_SIDE: u32 = 16_384;
pub const MAX_FRAME_PIXELS: u64 = 40_000_000;
pub const MAX_GIF_FRAMES: usize = 300;
pub const MAX_TOTAL_PIXELS: u64 = 400_000_000;
pub const MAX_ALLOC: u64 = 192 * 1024 * 1024;
pub const JPEG_QUALITY: u8 = 90;
// NeuQuant speed for GIF frames with more than 256 colours: 1 is best and
// slowest, 30 is fastest. 10 is the gif crate's suggested balance.
const GIF_QUANT_SPEED: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Png,
    Jpg,
    Gif,
    Webp,
}

impl Kind {
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpg => "jpg",
            Kind::Gif => "gif",
            Kind::Webp => "webp",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProcessError {
    /// not one of the four accepted formats, or an animated WebP
    Unsupported(&'static str),
    /// the file does not decode, or it breaks a size limit
    Invalid(String),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessError::Unsupported(m) => f.write_str(m),
            ProcessError::Invalid(m) => f.write_str(m),
        }
    }
}

fn invalid(e: impl fmt::Display) -> ProcessError {
    ProcessError::Invalid(e.to_string())
}

/// Identify the format from magic bytes. A file's name and declared type are
/// never trusted.
pub fn sniff(b: &[u8]) -> Option<Kind> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Kind::Jpg)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(Kind::Gif)
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else {
        None
    }
}

fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(MAX_SIDE);
    l.max_image_height = Some(MAX_SIDE);
    l.max_alloc = Some(MAX_ALLOC);
    l
}

fn check_frame(w: u32, h: u32) -> Result<(), ProcessError> {
    if w == 0 || h == 0 {
        return Err(invalid("image has no pixels"));
    }
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(invalid(format!("{w}x{h} exceeds {MAX_SIDE} px per side")));
    }
    if u64::from(w) * u64::from(h) > MAX_FRAME_PIXELS {
        return Err(invalid(format!(
            "{w}x{h} exceeds {MAX_FRAME_PIXELS} pixels"
        )));
    }

    Ok(())
}

/// Re-encode `input` in its own format. Returns the new bytes and the kind,
/// which also decides the stored file's extension.
pub fn process(input: &[u8]) -> Result<(Vec<u8>, Kind), ProcessError> {
    let kind = sniff(input).ok_or(ProcessError::Unsupported(
        "only PNG, JPEG, GIF and WebP are accepted",
    ))?;

    let out = match kind {
        Kind::Gif => reencode_gif(input)?,
        Kind::Webp => {
            let dec = WebPDecoder::new(Cursor::new(input)).map_err(invalid)?;
            if dec.has_animation() {
                return Err(ProcessError::Unsupported(
                    "animated WebP is not accepted; use GIF",
                ));
            }
            encode_still(decode_still(input, kind)?, kind)?
        }
        Kind::Png | Kind::Jpg => encode_still(decode_still(input, kind)?, kind)?,
    };

    Ok((out, kind))
}

fn decode_still(input: &[u8], kind: Kind) -> Result<DynamicImage, ProcessError> {
    let format = match kind {
        Kind::Png => image::ImageFormat::Png,
        Kind::Jpg => image::ImageFormat::Jpeg,
        Kind::Webp => image::ImageFormat::WebP,
        Kind::Gif => image::ImageFormat::Gif,
    };
    let mut reader = ImageReader::with_format(Cursor::new(input), format);
    reader.limits(limits());

    let mut dec = reader.into_decoder().map_err(invalid)?;
    let (w, h) = dec.dimensions();
    check_frame(w, h)?;
    // orientation lives in EXIF, which re-encoding drops, so bake it into the pixels
    let orientation = dec.orientation().map_err(invalid)?;

    let mut img = DynamicImage::from_decoder(dec).map_err(invalid)?;
    img.apply_orientation(orientation);
    Ok(img)
}

fn encode_still(img: DynamicImage, kind: Kind) -> Result<Vec<u8>, ProcessError> {
    let mut out = Vec::new();

    match kind {
        // adaptive filtering keeps screenshots near their original size; the
        // encoder's default filter made a 3 MB 5K screenshot 7.4 MB
        Kind::Png => img
            .write_with_encoder(PngEncoder::new_with_quality(
                &mut out,
                CompressionType::Default,
                FilterType::Adaptive,
            ))
            .map_err(invalid)?,
        Kind::Jpg => {
            // JPEG has no alpha channel
            let rgb = img.to_rgb8();
            JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
                .write_image(
                    &rgb,
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(invalid)?;
        }
        Kind::Webp => {
            let rgba = img.to_rgba8();
            WebPEncoder::new_lossless(&mut out)
                .write_image(
                    &rgba,
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(invalid)?;
        }
        Kind::Gif => unreachable!("GIF goes through reencode_gif"),
    }

    Ok(out)
}

/// Decode and encode one frame at a time, so memory holds one frame rather
/// than the whole animation.
fn reencode_gif(input: &[u8]) -> Result<Vec<u8>, ProcessError> {
    let mut dec = GifDecoder::new(Cursor::new(input)).map_err(invalid)?;
    dec.set_limits(limits()).map_err(invalid)?;
    let (w, h) = dec.dimensions();
    check_frame(w, h)?;

    let mut out = Vec::new();
    let mut count = 0usize;
    let mut total: u64 = 0;
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, GIF_QUANT_SPEED);
        // the loop extension has to precede the first frame; on a still GIF it has no effect
        enc.set_repeat(Repeat::Infinite).map_err(invalid)?;

        for frame in dec.into_frames() {
            let frame = frame.map_err(invalid)?;
            count += 1;
            if count > MAX_GIF_FRAMES {
                return Err(invalid(format!("more than {MAX_GIF_FRAMES} frames")));
            }
            total += u64::from(frame.buffer().width()) * u64::from(frame.buffer().height());
            if total > MAX_TOTAL_PIXELS {
                return Err(invalid(format!(
                    "frames exceed {MAX_TOTAL_PIXELS} pixels in total"
                )));
            }

            enc.encode_frame(frame).map_err(invalid)?;
        }
    }

    if count == 0 {
        return Err(invalid("GIF has no frames"));
    }
    Ok(out)
}
