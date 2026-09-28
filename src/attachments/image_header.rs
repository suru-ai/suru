//! Reads an image's format and pixel dimensions from its header bytes alone.
//!
//! Suru never decodes pixels on the Server (ADR 0037). The format is sniffed
//! from the magic bytes the image begins with, never taken from what a client
//! declared, and width and height come from the few header bytes each of the
//! four admitted formats keeps them in.

/// One of the four image formats an Attachment may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageFormat {
    pub(crate) fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::WebP => "WebP",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageHeader {
    pub(crate) format: ImageFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeaderError {
    /// The bytes begin like none of the four admitted formats.
    UnsupportedFormat,
    /// The bytes begin like this format, but its header is cut short, is
    /// malformed, or measures no pixels.
    Malformed(ImageFormat),
    /// A JPEG whose frame is coded other than as baseline, extended, or
    /// progressive Huffman — lossless or arithmetic coding, which next to
    /// nothing decodes.
    UnsupportedJpegCoding,
}

pub(crate) fn read(bytes: &[u8]) -> Result<ImageHeader, HeaderError> {
    let format = sniff(bytes).ok_or(HeaderError::UnsupportedFormat)?;
    let malformed = HeaderError::Malformed(format);
    let (width, height) = match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes)?,
        ImageFormat::Gif => gif_dimensions(bytes),
        ImageFormat::WebP => webp_dimensions(bytes),
    }
    .ok_or(malformed)?;
    if width == 0 || height == 0 {
        return Err(malformed);
    }
    Ok(ImageHeader {
        format,
        width,
        height,
    })
}

fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(ImageFormat::WebP)
    } else {
        None
    }
}

/// The IHDR chunk must come first, straight after the signature.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    (bytes.get(12..16)? == b"IHDR").then_some(())?;
    Some((be32(bytes, 16)?, be32(bytes, 20)?))
}

/// The logical screen descriptor follows the six-byte signature.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    Some((le16(bytes, 6)?.into(), le16(bytes, 8)?.into()))
}

/// Walks the segments after the start-of-image marker to the first frame
/// header, which carries height before width.
fn jpeg_dimensions(bytes: &[u8]) -> Result<Option<(u32, u32)>, HeaderError> {
    let mut at = 2;
    loop {
        if bytes.get(at) != Some(&0xFF) {
            return Ok(None);
        }
        // Any number of fill bytes may precede a marker.
        while bytes.get(at) == Some(&0xFF) {
            at += 1;
        }
        let Some(&marker) = bytes.get(at) else {
            return Ok(None);
        };
        at += 1;
        match marker {
            // Standalone markers carry no segment.
            0x01 | 0xD0..=0xD7 => {}
            // Baseline, extended sequential, and progressive Huffman frames.
            0xC0..=0xC2 => {
                return Ok(be16(bytes, at + 3)
                    .zip(be16(bytes, at + 5))
                    .map(|(height, width)| (width.into(), height.into())));
            }
            // Every other frame kind: lossless, or arithmetic coded.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(HeaderError::UnsupportedJpegCoding);
            }
            // A second start of image, the end of the image, a scan, or no
            // marker at all, each before any frame was described.
            0x00 | 0xD8 | 0xD9 | 0xDA => return Ok(None),
            _ => {
                let Some(length) = be16(bytes, at).filter(|length| *length >= 2) else {
                    return Ok(None);
                };
                at += usize::from(length);
            }
        }
    }
}

