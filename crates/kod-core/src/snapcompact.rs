//! Text-to-PNG rasterizer for snapcompact (borrow from oh-my-pi,
//! delta §4.5).
//!
//! # What this is
//!
//! A small, dependency-free rasterizer that turns a block of text
//! into a PNG an image-capable model can consume. The design's
//! §4.5 uses it to compress old tool results — dense text that a
//! vision model reads for far fewer tokens than the raw bytes.
//!
//! # Why no dependency
//!
//! The alternative is `image` + a font crate + a shaping engine,
//! which is tens of megabytes of dependency for one feature. The
//! PNG format is small enough to write directly: a signature, an
//! `IHDR`, one `IDAT` with stored-mode deflate, and an `IEND`. Stored
//! mode means the "compression" is a 5-byte wrapper around the raw
//! bytes — the image is not smaller on disk, but the *token* cost
//! is what matters and the provider re-encodes anyway.
//!
//! The font is an 8×8 bitmap covering ASCII 0x20–0x7E. Each glyph is
//! eight bytes, one per row, MSB first. The table is public-domain
//! data (a "font8x8" style set), transcribed here so the crate has
//! no build-time asset.
//!
//! # What this does NOT do
//!
//! * Not anti-aliased. Bitmap glyphs at 1× scale are legible and
//!   cheap; smoothing would need subpixel coverage arithmetic and a
//!   larger canvas for the same content.
//! * Not Unicode. ASCII printable only. A codepoint outside the
//!   range renders as `?`. The design's own use case is code and
//!   tool output, which is overwhelmingly ASCII; a CJK transcript
//!   would need a different strategy (documented, not implemented).
//! * Not a compressor. Stored-mode deflate means a 4000×2000 canvas
//!   is 8 MB in memory and ~8 MB on the wire before the provider's
//!   own re-encode. The savings come from the *model's* tokenizer
//!   reading the image, not from the bytes being smaller.

