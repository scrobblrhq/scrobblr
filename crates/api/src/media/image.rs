//! Turns an uploaded file into the image we store: the format is detected
//! from its bytes, never its name or Content-Type, and the result is always
//! a fresh JPEG, so no metadata (EXIF, GPS) survives.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits, RgbImage};

/// Larger sources are refused before decoding: the body limit only bounds
/// the compressed size, and a small file can declare a huge bitmap.
pub const MAX_SOURCE_SIDE: u32 = 12_000;
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// What a decoder may allocate; 16-bit and alpha sources need more per pixel.
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const JPEG_QUALITY: u8 = 85;

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    NotAnImage,
    TooLarge,
}

/// Decodes `bytes` (JPEG, PNG or WebP), applies its EXIF orientation, fits
/// it within `max_side` pixels, flattens transparency onto white and
/// encodes it as JPEG. CPU-bound: call it off the async runtime.
pub fn normalize(bytes: &[u8], max_side: u32) -> Result<Vec<u8>, Rejected> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Rejected::NotAnImage)?;
    if !matches!(
        reader.format(),
        Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)
    ) {
        return Err(Rejected::NotAnImage);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);

    let mut decoder = reader.into_decoder().map_err(rejection)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(Rejected::TooLarge);
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(rejection)?;

    // Downscale before rotating: the bounding box is square, and turning
    // the small image is cheap.
    if image.width() > max_side || image.height() > max_side {
        image = image.thumbnail(max_side, max_side);
    }
    image.apply_orientation(orientation);

    let rgb = flatten(image);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|_| Rejected::NotAnImage)?;
    Ok(out)
}

fn rejection(e: ImageError) -> Rejected {
    match e {
        ImageError::Limits(_) => Rejected::TooLarge,
        _ => Rejected::NotAnImage,
    }
}

/// Transparent pixels over white rather than whatever color they hide.
fn flatten(image: DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.into_rgb8();
    }
    let rgba = image.into_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let over_white =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([over_white(r), over_white(g), over_white(b)])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn gradient(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }))
    }

    fn decode(jpeg: &[u8]) -> DynamicImage {
        assert_eq!(image::guess_format(jpeg).unwrap(), ImageFormat::Jpeg);
        image::load_from_memory(jpeg).unwrap()
    }

    /// A PNG declaring `width`×`height` with a few bytes of pixel data:
    /// decoding it would allocate the declared bitmap.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in bytes {
                crc ^= u32::from(byte);
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
        let mut header = width.to_be_bytes().to_vec();
        header.extend(height.to_be_bytes());
        header.extend([8, 2, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        for (name, data) in [
            (b"IHDR", header),
            (b"IDAT", vec![0x78, 0x9c]),
            (b"IEND", vec![]),
        ] {
            let mut chunk = name.to_vec();
            chunk.extend(&data);
            png.extend((data.len() as u32).to_be_bytes());
            png.extend(&chunk);
            png.extend(crc32(&chunk).to_be_bytes());
        }
        png
    }

    /// `jpeg` with an EXIF segment saying it is turned 90° clockwise, plus
    /// a GPS-looking marker that must not survive.
    fn with_exif_orientation(jpeg: &[u8]) -> Vec<u8> {
        let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08".to_vec();
        tiff.extend([0x00, 0x01]);
        tiff.extend([
            0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00,
        ]);
        tiff.extend([0x00; 4]);
        tiff.extend(b"GPSLatitude 40.4168 N");
        let mut segment = b"Exif\x00\x00".to_vec();
        segment.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend(((segment.len() + 2) as u16).to_be_bytes());
        out.extend(segment);
        out.extend(&jpeg[2..]);
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn every_accepted_format_becomes_a_jpeg_of_the_same_size() {
        let source = gradient(40, 30);
        for format in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP] {
            let out = normalize(&encode(&source, format), 1024).unwrap();
            let image = decode(&out);
            assert_eq!((image.width(), image.height()), (40, 30), "{format:?}");
        }
    }

    #[test]
    fn large_images_are_fitted_within_the_limit_and_small_ones_never_grown() {
        let out =
            decode(&normalize(&encode(&gradient(3000, 1000), ImageFormat::Png), 512).unwrap());
        assert_eq!(out.width(), 512);
        assert!((170..=171).contains(&out.height()));
        let out =
            decode(&normalize(&encode(&gradient(1000, 3000), ImageFormat::Png), 512).unwrap());
        assert_eq!(out.height(), 512);
        let out = decode(&normalize(&encode(&gradient(20, 10), ImageFormat::Png), 512).unwrap());
        assert_eq!((out.width(), out.height()), (20, 10));
        let out = decode(&normalize(&encode(&gradient(4000, 1), ImageFormat::Png), 512).unwrap());
        assert_eq!((out.width(), out.height()), (512, 1));
    }

    #[test]
    fn exif_orientation_is_applied_and_the_metadata_dropped() {
        let jpeg = with_exif_orientation(&encode(&gradient(20, 10), ImageFormat::Jpeg));
        assert!(contains(&jpeg, b"Exif\0\0"));
        let out = normalize(&jpeg, 1024).unwrap();
        assert!(!contains(&out, b"Exif"));
        assert!(!contains(&out, b"GPSLatitude"));
        let image = decode(&out);
        assert_eq!((image.width(), image.height()), (10, 20));
    }

    #[test]
    fn transparency_is_flattened_onto_white() {
        let clear = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0])));
        let image =
            decode(&normalize(&encode(&clear, ImageFormat::Png), 1024).unwrap()).into_rgb8();
        assert!(image.pixels().all(|p| p.0.iter().all(|&c| c >= 250)));
    }

    #[test]
    fn oversized_sources_are_refused_before_decoding() {
        assert_eq!(
            normalize(&png_header(MAX_SOURCE_SIDE + 1, 10), 512),
            Err(Rejected::TooLarge)
        );
        assert_eq!(
            normalize(&png_header(8_000, 6_000), 512),
            Err(Rejected::TooLarge)
        );
    }

    #[test]
    fn other_formats_and_garbage_are_not_images() {
        for bytes in [
            &b""[..],
            b"GIF89a\x01\x00\x01\x00\x00\x00\x00;",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            b"BM\x00\x00\x00\x00",
            b"%PDF-1.7",
            b"\xff\xd8\xff",
            &[0u8; 64],
        ] {
            assert_eq!(
                normalize(bytes, 512),
                Err(Rejected::NotAnImage),
                "{bytes:?}"
            );
        }
    }

    /// Truncated and corrupted files of every accepted format: each must be
    /// refused or decoded, never panic (a panic would be a 500 for a bad
    /// upload).
    #[test]
    fn malformed_files_never_panic() {
        let source = gradient(64, 48);
        for format in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP] {
            let valid = encode(&source, format);
            for len in (0..valid.len()).step_by(7) {
                let _ = normalize(&valid[..len], 512);
            }
            for position in (0..valid.len()).step_by(3) {
                for flip in [0x01, 0x80, 0xff] {
                    let mut corrupt = valid.clone();
                    corrupt[position] ^= flip;
                    let _ = normalize(&corrupt, 512);
                }
            }
        }
    }
}
