//! Minimal PNG encoder (RGBA8 -> PNG bytes), zero dependencies.
//!
//! Each scanline uses filter type 0 (None) and the zlib stream is made of
//! stored (uncompressed) DEFLATE blocks, so no compressor is needed. The
//! output is a valid PNG any decoder can read; it is simply larger than a
//! compressed one. Screenshots are the main use; a future improvement is a
//! real DEFLATE compressor for smaller frames.
//!
//! Performance notes (the output bytes are identical to the naive version
//! — see the `encoded_bytes_are_stable` test, which pins the exact output
//! for a small image): the encoder used to build two extra full-size
//! buffers per frame (a `raw` scanline buffer and a second copy just to
//! CRC it) and ran a division per byte for Adler-32. It now streams rows
//! straight into the zlib stream, CRCs incrementally, and defers the
//! Adler modulo to 5552-byte blocks. Same bytes out, roughly half the
//! memory traffic and no per-byte divisions.

use std::sync::OnceLock;

/// CRC-32 (IEEE, polynomial 0xEDB88320), as PNG requires.
fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            table[i as usize] = c;
        }
        table
    })
}

/// Streaming CRC-32: same result as `crc32` over the concatenated input.
struct Crc {
    state: u32,
}

impl Crc {
    fn new() -> Crc {
        Crc { state: 0xFFFF_FFFF }
    }