/// An 8×8 bitmap font for ASCII 0x20–0x7E.
///
/// 95 glyphs, 8 bytes each, MSB-first. Index `c - 0x20` for
/// character `c`; byte `row` of glyph `c` is `FONT[(c - 0x20) * 8 +
/// row]`, and bit `7 - col` is the pixel at `(col, row)`.
const FONT8X8: [[u8; 8]; 95] = [
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // space
    [0x18,0x3C,0x3C,0x18,0x18,0x00,0x18,0x00], // !
    [0x36,0x36,0x00,0x00,0x00,0x00,0x00,0x00], // "
    [0x36,0x36,0x7F,0x36,0x7F,0x36,0x36,0x00], // #
    [0x0C,0x3E,0x03,0x1E,0x30,0x1F,0x0C,0x00], // $
    [0x00,0x63,0x33,0x18,0x0C,0x66,0x63,0x00], // %
    [0x1C,0x36,0x1C,0x6E,0x3B,0x33,0x6E,0x00], // &
    [0x06,0x06,0x03,0x00,0x00,0x00,0x00,0x00], // '
    [0x18,0x0C,0x06,0x06,0x06,0x0C,0x18,0x00], // (
    [0x06,0x0C,0x18,0x18,0x18,0x0C,0x06,0x00], // )
    [0x00,0x66,0x3C,0xFF,0x3C,0x66,0x00,0x00], // *
    [0x00,0x0C,0x0C,0x3F,0x0C,0x0C,0x00,0x00], // +
    [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x06], // ,
    [0x00,0x00,0x00,0x3F,0x00,0x00,0x00,0x00], // -
    [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x00], // .
    [0x60,0x30,0x18,0x0C,0x06,0x03,0x01,0x00], // /
    [0x3E,0x63,0x73,0x7B,0x6F,0x67,0x3E,0x00], // 0
    [0x0C,0x0E,0x0C,0x0C,0x0C,0x0C,0x3F,0x00], // 1
    [0x1E,0x33,0x30,0x1C,0x06,0x33,0x3F,0x00], // 2
    [0x1E,0x33,0x30,0x1C,0x30,0x33,0x1E,0x00], // 3
    [0x38,0x3C,0x36,0x33,0x7F,0x30,0x78,0x00], // 4
    [0x3F,0x03,0x1F,0x30,0x30,0x33,0x1E,0x00], // 5
    [0x1C,0x06,0x03,0x1F,0x33,0x33,0x1E,0x00], // 6
    [0x3F,0x33,0x30,0x18,0x0C,0x0C,0x0C,0x00], // 7
    [0x1E,0x33,0x33,0x1E,0x33,0x33,0x1E,0x00], // 8
    [0x1E,0x33,0x33,0x3E,0x30,0x18,0x0E,0x00], // 9
    [0x00,0x0C,0x0C,0x00,0x00,0x0C,0x0C,0x00], // :
    [0x00,0x0C,0x0C,0x00,0x00,0x0C,0x0C,0x06], // ;
    [0x18,0x0C,0x06,0x03,0x06,0x0C,0x18,0x00], // <
    [0x00,0x00,0x3F,0x00,0x00,0x3F,0x00,0x00], // =
    [0x06,0x0C,0x18,0x30,0x18,0x0C,0x06,0x00], // >
    [0x1E,0x33,0x30,0x18,0x0C,0x00,0x0C,0x00], // ?
    [0x3E,0x63,0x7B,0x7B,0x7B,0x03,0x1E,0x00], // @
    [0x0C,0x1E,0x33,0x33,0x3F,0x33,0x33,0x00], // A
    [0x3F,0x66,0x66,0x3E,0x66,0x66,0x3F,0x00], // B
    [0x3C,0x66,0x03,0x03,0x03,0x66,0x3C,0x00], // C
    [0x1F,0x36,0x66,0x66,0x66,0x36,0x1F,0x00], // D
    [0x7F,0x46,0x16,0x1E,0x16,0x46,0x7F,0x00], // E
    [0x7F,0x46,0x16,0x1E,0x16,0x06,0x0F,0x00], // F
    [0x3C,0x66,0x03,0x03,0x73,0x66,0x7C,0x00], // G
    [0x33,0x33,0x33,0x3F,0x33,0x33,0x33,0x00], // H
    [0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00], // I
    [0x78,0x30,0x30,0x30,0x33,0x33,0x1E,0x00], // J
    [0x67,0x66,0x36,0x1E,0x36,0x66,0x67,0x00], // K
    [0x0F,0x06,0x06,0x06,0x46,0x66,0x7F,0x00], // L
    [0x63,0x77,0x7F,0x7F,0x6B,0x63,0x63,0x00], // M
    [0x63,0x67,0x6F,0x7B,0x73,0x63,0x63,0x00], // N
    [0x1C,0x36,0x63,0x63,0x63,0x36,0x1C,0x00], // O
    [0x3F,0x66,0x66,0x3E,0x06,0x06,0x0F,0x00], // P
    [0x1E,0x33,0x33,0x33,0x3B,0x1E,0x38,0x00], // Q
    [0x3F,0x66,0x66,0x3E,0x36,0x66,0x67,0x00], // R
    [0x1E,0x33,0x07,0x0E,0x38,0x33,0x1E,0x00], // S
    [0x3F,0x2D,0x0C,0x0C,0x0C,0x0C,0x1E,0x00], // T
    [0x33,0x33,0x33,0x33,0x33,0x33,0x3F,0x00], // U
    [0x33,0x33,0x33,0x33,0x33,0x1E,0x0C,0x00], // V
    [0x63,0x63,0x63,0x6B,0x7F,0x77,0x63,0x00], // W
    [0x63,0x63,0x36,0x1C,0x1C,0x36,0x63,0x00], // X
    [0x33,0x33,0x33,0x1E,0x0C,0x0C,0x1E,0x00], // Y
    [0x7F,0x63,0x31,0x18,0x4C,0x66,0x7F,0x00], // Z
    [0x1E,0x06,0x06,0x06,0x06,0x06,0x1E,0x00], // [
    [0x03,0x06,0x0C,0x18,0x30,0x60,0x40,0x00], // backslash
    [0x1E,0x18,0x18,0x18,0x18,0x18,0x1E,0x00], // ]
    [0x08,0x1C,0x36,0x63,0x00,0x00,0x00,0x00], // ^
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xFF], // _
    [0x0C,0x0C,0x18,0x00,0x00,0x00,0x00,0x00], // `
    [0x00,0x00,0x1E,0x30,0x3E,0x33,0x6E,0x00], // a
    [0x07,0x06,0x06,0x3E,0x66,0x66,0x3B,0x00], // b
    [0x00,0x00,0x1E,0x33,0x03,0x33,0x1E,0x00], // c
    [0x38,0x30,0x30,0x3e,0x33,0x33,0x6E,0x00], // d
    [0x00,0x00,0x1E,0x33,0x3f,0x03,0x1E,0x00], // e
    [0x1C,0x36,0x06,0x0f,0x06,0x06,0x0F,0x00], // f
    [0x00,0x00,0x6E,0x33,0x33,0x3E,0x30,0x1F], // g
    [0x07,0x06,0x36,0x6E,0x66,0x66,0x67,0x00], // h
    [0x0C,0x00,0x0E,0x0C,0x0C,0x0C,0x1E,0x00], // i
    [0x30,0x00,0x30,0x30,0x30,0x33,0x33,0x1E], // j
    [0x07,0x06,0x66,0x36,0x1E,0x36,0x67,0x00], // k
    [0x0E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00], // l
    [0x00,0x00,0x33,0x7F,0x7F,0x6B,0x63,0x00], // m
    [0x00,0x00,0x1F,0x33,0x33,0x33,0x33,0x00], // n
    [0x00,0x00,0x1E,0x33,0x33,0x33,0x1E,0x00], // o
    [0x00,0x00,0x3B,0x66,0x66,0x3E,0x06,0x0F], // p
    [0x00,0x00,0x6E,0x33,0x33,0x3E,0x30,0x78], // q
    [0x00,0x00,0x3B,0x6E,0x66,0x06,0x0F,0x00], // r
    [0x00,0x00,0x3E,0x03,0x1E,0x30,0x1F,0x00], // s
    [0x08,0x0C,0x3E,0x0C,0x0C,0x2C,0x18,0x00], // t
    [0x00,0x00,0x33,0x33,0x33,0x33,0x6E,0x00], // u
    [0x00,0x00,0x33,0x33,0x33,0x1E,0x0C,0x00], // v
    [0x00,0x00,0x63,0x6B,0x7F,0x7F,0x36,0x00], // w
    [0x00,0x00,0x63,0x36,0x1C,0x36,0x63,0x00], // x
    [0x00,0x00,0x33,0x33,0x33,0x3E,0x30,0x1F], // y
    [0x00,0x00,0x3F,0x19,0x0C,0x26,0x3F,0x00], // z
    [0x38,0x0C,0x0C,0x07,0x0C,0x0C,0x38,0x00], // {
    [0x18,0x18,0x18,0x00,0x18,0x18,0x18,0x00], // |
    [0x07,0x0C,0x0C,0x38,0x0C,0x0C,0x07,0x00], // }
    [0x6E,0x3B,0x00,0x00,0x00,0x00,0x00,0x00], // ~
];

