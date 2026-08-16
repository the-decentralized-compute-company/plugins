//! Decode, downscale, and re-encode an image for a chat completions request.
//!
//! # Why downscale at all
//!
//! A vision encoder does not see pixels; it sees a fixed grid of patches cut
//! out of whatever you send. A 12 MP phone photo is roughly 16 MB of base64 in
//! the request body, costs thousands of image tokens, frequently trips a
//! server's request-size limit, and reaches the encoder as the same patch grid
//! a 1 MP version would have produced. Downscaling before sending is not a
//! quality compromise — it is removing bytes that were never going to be
//! looked at.
//!
//! # Why the size is capped before decoding, too
//!
//! Dimensions come out of the file header, and a header is caller-controlled.
//! A 40 KiB PNG can declare 60000x60000, which is 14 GiB of RGBA the moment it
//! is decoded. Dimensions are therefore read and checked *before* a decode is
//! attempted, and the decoder is additionally given its own allocation limits.
//! On hardware somebody contributed to a mesh, an out-of-memory kill is a
//! denial of service this plugin would have shipped.

use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat as SourceFormat, ImageReader};

use crate::config::{ImageFormat as ConfiguredFormat, Limits};

/// Pixel budget under which a lossless source stays lossless.
///
/// Screenshots, diagrams, and scanned documents are where JPEG's ringing
/// artefacts around hard edges actually cost a model accuracy, and they are
/// also the images that compress well as PNG. Above this size a PNG of a real
/// photograph is several megabytes, so JPEG wins on every axis.
pub const PNG_PIXEL_BUDGET: u64 = 1_200_000;

/// What was sent, and what it came from. Every number here ends up in the tool
/// result so a caller can see that downscaling happened rather than guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub source_format: &'static str,
    pub source_bytes: usize,
    pub downscaled: bool,
}

impl Rendered {
    /// The `data:` URI that goes into an `image_url` content part.
    ///
    /// This is the whole reason the plugin re-encodes: the chat completions
    /// image format takes a URL, and an inline base64 `data:` URI is the only
    /// form that does not require the inference server to reach back out over
    /// the network to fetch something.
    pub fn as_data_uri(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.media_type,
            BASE64.encode(&self.bytes)
        )
    }
}

/// The dimensions an image is resized to so its longest edge is at most
/// `max_edge`, preserving aspect ratio.
///
/// Never upscales: an image already inside the budget is left alone, because
/// inventing pixels costs tokens and adds nothing. Both edges are floored at 1
/// so a 4000x3 panorama does not resize to a zero-height image, which every
/// encoder rejects.
pub fn scaled_dimensions(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_edge || longest == 0 {
        return (width, height);
    }
    let ratio = f64::from(max_edge) / f64::from(longest);
    let scaled_width = (f64::from(width) * ratio).round().max(1.0) as u32;
    let scaled_height = (f64::from(height) * ratio).round().max(1.0) as u32;
    (scaled_width, scaled_height)
}

/// Which encoding the resized image is written as.
///
/// `auto` keeps a small lossless source lossless — a screenshot of a terminal
/// stays crisp — and sends everything else as JPEG. An explicit
/// `--image-format` always wins, because an operator who measured their own
/// model's behaviour outranks this heuristic.
pub fn choose_encoding(
    configured: ConfiguredFormat,
    source: Option<SourceFormat>,
    pixels: u64,
) -> &'static str {
    match configured {
        ConfiguredFormat::Jpeg => "image/jpeg",
        ConfiguredFormat::Png => "image/png",
        ConfiguredFormat::Auto => {
            let lossless_source = matches!(
                source,
                Some(
                    SourceFormat::Png | SourceFormat::Bmp | SourceFormat::Tiff | SourceFormat::Gif
                )
            );
            if lossless_source && pixels <= PNG_PIXEL_BUDGET {
                "image/png"
            } else {
                "image/jpeg"
            }
        }
    }
}

