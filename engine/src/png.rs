//! Minimal PNG encoder (RGBA8 -> PNG bytes), zero dependencies.
//!
//! Each scanline uses filter type 0 (None) and the zlib stream is made of
//! stored (uncompressed) DEFLATE blocks, so no compressor is needed. The
//! output is a valid PNG any decoder can read; it is simply larger than a
//! compressed one. Screenshots are the main use; a future improvement is a
//! real DEFLATE compressor for smaller frames.

/// CRC-32 (IEEE, polynomial 0xEDB88320), as PNG requires.
pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Adler-32, as the zlib wrapper requires.
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
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

    // Raw scanlines: one filter byte (0) per row.
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }

    // zlib stream: header 0x78 0x01 (no/low compression) + stored blocks.
    let mut z = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < raw.len() || raw.is_empty() {
        let take = (raw.len() - offset).min(65535);
        let final_block = offset + take >= raw.len();
        z.push(if final_block { 1 } else { 0 });
        z.extend_from_slice(&(take as u16).to_le_bytes());
        z.extend_from_slice(&(!(take as u16)).to_le_bytes());
        z.extend_from_slice(&raw[offset..offset + take]);
        offset += take;
        if raw.is_empty() {
            break;
        }
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &z);
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
}