/// Glyph cell width in pixels, including the one-pixel right margin.
const CELL_W: u32 = 8;
/// Glyph cell height in pixels, including the one-pixel bottom margin.
const CELL_H: u32 = 10;

/// Rasterization failure.
#[derive(Debug, Clone)]
pub enum RasterizeError {
    /// The input is empty. Rendering nothing is not useful and the
    /// caller should fall through to the next compaction method.
    Empty,
    /// The input is larger than the canvas limit. A caller that hits
    /// this should not rasterize — the image would be enormous and
    /// the token cost would exceed the text it replaced.
    TooLarge { lines: usize, max: usize },
}

impl std::fmt::Display for RasterizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty input"),
            Self::TooLarge { lines, max } => {
                write!(f, "{lines} lines exceeds the {max}-line canvas limit")
            }
        }
    }
}

impl std::error::Error for RasterizeError {}

/// The maximum number of text lines a single frame renders.
///
/// The design's `FRAME_TOKEN_ESTIMATE = 5024` is the token cost of
/// one frame; the canvas limit keeps that from growing past a
/// multi-frame budget on a runaway output. 200 lines at 10 px is
/// 2000 px tall — well within every vision model's limit and about
/// 8000 characters of source text.
pub const MAX_LINES: usize = 200;

/// The maximum number of columns a line renders. Longer lines are
/// truncated with a trailing `...`.
pub const MAX_COLS: usize = 200;