/// A stable name for a source format, for the tool result.
fn format_label(format: Option<SourceFormat>) -> &'static str {
    match format {
        Some(SourceFormat::Png) => "png",
        Some(SourceFormat::Jpeg) => "jpeg",
        Some(SourceFormat::Gif) => "gif",
        Some(SourceFormat::WebP) => "webp",
        Some(SourceFormat::Bmp) => "bmp",
        Some(SourceFormat::Tiff) => "tiff",
        _ => "unknown",
    }
}

/// Decode, downscale, and re-encode one image.
///
/// `declared` is the media type the caller claimed, used only to make an error
/// message useful — the actual format comes from sniffing the bytes, because a
/// `data:image/png` header on JPEG bytes is a mislabelling this should survive
/// rather than a reason to fail.
pub fn render(bytes: &[u8], declared: Option<&str>, limits: &Limits) -> Result<Rendered, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("those image bytes could not be read: {error}"))?;
    let source_format = reader.format();
    if source_format.is_none() {
        return Err(format!(
            "those bytes are not an image format this plugin can decode{}. Supported: {}.",
            declared
                .map(|declared| format!(" (the caller labelled them `{declared}`)"))
                .unwrap_or_default(),
            crate::source::SUPPORTED_MEDIA_TYPES.join(", ")
        ));
    }

    // Dimensions first, from the header, before a single pixel is allocated.
    let (source_width, source_height) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("those image bytes could not be read: {error}"))?
        .into_dimensions()
        .map_err(|error| format!("that image's dimensions could not be read: {error}"))?;
    let pixels = u64::from(source_width) * u64::from(source_height);
    if pixels == 0 {
        return Err("that image has a zero width or height.".to_string());
    }
    if pixels > limits.max_pixels {
        return Err(format!(
            "that image is {source_width}x{source_height} ({pixels} pixels), over the \
             {}-pixel limit for one image. The limit is a decompression-bomb guard, not a \
             preference; raise --max-pixels if the image is genuinely that large.",
            limits.max_pixels
        ));
    }

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("those image bytes could not be read: {error}"))?;
    let mut decoder_limits = image::Limits::default();
    decoder_limits.max_image_width = Some(u32::MAX);
    decoder_limits.max_image_height = Some(u32::MAX);
    // Four bytes per pixel plus generous slack for a decoder's own scratch
    // buffers. Belt and braces alongside the explicit pixel check above.
    decoder_limits.max_alloc = Some(limits.max_pixels.saturating_mul(6));
    reader.limits(decoder_limits);

    let decoded = reader
        .decode()
        .map_err(|error| format!("that image could not be decoded: {error}"))?;

    let (width, height) = scaled_dimensions(source_width, source_height, limits.max_dimension);
    let downscaled = (width, height) != (source_width, source_height);
    let resized = if downscaled {
        // Lanczos3 rather than a nearest or triangle filter: this plugin's
        // `read_text` tool asks a model to read small glyphs out of the result,
        // and a cheaper filter is where that legibility goes.
        decoded.resize(width, height, FilterType::Lanczos3)
    } else {
        decoded
    };

    let media_type = choose_encoding(
        limits.image_format,
        source_format,
        u64::from(width) * u64::from(height),
    );
    let encoded = encode(&resized, media_type, limits.jpeg_quality)?;

    Ok(Rendered {
        media_type,
        bytes: encoded,
        width,
        height,
        source_width,
        source_height,
        source_format: format_label(source_format),
        source_bytes: bytes.len(),
        downscaled,
    })
}

/// Write the resized image out in the chosen encoding.
///
/// JPEG has no alpha channel, so a transparent source is flattened onto white
/// rather than onto whatever the encoder happens to leave behind — a PNG icon
/// with a transparent background otherwise arrives as a black rectangle with a
/// black glyph in it.
fn encode(image: &DynamicImage, media_type: &str, jpeg_quality: u8) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    match media_type {
        "image/jpeg" => {
            let flattened = flatten_onto_white(image);
            JpegEncoder::new_with_quality(&mut buffer, jpeg_quality)
                .encode_image(&flattened)
                .map_err(|error| format!("that image could not be re-encoded as JPEG: {error}"))?;
        }
        "image/png" => {
            let rgba = image.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut buffer)
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|error| format!("that image could not be re-encoded as PNG: {error}"))?;
        }
        other => return Err(format!("unsupported output encoding `{other}`")),
    }
    Ok(buffer)
}

