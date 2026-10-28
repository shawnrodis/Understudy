//! Baseline JPEG encoder (RGB -> JFIF bytes) and a box-average
//! downscaler, zero dependencies.
//!
//! Purpose: the local-operator loop needs small frames. The PNG encoder
//! emits stored (uncompressed) blocks, so a full screenshot is ~14.7 MB;
//! a 1280-wide JPEG at quality 80 of the same frame is typically
//! ~100-200 KB. The encoder here uses the standard Annex K quantization
//! and Huffman tables, 4:2:0 chroma subsampling, and a separable float
//! DCT — good enough for a model to read a screen, not a photography
//! codec. Output is a valid baseline JFIF any decoder can read (verified
//! against a real decoder in development, not just by construction).
//!
//! Alpha is ignored on downscale: engine screenshots are opaque.

// Standard quantization tables, in row-major order (DQT stores them in
// zigzag order; the encoder reorders on write).
const QUANT_LUMA: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13,
    16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56,
    68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103,
    121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

const QUANT_CHROMA: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, //
    18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, //
    47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99,
];

// Zigzag scan order: zigzag[i] = row-major index of the i-th coefficient.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33,
    40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43,
    36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53,
    60, 61, 54, 47, 55, 62, 63,
];

// Standard Huffman tables: (counts per code length 1..=16, symbols).
const DC_LUMA_COUNTS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA_COUNTS: [u8; 16] =
    [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMA_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_COUNTS: [u8; 16] =
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_SYMBOLS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
    0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08,
    0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72,
    0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
    0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
    0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3,
    0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
    0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9,
    0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4,
    0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const AC_CHROMA_COUNTS: [u8; 16] =
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_SYMBOLS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41,
    0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91,
    0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1,
    0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
    0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58,
    0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74,
    0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a,
    0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
    0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
    0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4,
    0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

/// Canonical Huffman codes for one table: symbol -> (code, length).
struct HuffTable {
    codes: [u32; 256],
    lens: [u8; 256],
}

fn build_huff(counts: &[u8; 16], symbols: &[u8]) -> HuffTable {
    let mut codes = [0u32; 256];
    let mut lens = [0u8; 256];
    let mut code: u32 = 0;
    let mut k = 0usize;
    for len in 1..=16usize {
        for _ in 0..counts[len - 1] {
            if k >= symbols.len() {
                break;
            }
            let sym = symbols[k];
            codes[sym as usize] = code;
            lens[sym as usize] = len as u8;
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    HuffTable { codes, lens }
}

/// MSB-first bit writer with 0xFF stuffing (entropy data only).
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    fn write(&mut self, code: u32, len: u8) {
        self.acc = (self.acc << len) | (code & ((1u32 << len) - 1));
        self.nbits += len as u32;
        while self.nbits >= 8 {
            let byte = (self.acc >> (self.nbits - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
            self.nbits -= 8;
        }
        // Keep the accumulator small.
        self.acc &= (1u32 << self.nbits) - 1;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            // Pad with 1 bits to a byte boundary.
            let pad = 8 - self.nbits;
            let byte = ((self.acc << pad) | ((1u32 << pad) - 1)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
        }
        self.out
    }
}

fn scale_quant(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = quality.clamp(1, 100) as u32;
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    for i in 0..64 {
        let v = (base[i] as u32 * scale + 50) / 100;
        out[i] = v.clamp(1, 255) as u16;
    }
    out
}

fn cos_table() -> [[f32; 8]; 8] {
    let mut t = [[0f32; 8]; 8];
    for u in 0..8 {
        for x in 0..8 {
            t[u][x] = (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
        }
    }
    t
}

/// Separable forward DCT on an 8x8 block of level-shifted samples.
fn dct_block(input: &[f32; 64], cos: &[[f32; 8]; 8]) -> [f32; 64] {
    let mut tmp = [0f32; 64];
    for i in 0..8 {
        for v in 0..8 {
            let mut s = 0f32;
            for y in 0..8 {
                s += input[i * 8 + y] * cos[v][y];
            }
            tmp[i * 8 + v] = s;
        }
    }
    let mut out = [0f32; 64];
    for u in 0..8 {
        let cu = if u == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
        for v in 0..8 {
            let cv = if v == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
            let mut s = 0f32;
            for i in 0..8 {
                s += tmp[i * 8 + v] * cos[u][i];
            }
            out[u * 8 + v] = 0.25 * cu * cv * s;
        }
    }
    out
}

/// Quantize a DCT block. AC coefficients are clamped to [-1023, 1023]:
/// the standard AC Huffman tables only carry sizes up to 10, so an
/// unclamped extreme block at very high quality could otherwise emit a
/// symbol with no code. The clamp costs nothing visible (at most one
/// least-significant step on pathological blocks) and keeps the stream
/// valid for every quality.
fn quantize(dct: &[f32; 64], q: &[u16; 64]) -> [i32; 64] {
    let mut coeffs = [0i32; 64];
    for k in 0..64 {
        coeffs[k] = (dct[k] / q[k] as f32).round() as i32;
    }
    for k in 1..64 {
        coeffs[k] = coeffs[k].clamp(-1023, 1023);
    }
    coeffs
}

fn magnitude_category(v: i32) -> u8 {
    let a = v.unsigned_abs();
    if a == 0 {
        0
    } else {
        32 - a.leading_zeros() as u8
    }
}

/// Append the value bits for a coefficient of the given category.
/// Negative values use the JPEG complement encoding.
fn write_value_bits(w: &mut BitWriter, v: i32, size: u8) {
    if size == 0 {
        return;
    }
    let bits = if v < 0 {
        (v + ((1 << size) - 1)) as u32
    } else {
        v as u32
    };
    w.write(bits, size);
}

fn encode_block(
    w: &mut BitWriter,
    coeffs: &[i32; 64],
    dc_table: &HuffTable,
    ac_table: &HuffTable,
    prev_dc: &mut i32,
) {
    // DC: difference from the previous block of this component.
    let diff = coeffs[0] - *prev_dc;
    *prev_dc = coeffs[0];
    let size = magnitude_category(diff);
    w.write(
        dc_table.codes[size as usize],
        dc_table.lens[size as usize],
    );
    write_value_bits(w, diff, size);
    // AC: zigzag order, run-length coded.
    let mut run = 0u8;
    for i in 1..64 {
        let v = coeffs[ZIGZAG[i]];
        if v == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            let sym = 0xF0u8; // ZRL: 16 zeroes.
            w.write(ac_table.codes[sym as usize], ac_table.lens[sym as usize]);
            run -= 16;
        }
        let size = magnitude_category(v);
        let sym = (run << 4) | size;
        w.write(ac_table.codes[sym as usize], ac_table.lens[sym as usize]);
        write_value_bits(w, v, size);
        run = 0;
    }
    if run > 0 {
        let sym = 0x00u8; // EOB.
        w.write(ac_table.codes[sym as usize], ac_table.lens[sym as usize]);
    }
}

/// Box-average downscale of RGBA to `target_w` wide (aspect kept,
/// alpha preserved). If `target_w` >= width, returns a copy of the
/// original pixels. Used by the PNG downscale path; the JPEG path uses
/// `downscale_rgb` (alpha dropped, screenshots are opaque).
pub fn downscale_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    target_w: u32,
) -> (u32, u32, Vec<u8>) {
    if target_w == 0 || target_w >= width {
        return (width, height, rgba.to_vec());
    }
    let tw = target_w;
    let th = ((height as u64 * tw as u64) / width as u64).max(1) as u32;
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for oy in 0..th {
        let y0 = (oy as u64 * height as u64 / th as u64) as usize;
        let y1 = (((oy + 1) as u64 * height as u64 / th as u64).max(y0 as u64 + 1)) as usize;
        for ox in 0..tw {
            let x0 = (ox as u64 * width as u64 / tw as u64) as usize;
            let x1 = (((ox + 1) as u64 * width as u64 / tw as u64).max(x0 as u64 + 1)) as usize;
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            let mut sa = 0u64;
            let mut n = 0u64;
            for sy in y0..y1.min(height as usize) {
                for sx in x0..x1.min(width as usize) {
                    let p = (sy * width as usize + sx) * 4;
                    sr += rgba[p] as u64;
                    sg += rgba[p + 1] as u64;
                    sb += rgba[p + 2] as u64;
                    sa += rgba[p + 3] as u64;
                    n += 1;
                }
            }
            if n == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                out.extend_from_slice(&[
                    (sr / n) as u8,
                    (sg / n) as u8,
                    (sb / n) as u8,
                    (sa / n) as u8,
                ]);
            }
        }
    }
    (tw, th, out)
}

/// Box-average downscale of RGBA to `target_w` wide (aspect kept, alpha
/// dropped). If `target_w` >= width, returns the original dimensions.
pub fn downscale_rgb(
    rgba: &[u8],
    width: u32,
    height: u32,
    target_w: u32,
) -> (u32, u32, Vec<u8>) {
    if target_w == 0 || target_w >= width {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for px in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[0..3]);
        }
        return (width, height, rgb);
    }
    let tw = target_w;
    let th = ((height as u64 * tw as u64) / width as u64).max(1) as u32;
    let mut rgb = Vec::with_capacity((tw * th * 3) as usize);
    for oy in 0..th {
        let y0 = (oy as u64 * height as u64 / th as u64) as usize;
        let y1 = (((oy + 1) as u64 * height as u64 / th as u64).max(y0 as u64 + 1)) as usize;
        for ox in 0..tw {
            let x0 = (ox as u64 * width as u64 / tw as u64) as usize;
            let x1 = (((ox + 1) as u64 * width as u64 / tw as u64).max(x0 as u64 + 1)) as usize;
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            let mut n = 0u64;
            for sy in y0..y1.min(height as usize) {
                for sx in x0..x1.min(width as usize) {
                    let p = (sy * width as usize + sx) * 4;
                    sr += rgba[p] as u64;
                    sg += rgba[p + 1] as u64;
                    sb += rgba[p + 2] as u64;
                    n += 1;
                }
            }
            if n == 0 {
                rgb.extend_from_slice(&[0, 0, 0]);
            } else {
                rgb.extend_from_slice(&[
                    (sr / n) as u8,
                    (sg / n) as u8,
                    (sb / n) as u8,
                ]);
            }
        }
    }
    (tw, th, rgb)
}