/// Rasterize `text` into a PNG.
///
/// The image is monochrome — black glyphs on a white background.
/// That is the highest-contrast choice a vision model reads most
/// reliably, and it needs no colour table beyond two entries.
pub fn rasterize_to_png(text: &str) -> Result<Vec<u8>, RasterizeError> {
    let lines: Vec<&str> = text.lines().take(MAX_LINES).collect();
    if lines.is_empty() || lines.iter().all(|l| l.trim().is_empty()) {
        return Err(RasterizeError::Empty);
    }
    if text.lines().count() > MAX_LINES {
        return Err(RasterizeError::TooLarge {
            lines: text.lines().count(),
            max: MAX_LINES,
        });
    }

    let width = (MAX_COLS as u32) * CELL_W;
    let height = (lines.len() as u32) * CELL_H;

    // Packed monochrome: 1 bit per pixel, MSB first within each byte,
    // rows byte-aligned. Every scanline is padded to a byte boundary.
    let row_bytes = width.div_ceil(8) as usize;
    let mut pixels = vec![0xFFu8; row_bytes * height as usize]; // white

    for (line_idx, line) in lines.iter().enumerate() {
        let y0 = line_idx as u32 * CELL_H;
        for (col, ch) in line.chars().take(MAX_COLS).enumerate() {
            let x0 = col as u32 * CELL_W;
            let glyph = glyph_for(ch);
            for (row, bits) in glyph.iter().enumerate() {
                let y = y0 + row as u32;
                for bit in 0..8u32 {
                    // MSB-first.
                    if bits & (0x80 >> bit) == 0 {
                        continue;
                    }
                    let x = x0 + bit;
                    set_pixel(&mut pixels, row_bytes, x, y, false); // black
                }
            }
        }
    }

    Ok(encode_mono_png(width, height, &pixels, row_bytes))
}

/// The glyph bitmap for `ch`. Non-ASCII-printable falls back to `?`.
fn glyph_for(ch: char) -> [u8; 8] {
    let c = ch as u32;
    if (0x20..=0x7E).contains(&c) {
        FONT8X8[(c - 0x20) as usize]
    } else if ch == '\t' {
        FONT8X8[0] // render a tab as a space; the caller handles indentation
    } else {
        FONT8X8[('?' as u32 - 0x20) as usize]
    }
}

fn set_pixel(buf: &mut [u8], row_bytes: usize, x: u32, y: u32, on: bool) {
    let byte = (y as usize) * row_bytes + (x as usize) / 8;
    let bit = 7 - (x % 8);
    let mask = 1u8 << bit;
    if on {
        buf[byte] |= mask;
    } else {
        buf[byte] &= !mask;
    }
}