/// Composite any alpha channel over white and return an opaque RGB image.
pub fn flatten_onto_white(image: &DynamicImage) -> DynamicImage {
    if !image.color().has_alpha() {
        return DynamicImage::ImageRgb8(image.to_rgb8());
    }
    let source = image.to_rgba8();
    let mut flattened = image::RgbImage::new(source.width(), source.height());
    for (x, y, pixel) in source.enumerate_pixels() {
        let alpha = f32::from(pixel[3]) / 255.0;
        let blend = |channel: u8| {
            (f32::from(channel) * alpha + 255.0 * (1.0 - alpha))
                .round()
                .clamp(0.0, 255.0) as u8
        };
        flattened.put_pixel(
            x,
            y,
            image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])]),
        );
    }
    DynamicImage::ImageRgb8(flattened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn limits(configure: impl FnOnce(&mut Limits)) -> Limits {
        let mut limits = Config::parse(&[], &Default::default())
            .expect("defaults parse")
            .limits;
        configure(&mut limits);
        limits
    }

    /// A deterministic test image: a coloured gradient with a black bar, so a
    /// resize visibly changes content rather than producing a flat field that
    /// any filter would get right.
    fn sample(width: u32, height: u32, format: SourceFormat) -> Vec<u8> {
        let mut buffer = image::RgbImage::new(width, height);
        for (x, y, pixel) in buffer.enumerate_pixels_mut() {
            let bar = y * 4 / height.max(1) == 1;
            *pixel = if bar {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
            };
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(buffer)
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("the sample encodes");
        bytes
    }

    fn sample_rgba(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let mut buffer = image::RgbaImage::new(width, height);
        for pixel in buffer.pixels_mut() {
            *pixel = image::Rgba([0, 0, 0, alpha]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(buffer)
            .write_to(&mut Cursor::new(&mut bytes), SourceFormat::Png)
            .expect("the sample encodes");
        bytes
    }

    /// The list this plugin advertises has to be the list the binary can
    /// actually read. `Cargo.toml` enables six decoder features by hand rather
    /// than taking `image`'s defaults, so dropping one there without editing
    /// [`crate::source::SUPPORTED_MEDIA_TYPES`] would leave the tool schema
    /// promising something the build cannot do. This test is what stops that.
    #[test]
    fn every_advertised_media_type_has_a_decoder_linked_into_this_build() {
        for media_type in crate::source::SUPPORTED_MEDIA_TYPES {
            let format = SourceFormat::from_mime_type(media_type)
                .unwrap_or_else(|| panic!("`{media_type}` is a media type `image` knows"));
            assert!(
                format.reading_enabled(),
                "`{media_type}` is advertised but its decoder feature is not enabled"
            );
        }
    }

    /// The other direction: nothing advertised is missing, and nothing is
    /// advertised twice.
    #[test]
    fn the_advertised_list_is_sorted_and_free_of_duplicates() {
        let mut sorted = crate::source::SUPPORTED_MEDIA_TYPES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), crate::source::SUPPORTED_MEDIA_TYPES);
    }

    #[test]
    fn the_other_lossless_formats_decode_and_re_encode() {
        for format in [SourceFormat::Bmp, SourceFormat::Gif, SourceFormat::Tiff] {
            let bytes = sample(200, 150, format);
            let rendered = render(&bytes, None, &limits(|_| {}))
                .unwrap_or_else(|error| panic!("{format:?} should render: {error}"));
            assert_eq!((rendered.width, rendered.height), (200, 150));
        }
    }

    #[test]
    fn an_image_inside_the_budget_is_left_at_its_own_size() {
        assert_eq!(scaled_dimensions(800, 600, 1_024), (800, 600));
        assert_eq!(scaled_dimensions(1_024, 1_024, 1_024), (1_024, 1_024));
        assert_eq!(scaled_dimensions(1, 1, 1_024), (1, 1));
    }

    #[test]
    fn the_longest_edge_lands_on_the_budget_and_the_ratio_survives() {
        assert_eq!(scaled_dimensions(4_032, 3_024, 1_024), (1_024, 768));
        assert_eq!(scaled_dimensions(3_024, 4_032, 1_024), (768, 1_024));
        assert_eq!(scaled_dimensions(2_048, 2_048, 512), (512, 512));
    }

    #[test]
    fn an_extreme_aspect_ratio_never_collapses_to_zero() {
        // 4000x3 at a 1024 budget is 0.768 pixels tall before flooring.
        let (width, height) = scaled_dimensions(4_000, 3, 1_024);
        assert_eq!(width, 1_024);
        assert!(height >= 1, "a zero-height image is rejected by encoders");
    }

    #[test]
    fn auto_keeps_a_small_lossless_source_lossless() {
        assert_eq!(
            choose_encoding(ConfiguredFormat::Auto, Some(SourceFormat::Png), 800 * 600),
            "image/png"
        );
        assert_eq!(
            choose_encoding(ConfiguredFormat::Auto, Some(SourceFormat::Bmp), 640 * 480),
            "image/png"
        );
    }

    #[test]
    fn auto_sends_photographs_and_large_images_as_jpeg() {
        assert_eq!(
            choose_encoding(ConfiguredFormat::Auto, Some(SourceFormat::Jpeg), 800 * 600),
            "image/jpeg"
        );
        assert_eq!(
            choose_encoding(ConfiguredFormat::Auto, Some(SourceFormat::WebP), 800 * 600),
            "image/jpeg"
        );
        // A large PNG is a photograph in a lossless wrapper; PNG would be
        // several megabytes for nothing.
        assert_eq!(
            choose_encoding(
                ConfiguredFormat::Auto,
                Some(SourceFormat::Png),
                PNG_PIXEL_BUDGET + 1
            ),
            "image/jpeg"
        );
    }

    #[test]
    fn an_explicit_encoding_always_wins_over_the_heuristic() {
        assert_eq!(
            choose_encoding(ConfiguredFormat::Png, Some(SourceFormat::Jpeg), 4_000_000),
            "image/png"
        );
        assert_eq!(
            choose_encoding(ConfiguredFormat::Jpeg, Some(SourceFormat::Png), 100),
            "image/jpeg"
        );
    }

    #[test]
    fn a_large_photo_is_downscaled_and_the_result_reports_both_sizes() {
        let bytes = sample(2_400, 1_800, SourceFormat::Jpeg);
        let rendered = render(&bytes, Some("image/jpeg"), &limits(|_| {})).expect("renders");

        assert_eq!(
            (rendered.source_width, rendered.source_height),
            (2_400, 1_800)
        );
        assert_eq!((rendered.width, rendered.height), (1_024, 768));
        assert!(rendered.downscaled);
        assert_eq!(rendered.source_format, "jpeg");
        assert_eq!(rendered.media_type, "image/jpeg");
        assert!(
            rendered.bytes.len() < bytes.len(),
            "downscaling has to actually save bytes: {} -> {}",
            bytes.len(),
            rendered.bytes.len()
        );
    }

    #[test]
    fn a_small_image_is_re_encoded_but_not_resized() {
        let bytes = sample(320, 240, SourceFormat::Png);
        let rendered = render(&bytes, Some("image/png"), &limits(|_| {})).expect("renders");

        assert!(!rendered.downscaled);
        assert_eq!((rendered.width, rendered.height), (320, 240));
        assert_eq!(rendered.media_type, "image/png", "a small PNG stays a PNG");
    }

    #[test]
    fn the_rendered_bytes_are_a_decodable_image_of_the_reported_size() {
        let bytes = sample(2_000, 1_000, SourceFormat::Png);
        let rendered = render(&bytes, None, &limits(|_| {})).expect("renders");

        let round_tripped = ImageReader::new(Cursor::new(&rendered.bytes))
            .with_guessed_format()
            .expect("the output is readable")
            .decode()
            .expect("the output decodes");
        assert_eq!(round_tripped.width(), rendered.width);
        assert_eq!(round_tripped.height(), rendered.height);
    }

    #[test]
    fn the_data_uri_carries_the_media_type_and_round_trips() {
        let bytes = sample(64, 64, SourceFormat::Png);
        let rendered = render(&bytes, None, &limits(|_| {})).expect("renders");

        let uri = rendered.as_data_uri();
        assert!(uri.starts_with("data:image/png;base64,"), "{}", &uri[..40]);
        let payload = uri.split_once(',').expect("there is a comma").1;
        assert_eq!(BASE64.decode(payload).expect("decodes"), rendered.bytes);
    }

    #[test]
    fn a_mislabelled_data_uri_is_rendered_from_what_the_bytes_actually_are() {
        // PNG bytes claiming to be a JPEG. Sniffing wins; the caller's label is
        // only ever used to make an error message readable.
        let bytes = sample(100, 100, SourceFormat::Png);
        let rendered = render(&bytes, Some("image/jpeg"), &limits(|_| {})).expect("renders");
        assert_eq!(rendered.source_format, "png");
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused_with_the_supported_list() {
        let error = render(b"this is a text file, not a picture", None, &limits(|_| {}))
            .expect_err("text is not an image");
        assert!(error.contains("image/png"), "{error}");
    }

    #[test]
    fn a_truncated_image_fails_at_decode_rather_than_returning_a_partial_picture() {
        let bytes = sample(400, 400, SourceFormat::Png);
        let truncated = &bytes[..bytes.len() / 2];
        let error = render(truncated, None, &limits(|_| {})).expect_err("half a PNG is not a PNG");
        assert!(error.contains("could not be decoded"), "{error}");
    }

    #[test]
    fn an_image_over_the_pixel_cap_is_refused_before_it_is_decoded() {
        let bytes = sample(800, 800, SourceFormat::Png);
        let error = render(&bytes, None, &limits(|limits| limits.max_pixels = 100_000))
            .expect_err("over the pixel cap");

        assert!(error.contains("800x800"), "{error}");
        assert!(error.contains("--max-pixels"), "{error}");
        assert!(error.contains("decompression-bomb"), "{error}");
    }

    #[test]
    fn a_transparent_png_flattens_onto_white_rather_than_black_when_it_becomes_a_jpeg() {
        let bytes = sample_rgba(64, 64, 0);
        let rendered = render(
            &bytes,
            Some("image/png"),
            &limits(|limits| limits.image_format = ConfiguredFormat::Jpeg),
        )
        .expect("renders");

        let decoded = ImageReader::new(Cursor::new(&rendered.bytes))
            .with_guessed_format()
            .expect("readable")
            .decode()
            .expect("decodes")
            .to_rgb8();
        let pixel = decoded.get_pixel(32, 32);
        assert!(
            pixel[0] > 240 && pixel[1] > 240 && pixel[2] > 240,
            "a fully transparent source must land on white, got {pixel:?}"
        );
    }

    #[test]
    fn a_transparent_png_keeps_its_alpha_when_it_stays_a_png() {
        let bytes = sample_rgba(64, 64, 0);
        let rendered = render(&bytes, Some("image/png"), &limits(|_| {})).expect("renders");

        assert_eq!(rendered.media_type, "image/png");
        let decoded = ImageReader::new(Cursor::new(&rendered.bytes))
            .with_guessed_format()
            .expect("readable")
            .decode()
            .expect("decodes");
        assert!(decoded.color().has_alpha());
    }

    #[test]
    fn jpeg_quality_is_actually_applied() {
        let bytes = sample(600, 600, SourceFormat::Jpeg);
        let low = render(
            &bytes,
            None,
            &limits(|limits| {
                limits.image_format = ConfiguredFormat::Jpeg;
                limits.jpeg_quality = 40;
            }),
        )
        .expect("renders");
        let high = render(
            &bytes,
            None,
            &limits(|limits| {
                limits.image_format = ConfiguredFormat::Jpeg;
                limits.jpeg_quality = 95;
            }),
        )
        .expect("renders");

        assert!(
            low.bytes.len() < high.bytes.len(),
            "quality 40 produced {} bytes and quality 95 produced {}",
            low.bytes.len(),
            high.bytes.len()
        );
    }
}