/// Encode RGB (`width * height * 3` bytes) as baseline JFIF JPEG.
/// `quality` 1..=100 (80 is the operator default).
pub fn encode_jpeg_rgb(
    width: u32,
    height: u32,
    rgb: &[u8],
    quality: u8,
) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * 3;
    if rgb.len() != expected {
        return Err(format!(
            "rgb length {} does not match {}x{} (expected {})",
            rgb.len(),
            width,
            height,
            expected
        ));
    }
    if width == 0 || height == 0 {
        return Err("zero-size image".to_string());
    }
    let ql = scale_quant(&QUANT_LUMA, quality);
    let qc = scale_quant(&QUANT_CHROMA, quality);
    let dc_luma = build_huff(&DC_LUMA_COUNTS, &DC_LUMA_SYMBOLS);
    let dc_chroma = build_huff(&DC_CHROMA_COUNTS, &DC_CHROMA_SYMBOLS);
    let ac_luma = build_huff(&AC_LUMA_COUNTS, &AC_LUMA_SYMBOLS);
    let ac_chroma = build_huff(&AC_CHROMA_COUNTS, &AC_CHROMA_SYMBOLS);
    let cos = cos_table();

    let mut out = Vec::new();
    // SOI.
    out.extend_from_slice(&[0xFF, 0xD8]);
    // APP0 (JFIF 1.1, no thumbnail).
    out.extend_from_slice(&[
        0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01,
        0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
    ]);
    // DQT: both tables in one segment, values in zigzag order.
    {
        let mut dqt = vec![0xFF, 0xDB, 0x00, 0x84, 0x00];
        for i in 0..64 {
            dqt.push(ql[ZIGZAG[i]] as u8);
        }
        dqt.push(0x01);
        for i in 0..64 {
            dqt.push(qc[ZIGZAG[i]] as u8);
        }
        out.extend_from_slice(&dqt);
    }
    // SOF0: 8-bit, 3 components, Y sampled 2x2, Cb/Cr 1x1.
    out.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x11, 0x08,
        (height >> 8) as u8,
        (height & 0xFF) as u8,
        (width >> 8) as u8,
        (width & 0xFF) as u8,
        0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
    ]);
    // DHT: four tables.
    let mut dht_body: Vec<u8> = Vec::new();
    for (tc_th, counts, symbols) in [
        (0x00u8, &DC_LUMA_COUNTS, DC_LUMA_SYMBOLS.as_slice()),
        (0x10u8, &AC_LUMA_COUNTS, AC_LUMA_SYMBOLS.as_slice()),
        (0x01u8, &DC_CHROMA_COUNTS, DC_CHROMA_SYMBOLS.as_slice()),
        (0x11u8, &AC_CHROMA_COUNTS, AC_CHROMA_SYMBOLS.as_slice()),
    ] {
        dht_body.push(tc_th);
        dht_body.extend_from_slice(counts);
        dht_body.extend_from_slice(symbols);
    }
    let dht_len = (dht_body.len() + 2) as u16;
    out.extend_from_slice(&[0xFF, 0xC4, (dht_len >> 8) as u8, (dht_len & 0xFF) as u8]);
    out.extend_from_slice(&dht_body);
    // SOS.
    out.extend_from_slice(&[
        0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11,
        0x00, 0x3F, 0x00,
    ]);

    // Pixel access with edge replication (pads to a multiple of 16).
    let px = |x: u32, y: u32| -> (f32, f32, f32) {
        let x = x.min(width - 1) as usize;
        let y = y.min(height - 1) as usize;
        let p = (y * width as usize + x) * 3;
        (
            rgb[p] as f32,
            rgb[p + 1] as f32,
            rgb[p + 2] as f32,
        )
    };

    let mut w = BitWriter::new();
    let mut prev_y = 0i32;
    let mut prev_cb = 0i32;
    let mut prev_cr = 0i32;
    let mcus_x = (width + 15) / 16;
    let mcus_y = (height + 15) / 16;
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            // 4 luma blocks + 1 Cb + 1 Cr per MCU (4:2:0).
            for by in 0..2 {
                for bx in 0..2 {
                    let mut block = [0f32; 64];
                    for i in 0..8 {
                        for j in 0..8 {
                            let (r, g, b) = px(mx * 16 + bx * 8 + j as u32, my * 16 + by * 8 + i as u32);
                            block[i * 8 + j] =
                                0.299 * r + 0.587 * g + 0.114 * b - 128.0;
                        }
                    }
                    let dct = dct_block(&block, &cos);
                    let coeffs = quantize(&dct, &ql);
                    encode_block(&mut w, &coeffs, &dc_luma, &ac_luma, &mut prev_y);
                }
            }
            for (comp, prev) in [(0u8, &mut prev_cb), (1u8, &mut prev_cr)] {
                let mut block = [0f32; 64];
                for i in 0..8 {
                    for j in 0..8 {
                        // 2x2 average over the MCU for this chroma sample.
                        let mut sr = 0f32;
                        let mut sg = 0f32;
                        let mut sb = 0f32;
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let (r, g, b) = px(
                                    mx * 16 + j as u32 * 2 + dx as u32,
                                    my * 16 + i as u32 * 2 + dy as u32,
                                );
                                sr += r;
                                sg += g;
                                sb += b;
                            }
                        }
                        let (r, g, b) = (sr / 4.0, sg / 4.0, sb / 4.0);
                        let v = if comp == 0 {
                            -0.16873589 * r - 0.33126411 * g + 0.5 * b + 128.0
                        } else {
                            0.5 * r - 0.41868759 * g - 0.08131241 * b + 128.0
                        };
                        block[i * 8 + j] = v - 128.0;
                    }
                }
                let dct = dct_block(&block, &cos);
                let coeffs = quantize(&dct, &qc);
                encode_block(&mut w, &coeffs, &dc_chroma, &ac_chroma, prev);
            }
        }
    }
    out.extend_from_slice(&w.finish());
    // EOI.
    out.extend_from_slice(&[0xFF, 0xD9]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_structure_markers() {
        let rgb = vec![128u8; 16 * 16 * 3];
        let jpg = encode_jpeg_rgb(16, 16, &rgb, 80).unwrap();
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8]); // SOI
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9]); // EOI
        // SOF0 present with the right dimensions.
        let mut found = false;
        let mut i = 2;
        while i + 9 < jpg.len() {
            if jpg[i] == 0xFF && jpg[i + 1] == 0xC0 {
                let h = ((jpg[i + 5] as u32) << 8) | jpg[i + 6] as u32;
                let w = ((jpg[i + 7] as u32) << 8) | jpg[i + 8] as u32;
                assert_eq!((w, h), (16, 16));
                found = true;
            }
            if jpg[i] != 0xFF {
                i += 1;
                continue;
            }
            // Skip markers with lengths; SOS starts entropy data (stop).
            if jpg[i + 1] == 0xDA {
                break;
            }
            if jpg[i + 1] == 0xD8 || jpg[i + 1] == 0xD9 || (0xD0..=0xD7).contains(&jpg[i + 1]) {
                i += 2;
                continue;
            }
            let len = ((jpg[i + 2] as usize) << 8) | jpg[i + 3] as usize;
            i += 2 + len;
        }
        assert!(found, "SOF0 marker not found");
    }

    #[test]
    fn jpeg_rejects_bad_dimensions() {
        assert!(encode_jpeg_rgb(0, 1, &[], 80).is_err());
        assert!(encode_jpeg_rgb(1, 1, &[1, 2], 80).is_err());
    }

    #[test]
    fn downscale_averages_boxes() {
        // 4x2: left half red, right half green -> 2x1 keeps the split.
        let mut rgba = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                if x < 2 {
                    rgba.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    rgba.extend_from_slice(&[0, 255, 0, 255]);
                }
            }
        }
        let (w, h, rgb) = downscale_rgb(&rgba, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(&rgb[0..3], &[255, 0, 0]);
        assert_eq!(&rgb[3..6], &[0, 255, 0]);
    }

    #[test]
    fn downscale_keeps_original_when_target_wider() {
        let rgba = vec![9u8; 4 * 4 * 4];
        let (w, h, rgb) = downscale_rgb(&rgba, 4, 4, 1280);
        assert_eq!((w, h), (4, 4));
        assert_eq!(rgb.len(), 4 * 4 * 3);
    }
}