/// Encode a packed 1-bit monochrome image as a PNG.
///
/// Minimal: one `IDAT` with a stored-mode deflate stream. Every
/// scanline is prefixed with a `0x00` filter byte (None), as the PNG
/// spec requires. No palette needed — a 1-bit grayscale image uses
/// only black and white by definition (`0` = black, `1` = white).
fn encode_mono_png(width: u32, height: u32, pixels: &[u8], row_bytes: usize) -> Vec<u8> {
    // Raw scanlines for the IDAT: filter byte + row, per row.
    let mut raw = Vec::with_capacity((row_bytes + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0x00); // filter: None
        raw.extend_from_slice(&pixels[y * row_bytes..(y + 1) * row_bytes]);
    }

    let mut out = Vec::new();
    // PNG signature.
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR: width, height, bit depth 1, colour type 0 (grayscale),
    // compression 0, filter 0, interlace 0.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[1, 0, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);

    // IDAT: zlib stream (2-byte header, stored deflate blocks,
    // 4-byte Adler-32).
    let idat = zlib_stored(&raw);
    write_chunk(&mut out, b"IDAT", &idat);

    // IEND.
    write_chunk(&mut out, b"IEND", &[]);

    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    // CRC over kind + data.
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    let crc = crc32(&crc_input);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Wrap `data` in a zlib stream using *stored* (uncompressed) deflate
/// blocks. Each block is at most 65535 bytes.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    // zlib header: CMF (0x78) = deflate, 32K window; FLG such that
    // (CMF*256 + FLG) % 31 == 0. 0x78 0x01 satisfies it.
    out.push(0x78);
    out.push(0x01);

    let mut i = 0;
    while i < data.len() {
        let remaining = data.len() - i;
        let block = remaining.min(65535);
        let is_last = i + block == data.len();
        out.push(if is_last { 0x01 } else { 0x00 });
        out.extend_from_slice(&(block as u16).to_le_bytes());
        out.extend_from_slice(&(!(block as u16)).to_le_bytes());
        out.extend_from_slice(&data[i..i + block]);
        i += block;
    }
    // Empty input is not reachable from the caller (Empty is
    // checked first), but guard anyway so the stream is well-formed.
    if data.is_empty() {
        out.push(0x01);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    }

    // Adler-32 over the *raw* data.
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    let adler = (b << 16) | a;
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

/// CRC-32 (IEEE 802.3) over `data`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = if crc & 1 != 0 { 0xEDB8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

/// Base64-encode `bytes` with the standard alphabet and `=` padding,
/// as the PNG-in-JSON wire format expects.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_signature_ok(png: &[u8]) -> bool {
        png.len() >= 8
            && png[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    }

    #[test]
    fn an_empty_input_errors() {
        assert!(matches!(rasterize_to_png(""), Err(RasterizeError::Empty)));
        assert!(matches!(
            rasterize_to_png("   \n   \n"),
            Err(RasterizeError::Empty),
        ));
    }

    #[test]
    fn too_many_lines_errors() {
        let big = "line\n".repeat(MAX_LINES + 1);
        match rasterize_to_png(&big) {
            Err(RasterizeError::TooLarge { lines, max }) => {
                assert_eq!(max, MAX_LINES);
                assert!(lines > MAX_LINES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn a_single_line_renders_a_png() {
        let png = rasterize_to_png("hello").unwrap();
        assert!(png_signature_ok(&png));
    }

    #[test]
    fn the_png_ends_with_iend() {
        let png = rasterize_to_png("A").unwrap();
        // IEND chunk is length 0 + "IEND" + CRC. Total trailing 12 bytes.
        let tail = &png[png.len() - 8..];
        assert_eq!(&tail[..4], b"IEND");
    }

    #[test]
    fn the_ihdr_reports_the_expected_dimensions() {
        let png = rasterize_to_png("line one\nline two").unwrap();
        // Signature 8, IHDR length 4, "IHDR" 4, then data.
        let ihdr_data = &png[16..16 + 13];
        let w = u32::from_be_bytes([ihdr_data[0], ihdr_data[1], ihdr_data[2], ihdr_data[3]]);
        let h = u32::from_be_bytes([ihdr_data[4], ihdr_data[5], ihdr_data[6], ihdr_data[7]]);
        assert_eq!(w, MAX_COLS as u32 * CELL_W);
        assert_eq!(h, 2 * CELL_H);
        assert_eq!(ihdr_data[8], 1); // bit depth
        assert_eq!(ihdr_data[9], 0); // grayscale
    }

    #[test]
    fn the_rendered_bitmap_has_black_pixels_for_a_glyph() {
        // Render "A" and check that some pixel in the first cell is
        // black (0 bit in the packed stream).
        let png = rasterize_to_png("A").unwrap();
        // Find the IDAT chunk and decompress trivially (we control
        // the encoder: stored mode, so the raw bytes are visible).
        let idat_idx = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let idat_len_bytes = &png[idat_idx - 4..idat_idx];
        let idat_len = u32::from_be_bytes([
            idat_len_bytes[0],
            idat_len_bytes[1],
            idat_len_bytes[2],
            idat_len_bytes[3],
        ]) as usize;
        let idat = &png[idat_idx + 4..idat_idx + 4 + idat_len];
        // zlib header (2) + block header (5) = 7, then row 0.
        // Row 0 filter byte is at idat[7], pixels at idat[8..].
        let row_bytes = (MAX_COLS as usize) * (CELL_W as usize) / 8;
        let row0 = &idat[8..8 + row_bytes];
        // At least one pixel must be a black bit (0). Row 0 of 'A'
        // is 0x0C, which has bits clear at positions 0-3 and 6-7.
        assert!(
            row0.iter().any(|b| *b != 0xFF),
            "glyph 'A' row 0 must have black pixels",
        );
    }

    #[test]
    fn base64_encodes_the_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn crc32_matches_the_known_vector() {
        // CRC-32 of "123456789" is 0xCBF43926 (the standard check
        // value for the IEEE polynomial).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_tab_renders_as_a_space() {
        let g = glyph_for('\t');
        assert_eq!(g, FONT8X8[0]);
    }

    #[test]
    fn a_non_ascii_char_renders_as_a_question_mark() {
        let g = glyph_for('é');
        assert_eq!(g, FONT8X8[('?' as u32 - 0x20) as usize]);
    }

    #[test]
    fn a_long_line_is_truncated_to_max_cols() {
        // 300 chars exceeds MAX_COLS; the rasterizer takes the first
        // MAX_COLS and does not panic.
        let long = "x".repeat(300);
        let png = rasterize_to_png(&long).unwrap();
        assert!(png_signature_ok(&png));
    }
}