    fn update(&mut self, data: &[u8]) {
        let table = crc_table();
        let mut crc = self.state;
        for &b in data {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        self.state = crc;
    }

    fn finish(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.update(data);
    c.finish()
}

/// Adler-32, as the zlib wrapper requires. The modulo is deferred to
/// 5552-byte blocks (the largest block for which the accumulators cannot
/// overflow u32) instead of dividing on every byte.
struct Adler {
    a: u64,
    b: u64,
    since_mod: usize,
}

impl Adler {
    fn new() -> Adler {
        Adler {
            a: 1,
            b: 0,
            since_mod: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.a += byte as u64;
            self.b += self.a;
            self.since_mod += 1;
            if self.since_mod == 5552 {
                self.a %= 65521;
                self.b %= 65521;
                self.since_mod = 0;
            }
        }
    }

    fn finish(mut self) -> u32 {
        self.a %= 65521;
        self.b %= 65521;
        ((self.b << 16) | self.a) as u32
    }
}

pub fn adler32(data: &[u8]) -> u32 {
    let mut a = Adler::new();
    a.update(data);
    a.finish()
}

fn chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(name);
    crc_data.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

/// Encode RGBA8 pixel data (`width * height * 4` bytes) as a PNG.
/// Byte-identical to the previous implementation (see the golden test).
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(format!(
            "rgba length {} does not match {}x{} (expected {})",
            rgba.len(),
            width,
            height,
            expected
        ));
    }
    if width == 0 || height == 0 {
        return Err("zero-size image".to_string());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]); // PNG signature
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type 6 = RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    chunk(&mut out, b"IHDR", &ihdr);

    // zlib stream: header 0x78 0x01 (no/low compression) + stored blocks.
    // Rows stream straight in (one filter byte 0 per row) instead of
    // building a separate `raw` buffer first; block headers are emitted
    // at exact 65535-byte boundaries of the raw stream, which is what the
    // old code's chunking produced. Adler-32 and the IDAT CRC accumulate
    // incrementally over the same bytes.
    let stride = width as usize * 4;
    let raw_len = (stride + 1) * height as usize;
    let mut z: Vec<u8> = Vec::with_capacity(raw_len + raw_len / 65535 * 5 + 16);
    z.extend_from_slice(&[0x78, 0x01]);
    let mut adler = Adler::new();
    let mut crc = Crc::new();
    crc.update(b"IDAT");
    crc.update(&[0x78, 0x01]);
    // Append `bytes` to the zlib stream, splitting stored blocks at
    // 65535-byte raw boundaries. `raw_pos` is the position in the raw
    // stream before this piece.
    let mut raw_pos = 0usize;
    let append_raw = |z: &mut Vec<u8>, crc: &mut Crc, adler: &mut Adler, raw_pos: &mut usize, mut piece: &[u8]| {
        while !piece.is_empty() {
            // Start a new stored block at every 65535-byte boundary.
            if *raw_pos % 65535 == 0 {
                let remaining = raw_len - *raw_pos;
                let block_len = remaining.min(65535);
                let is_final = *raw_pos + block_len >= raw_len;
                let header = [
                    if is_final { 1u8 } else { 0u8 },
                    (block_len & 0xFF) as u8,
                    ((block_len >> 8) & 0xFF) as u8,
                    (!(block_len as u16)).to_le_bytes()[0],
                    (!(block_len as u16)).to_le_bytes()[1],
                ];
                z.extend_from_slice(&header);
                crc.update(&header);
            }
            let in_block = 65535 - (*raw_pos % 65535);
            let take = piece.len().min(in_block);
            z.extend_from_slice(&piece[..take]);
            crc.update(&piece[..take]);
            adler.update(&piece[..take]);
            *raw_pos += take;
            piece = &piece[take..];
        }
    };
    if raw_len == 0 {
        // Unreachable given the dimension checks above, but kept so an
        // empty frame still yields one final empty stored block.
        let header = [1u8, 0, 0, 0xFF, 0xFF];
        z.extend_from_slice(&header);
        crc.update(&header);
    } else {
        for y in 0..height as usize {
            append_raw(&mut z, &mut crc, &mut adler, &mut raw_pos, &[0u8]);
            append_raw(
                &mut z,
                &mut crc,
                &mut adler,
                &mut raw_pos,
                &rgba[y * stride..(y + 1) * stride],
            );
        }
    }
    let adler_value = adler.finish();
    z.extend_from_slice(&adler_value.to_be_bytes());
    crc.update(&adler_value.to_be_bytes());
    // IDAT chunk: length + name + stream + CRC (computed incrementally,
    // so no second copy of the stream is built just to CRC it).
    out.extend_from_slice(&(z.len() as u32).to_be_bytes());
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&z);
    out.extend_from_slice(&crc.finish().to_be_bytes());
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_known_vector() {
        // Value from the Adler-32 reference example for "Wikipedia".
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn adler32_matches_naive_on_large_input() {
        // Deferred-modulo Adler must equal the per-byte-modulo version,
        // including across several 5552-byte block boundaries.
        let data: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for &byte in &data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        let naive = (b << 16) | a;
        assert_eq!(adler32(&data), naive);
    }

    #[test]
    fn png_structure_is_well_formed() {
        // 2x1: opaque red, opaque green.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255];
        let png = encode_rgba(2, 1, &rgba).unwrap();
        assert_eq!(&png[0..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        // First chunk must be IHDR, 13 bytes of data, width 2 height 1, RGBA.
        assert_eq!(&png[8..12], &13u32.to_be_bytes()); // IHDR data length
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &2u32.to_be_bytes());
        assert_eq!(&png[20..24], &1u32.to_be_bytes());
        assert_eq!(png[24], 8);
        assert_eq!(png[25], 6);
        // Last chunk must be IEND.
        assert_eq!(&png[png.len() - 12..png.len() - 8], &0u32.to_be_bytes());
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn png_rejects_bad_dimensions() {
        assert!(encode_rgba(0, 1, &[]).is_err());
        assert!(encode_rgba(1, 1, &[1, 2, 3]).is_err()); // need 4 bytes
    }

    #[test]
    fn encoded_bytes_are_stable() {
        // Golden bytes produced by the pre-optimization encoder for this
        // exact 3x2 input. The streaming rewrite must not change one byte
        // of output: same PNG, just less copying to produce it.
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 10, 20, 30, 255,
            200, 150, 100, 255, 0, 0, 0, 0,
        ];
        let expected: Vec<u8> = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0,
            0, 0, 3, 0, 0, 0, 2, 8, 6, 0, 0, 0, 157, 116, 102, 26, 0, 0, 0,
            37, 73, 68, 65, 84, 120, 1, 1, 26, 0, 229, 255, 0, 255, 0, 0, 255,
            0, 255, 0, 255, 0, 0, 255, 255, 0, 10, 20, 30, 255, 200, 150,
            100, 255, 0, 0, 0, 0, 142, 204, 9, 247, 202, 122, 109, 159, 0, 0,
            0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ];
        assert_eq!(encode_rgba(3, 2, &rgba).unwrap(), expected);
    }
}