/// The first chunk decides which of the three WebP headers to read.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        // A lossy key frame: a start code, then fourteen bits of each
        // dimension whose top two bits are a scale.
        b"VP8 " => {
            (bytes.get(23..26)? == [0x9D, 0x01, 0x2A]).then_some(())?;
            Some((
                u32::from(le16(bytes, 26)? & 0x3FFF),
                u32::from(le16(bytes, 28)? & 0x3FFF),
            ))
        }
        // A lossless stream: a signature byte, then fourteen bits each of
        // width and height, both less one.
        b"VP8L" => {
            (*bytes.get(20)? == 0x2F).then_some(())?;
            let packed = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
            Some(((packed & 0x3FFF) + 1, ((packed >> 14) & 0x3FFF) + 1))
        }
        // An extended file: flags and reserved bytes, then twenty-four bits
        // each of canvas width and height, both less one.
        b"VP8X" => Some((le24(bytes, 24)? + 1, le24(bytes, 27)? + 1)),
        _ => None,
    }
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let [low, middle, high] = bytes.get(at..at + 3)?.try_into().ok()?;
    Some(u32::from_le_bytes([low, middle, high, 0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(13_u32.to_be_bytes());
        bytes.extend(b"IHDR");
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        // Bit depth, color type, compression, filter, interlace, then CRC.
        bytes.extend([8, 6, 0, 0, 0]);
        bytes.extend([0; 4]);
        bytes
    }

    fn gif(version: &[u8; 6], width: u16, height: u16) -> Vec<u8> {
        let mut bytes = version.to_vec();
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        // Packed fields, background color index, pixel aspect ratio.
        bytes.extend([0, 0, 0]);
        bytes
    }

    fn jpeg_segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFF, marker];
        bytes.extend(u16::try_from(body.len() + 2).unwrap().to_be_bytes());
        bytes.extend(body);
        bytes
    }

    fn jpeg_frame(marker: u8, width: u16, height: u16) -> Vec<u8> {
        let mut body = vec![8];
        body.extend(height.to_be_bytes());
        body.extend(width.to_be_bytes());
        // One component: identity, sampling factors, quantization table.
        body.extend([1, 1, 0x11, 0]);
        jpeg_segment(marker, &body)
    }

    fn jpeg(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        for segment in segments {
            bytes.extend(segment);
        }
        bytes.extend(jpeg_segment(0xDA, &[1, 1, 0, 0, 63, 0]));
        bytes
    }

    fn webp(chunk: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend(u32::try_from(4 + 8 + payload.len()).unwrap().to_le_bytes());
        bytes.extend(b"WEBP");
        bytes.extend(chunk);
        bytes.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    fn header(format: ImageFormat, width: u32, height: u32) -> Result<ImageHeader, HeaderError> {
        Ok(ImageHeader {
            format,
            width,
            height,
        })
    }

    #[test]
    fn a_png_is_measured_from_its_ihdr_chunk() {
        assert_eq!(read(&png(640, 480)), header(ImageFormat::Png, 640, 480));
        assert_eq!(
            read(&png(70_000, 1)),
            header(ImageFormat::Png, 70_000, 1),
            "a PNG's dimensions are four bytes wide"
        );
    }

    #[test]
    fn a_png_without_ihdr_first_is_malformed() {
        let mut bytes = png(640, 480);
        bytes[12..16].copy_from_slice(b"tEXt");
        assert_eq!(read(&bytes), Err(HeaderError::Malformed(ImageFormat::Png)));
        assert_eq!(
            read(&png(640, 480)[..20]),
            Err(HeaderError::Malformed(ImageFormat::Png)),
            "a header cut off before its height"
        );
    }

    #[test]
    fn a_gif_is_measured_from_its_logical_screen_in_either_version() {
        assert_eq!(
            read(&gif(b"GIF89a", 320, 200)),
            header(ImageFormat::Gif, 320, 200)
        );
        assert_eq!(
            read(&gif(b"GIF87a", 1, 65_535)),
            header(ImageFormat::Gif, 1, 65_535)
        );
        assert_eq!(
            read(&gif(b"GIF89a", 320, 200)[..9]),
            Err(HeaderError::Malformed(ImageFormat::Gif))
        );
    }

    #[test]
    fn a_jpeg_is_measured_from_the_first_baseline_extended_or_progressive_frame() {
        let app0 = jpeg_segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let quantization = jpeg_segment(0xDB, &[0; 65]);
        for marker in [0xC0, 0xC1, 0xC2] {
            assert_eq!(
                read(&jpeg(&[
                    app0.clone(),
                    quantization.clone(),
                    jpeg_frame(marker, 1920, 1080)
                ])),
                header(ImageFormat::Jpeg, 1920, 1080),
                "SOF marker {marker:#04x}"
            );
        }
    }

    #[test]
    fn a_jpeg_tolerates_fill_bytes_and_standalone_markers_between_segments() {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xFF, 0x01];
        bytes.extend(jpeg_frame(0xC0, 16, 9));
        assert_eq!(read(&bytes), header(ImageFormat::Jpeg, 16, 9));
    }

    #[test]
    fn a_jpeg_coded_losslessly_or_arithmetically_is_refused() {
        for marker in [0xC3, 0xC5, 0xC9, 0xCF] {
            assert_eq!(
                read(&jpeg(&[jpeg_frame(marker, 16, 9)])),
                Err(HeaderError::UnsupportedJpegCoding),
                "SOF marker {marker:#04x}"
            );
        }
    }

    #[test]
    fn a_jpeg_without_a_frame_before_its_scan_or_end_is_malformed() {
        assert_eq!(
            read(&jpeg(&[jpeg_segment(0xE0, b"JFIF\0")])),
            Err(HeaderError::Malformed(ImageFormat::Jpeg)),
            "a scan before any frame"
        );
        assert_eq!(
            read(&[0xFF, 0xD8, 0xFF, 0xD9]),
            Err(HeaderError::Malformed(ImageFormat::Jpeg)),
            "an image that ends before any frame"
        );
        let mut truncated = vec![0xFF, 0xD8];
        truncated.extend(&jpeg_frame(0xC0, 16, 9)[..6]);
        assert_eq!(
            read(&truncated),
            Err(HeaderError::Malformed(ImageFormat::Jpeg)),
            "a frame cut off before its width"
        );
        assert_eq!(
            read(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF]),
            Err(HeaderError::Malformed(ImageFormat::Jpeg)),
            "a segment claiming more bytes than the image holds"
        );
        assert_eq!(
            read(&jpeg(&[jpeg_frame(0xC0, 16, 0)])),
            Err(HeaderError::Malformed(ImageFormat::Jpeg)),
            "a frame whose height is deferred to a later DNL segment"
        );
    }

    #[test]
    fn a_lossy_webp_is_measured_from_its_vp8_frame_header() {
        let mut frame = vec![0x50, 0x01, 0x00, 0x9D, 0x01, 0x2A];
        // Fourteen bits of each dimension; the top two bits are a scale.
        frame.extend((0xC000_u16 | 800).to_le_bytes());
        frame.extend(600_u16.to_le_bytes());
        assert_eq!(
            read(&webp(b"VP8 ", &frame)),
            header(ImageFormat::WebP, 800, 600)
        );
        frame[3] = 0;
        assert_eq!(
            read(&webp(b"VP8 ", &frame)),
            Err(HeaderError::Malformed(ImageFormat::WebP)),
            "a frame without its start code"
        );
    }

    #[test]
    fn a_lossless_webp_is_measured_from_its_vp8l_header() {
        // The smallest lossless WebP any encoder writes: one pixel.
        let one_pixel = b"RIFF\x1a\x00\x00\x00WEBPVP8L\x0d\x00\x00\x00\x2f\x00\x00\x00\x10\x07\x10\x11\x11\x88\x88\xfe\x07\x00";
        assert_eq!(read(one_pixel), header(ImageFormat::WebP, 1, 1));
        // Fourteen bits each of width and height, both less one.
        let packed = (400_u32 - 1) | ((300 - 1) << 14);
        let mut payload = vec![0x2F];
        payload.extend(packed.to_le_bytes());
        assert_eq!(
            read(&webp(b"VP8L", &payload)),
            header(ImageFormat::WebP, 400, 300)
        );
        payload[0] = 0x2E;
        assert_eq!(
            read(&webp(b"VP8L", &payload)),
            Err(HeaderError::Malformed(ImageFormat::WebP)),
            "a lossless stream without its signature"
        );
    }

    #[test]
    fn an_extended_webp_is_measured_from_its_vp8x_canvas() {
        let mut payload = vec![0x10, 0, 0, 0];
        // Twenty-four bits each of canvas width and height, both less one.
        payload.extend(&(100_000_u32 - 1).to_le_bytes()[..3]);
        payload.extend(&(2_u32 - 1).to_le_bytes()[..3]);
        assert_eq!(
            read(&webp(b"VP8X", &payload)),
            header(ImageFormat::WebP, 100_000, 2)
        );
    }

    #[test]
    fn a_webp_with_an_unknown_first_chunk_is_malformed() {
        assert_eq!(
            read(&webp(b"ALPH", &[0; 16])),
            Err(HeaderError::Malformed(ImageFormat::WebP))
        );
    }

    #[test]
    fn a_png_or_gif_measuring_no_pixels_is_malformed() {
        assert_eq!(
            read(&png(0, 480)),
            Err(HeaderError::Malformed(ImageFormat::Png))
        );
        assert_eq!(
            read(&gif(b"GIF89a", 320, 0)),
            Err(HeaderError::Malformed(ImageFormat::Gif))
        );
    }

    #[test]
    fn other_formats_are_refused_whatever_they_resemble() {
        for bytes in [
            &b""[..],
            b"BM\x3a\0\0\0\0\0\0\0\x36\0\0\0",
            b"II*\0\x08\0\0\0",
            b"%PDF-1.7\n",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            b"\0\0\0\x1cftypavif",
            b"RIFF\x24\0\0\0WAVEfmt ",
            b"\x89PNG\r\n",
            b"GIF90a\x01\0\x01\0",
            &[0xFF, 0xD8],
        ] {
            assert_eq!(
                read(bytes),
                Err(HeaderError::UnsupportedFormat),
                "{bytes:?} is none of the four formats"
            );
        }
    }
}
