//! Restricted PNG/zlib decoder for PICO-8 cartridge images. No host I/O.
//! PNG: https://www.w3.org/TR/png-3/ ; DEFLATE: RFC 1951.
const WIDTH: usize = 160;
const HEIGHT: usize = 205;
const STRIDE: usize = WIDTH * 4;
const DECODED: usize = (STRIDE + 1) * HEIGHT;

/// Extract the 32 KiB cartridge and version byte encoded in the two low
/// bits of each RGBA component. Only noninterlaced 8-bit RGBA is accepted.
pub fn cartridge(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let rgba = rgba(bytes)?;
    Ok(rgba
        .chunks_exact(4)
        .take(0x8001)
        .map(|p| ((p[0] & 3) << 4) | ((p[1] & 3) << 2) | (p[2] & 3) | ((p[3] & 3) << 6))
        .collect())
}

/// The label drawn on a cartridge's picture (128x128 at 16, 24), each pixel
/// the palette colour nearest to it: the data in the pixels' low bits
/// changes them only a little.
pub fn label(bytes: &[u8]) -> Result<Box<[u8; 128 * 128]>, String> {
    let rgba = rgba(bytes)?;
    let mut out = Box::new([0u8; 128 * 128]);
    for y in 0..128 {
        for x in 0..128 {
            let p = &rgba[((y + 24) * WIDTH + x + 16) * 4..][..3];
            out[y * 128 + x] = nearest(p);
        }
    }
    Ok(out)
}

fn nearest(p: &[u8]) -> u8 {
    let d = |c: u32| {
        let (r, g, b) = ((c >> 16) as i32, (c >> 8 & 255) as i32, (c & 255) as i32);
        let (dr, dg, db) = (r - i32::from(p[0]), g - i32::from(p[1]), b - i32::from(p[2]));
        dr * dr + dg * dg + db * db
    };
    (0..32u8).min_by_key(|&i| d(crate::graphics::PALETTE[usize::from(i)])).unwrap_or(0)
}

/// The picture's pixels, RGBA.
fn rgba(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("PNG exceeds 16 MiB".into());
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("invalid PNG signature".into());
    }
    let (mut pos, mut header, mut ended, mut idat, mut past_idat) =
        (8usize, false, false, false, false);
    let mut compressed = Vec::new();
    while pos < bytes.len() {
        let chunk_header = bytes
            .get(pos..pos + 8)
            .ok_or("truncated PNG chunk header")?;
        let len = u32::from_be_bytes(chunk_header[..4].try_into().unwrap()) as usize;
        let end = pos
            .checked_add(12)
            .and_then(|n| n.checked_add(len))
            .ok_or("PNG chunk length overflow")?;
        let chunk = bytes.get(pos..end).ok_or("truncated PNG chunk")?;
        let kind = &chunk[4..8];
        if !kind.iter().all(u8::is_ascii_alphabetic) || kind[2] & 32 != 0 {
            return Err("invalid PNG chunk type".into());
        }
        let data = &chunk[8..8 + len];
        let expected = u32::from_be_bytes(chunk[8 + len..12 + len].try_into().unwrap());
        if crc32(&chunk[4..8 + len]) != expected {
            return Err("PNG chunk CRC mismatch".into());
        }
        if !header && kind != b"IHDR" {
            return Err("PNG IHDR must be first".into());
        }
        if idat && kind != b"IDAT" {
            past_idat = true;
        }
        match kind {
            b"IHDR" => {
                if header || len != 13 {
                    return Err("invalid or duplicate PNG IHDR".into());
                }
                let width = u32::from_be_bytes(data[..4].try_into().unwrap());
                let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                if width != WIDTH as u32 || height != HEIGHT as u32 {
                    return Err("PICO-8 PNG must be 160 by 205 pixels".into());
                }
                if data[8] != 8 || data[9] != 6 {
                    return Err("PICO-8 PNG requires 8-bit RGBA colour".into());
                }
                if data[10] != 0 || data[11] != 0 || data[12] != 0 {
                    return Err("unsupported PNG compression, filter, or interlace mode".into());
                }
                header = true;
            }
            b"IDAT" => {
                if past_idat {
                    return Err("PNG IDAT chunks must be consecutive".into());
                }
                idat = true;
                compressed.extend_from_slice(data);
            }
            b"IEND" => {
                if len != 0 || !idat {
                    return Err("invalid PNG IEND or missing image data".into());
                }
                ended = true;
                pos = end;
                break;
            }
            b"PLTE" => {
                if idat || len == 0 || len > 768 || !len.is_multiple_of(3) {
                    return Err("invalid PNG palette chunk".into());
                }
            }
            _ if kind[0] & 32 == 0 => return Err("unsupported critical PNG chunk".into()),
            _ => (),
        }
        pos = end;
    }
    if !ended || pos != bytes.len() {
        return Err("PNG missing IEND or has trailing data".into());
    }
    let scanlines = inflate_zlib(&compressed, DECODED)?;
    if scanlines.len() != DECODED {
        return Err("PNG decompressed image has wrong size".into());
    }
    let mut rgba = vec![0u8; STRIDE * HEIGHT];
    for y in 0..HEIGHT {
        let row = &scanlines[y * (STRIDE + 1)..(y + 1) * (STRIDE + 1)];
        let filter = row[0];
        if filter > 4 {
            return Err("invalid PNG row filter".into());
        }
        for x in 0..STRIDE {
            let left = if x >= 4 { rgba[y * STRIDE + x - 4] } else { 0 };
            let up = if y > 0 { rgba[(y - 1) * STRIDE + x] } else { 0 };
            let corner = if y > 0 && x >= 4 {
                rgba[(y - 1) * STRIDE + x - 4]
            } else {
                0
            };
            let predictor = match filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                4 => paeth(left, up, corner),
                _ => unreachable!(),
            };
            rgba[y * STRIDE + x] = row[x + 1].wrapping_add(predictor);
        }
    }
    Ok(rgba)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i32::from(a) + i32::from(b) - i32::from(c);
    let (da, db, dc) = (
        (p - i32::from(a)).abs(),
        (p - i32::from(b)).abs(),
        (p - i32::from(c)).abs(),
    );
    if da <= db && da <= dc {
        a
    } else if db <= dc {
        b
    } else {
        c
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Bits<'_> {
    fn read(&mut self, n: usize) -> Result<u32, String> {
        if n > 16
            || self
                .pos
                .checked_add(n)
                .is_none_or(|end| end > self.bytes.len() * 8)
        {
            return Err("truncated DEFLATE bit stream".into());
        }
        let mut value = 0;
        for i in 0..n {
            value |= u32::from((self.bytes[self.pos / 8] >> (self.pos & 7)) & 1) << i;
            self.pos += 1;
        }
        Ok(value)
    }
    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

struct Huffman {
    counts: [u16; 16],
    first: [u16; 16],
    offsets: [u16; 16],
    symbols: Vec<u16>,
}
impl Huffman {
    fn new(lengths: &[u8], allow_empty: bool) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err("invalid Huffman code length".into());
            }
            if len > 0 {
                counts[usize::from(len)] += 1;
            }
        }
        let mut left = 1i32;
        for &count in &counts[1..] {
            left = left * 2 - i32::from(count);
            if left < 0 {
                return Err("oversubscribed Huffman tree".into());
            }
        }
        let used = counts[1..].iter().map(|&n| usize::from(n)).sum::<usize>();
        if used == 0 && !allow_empty {
            return Err("empty Huffman tree".into());
        }
        if used > 0 && left > 0 && !(used == 1 && counts[1] == 1) {
            return Err("incomplete Huffman tree".into());
        }
        let mut first = [0u16; 16];
        let mut offsets = [0u16; 16];
        for i in 1..16 {
            first[i] = (first[i - 1] + counts[i - 1]) << 1;
            offsets[i] = offsets[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0; used];
        let mut next = offsets;
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let len = usize::from(len);
                symbols[usize::from(next[len])] = symbol as u16;
                next[len] += 1;
            }
        }
        Ok(Self {
            counts,
            first,
            offsets,
            symbols,
        })
    }
    fn decode(&self, bits: &mut Bits<'_>) -> Result<usize, String> {
        let mut code = 0u16;
        for len in 1..16 {
            code = (code << 1) | bits.read(1)? as u16;
            if code >= self.first[len] && code - self.first[len] < self.counts[len] {
                return Ok(usize::from(
                    self.symbols[usize::from(self.offsets[len] + code - self.first[len])],
                ));
            }
        }
        Err("invalid DEFLATE Huffman code".into())
    }
}

fn dynamic_trees(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), String> {
    let literals = bits.read(5)? as usize + 257;
    let distances = bits.read(5)? as usize + 1;
    let code_count = bits.read(4)? as usize + 4;
    if literals > 286 || distances > 32 {
        return Err("invalid dynamic Huffman alphabet".into());
    }
    // RFC 1951's permutation is deliberately spelled out below.
    const CODE_ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut code_lengths = [0u8; 19];
    for &index in &CODE_ORDER[..code_count] {
        code_lengths[index] = bits.read(3)? as u8;
    }
    let tree = Huffman::new(&code_lengths, false)?;
    let mut lengths = Vec::with_capacity(literals + distances);
    while lengths.len() < literals + distances {
        let symbol = tree.decode(bits)?;
        match symbol {
            0..=15 => lengths.push(symbol as u8),
            16 => {
                let previous = *lengths
                    .last()
                    .ok_or("Huffman repeat has no previous length")?;
                let repeat = bits.read(2)? as usize + 3;
                if lengths.len() + repeat > literals + distances {
                    return Err("Huffman repeat exceeds alphabet".into());
                }
                lengths.resize(lengths.len() + repeat, previous);
            }
            17 | 18 => {
                let repeat = if symbol == 17 {
                    bits.read(3)? as usize + 3
                } else {
                    bits.read(7)? as usize + 11
                };
                if lengths.len() + repeat > literals + distances {
                    return Err("Huffman zero repeat exceeds alphabet".into());
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            _ => return Err("invalid Huffman length symbol".into()),
        }
    }
    if lengths[256] == 0 {
        return Err("Huffman tree has no end-of-block code".into());
    }
    Ok((
        Huffman::new(&lengths[..literals], false)?,
        Huffman::new(&lengths[literals..], true)?,
    ))
}

fn inflate_zlib(bytes: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    if bytes.len() < 6 {
        return Err("truncated zlib stream".into());
    }
    let header = u16::from_be_bytes([bytes[0], bytes[1]]);
    if bytes[0] & 15 != 8 || bytes[0] >> 4 > 7 || !header.is_multiple_of(31) || bytes[1] & 32 != 0 {
        return Err("invalid or unsupported zlib header".into());
    }
    let payload = &bytes[2..bytes.len() - 4];
    let mut bits = Bits {
        bytes: payload,
        pos: 0,
    };
    let mut output = Vec::with_capacity(limit);
    loop {
        let final_block = bits.read(1)? != 0;
        match bits.read(2)? {
            0 => {
                bits.align();
                let len = bits.read(16)? as usize;
                let inverse = bits.read(16)? as u16;
                if len as u16 != !inverse {
                    return Err("DEFLATE stored block length mismatch".into());
                }
                if output.len() + len > limit {
                    return Err("PNG decompression exceeds image size".into());
                }
                let start = bits.pos / 8;
                let data = payload
                    .get(start..start + len)
                    .ok_or("truncated DEFLATE stored block")?;
                output.extend_from_slice(data);
                bits.pos += len * 8;
            }
            kind @ (1 | 2) => {
                let (literal, distance) = if kind == 1 {
                    let mut lengths = [8u8; 288];
                    lengths[144..256].fill(9);
                    lengths[256..280].fill(7);
                    lengths[280..].fill(8);
                    (
                        Huffman::new(&lengths, false)?,
                        Huffman::new(&[5u8; 32], false)?,
                    )
                } else {
                    dynamic_trees(&mut bits)?
                };
                decode_block(&mut bits, &literal, &distance, &mut output, limit)?;
            }
            _ => return Err("reserved DEFLATE block type".into()),
        }
        if final_block {
            break;
        }
    }
    if bits.pos.div_ceil(8) != payload.len() {
        return Err("trailing bytes in DEFLATE stream".into());
    }
    let expected = u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
    if adler32(&output) != expected {
        return Err("zlib Adler-32 mismatch".into());
    }
    Ok(output)
}

fn decode_block(
    bits: &mut Bits<'_>,
    literal: &Huffman,
    distance: &Huffman,
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<(), String> {
    const LENGTH_BASE: [usize; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LENGTH_EXTRA: [usize; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DISTANCE_BASE: [usize; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DISTANCE_EXTRA: [usize; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    loop {
        match literal.decode(bits)? {
            byte @ 0..=255 => {
                if output.len() == limit {
                    return Err("PNG decompression exceeds image size".into());
                }
                output.push(byte as u8);
            }
            256 => return Ok(()),
            symbol @ 257..=285 => {
                let index = symbol - 257;
                let len = LENGTH_BASE[index] + bits.read(LENGTH_EXTRA[index])? as usize;
                let symbol = distance.decode(bits)?;
                if symbol >= 30 {
                    return Err("reserved DEFLATE distance symbol".into());
                }
                let dist = DISTANCE_BASE[symbol] + bits.read(DISTANCE_EXTRA[symbol])? as usize;
                if dist > output.len() || dist > 32768 {
                    return Err("DEFLATE backreference before output".into());
                }
                if output.len() + len > limit {
                    return Err("PNG decompression exceeds image size".into());
                }
                for _ in 0..len {
                    output.push(output[output.len() - dist]);
                }
            }
            _ => return Err("reserved DEFLATE length symbol".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let mut crc_data = kind.to_vec();
        crc_data.extend_from_slice(data);
        png.extend_from_slice(&crc32(&crc_data).to_be_bytes());
    }
    fn stored(data: &[u8]) -> Vec<u8> {
        let mut z = vec![0x78, 0x01];
        let count = data.len().div_ceil(65535);
        for (i, block) in data.chunks(65535).enumerate() {
            z.push(u8::from(i + 1 == count));
            let len = block.len() as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(block);
        }
        z.extend_from_slice(&adler32(data).to_be_bytes());
        z
    }
    fn fixture(filter: u8) -> (Vec<u8>, Vec<u8>) {
        let rgba = (0..STRIDE * HEIGHT)
            .map(|i| ((i * 17 + i / 7) & 255) as u8)
            .collect::<Vec<_>>();
        let mut filtered = Vec::with_capacity(DECODED);
        for y in 0..HEIGHT {
            filtered.push(filter);
            for x in 0..STRIDE {
                let left = if x >= 4 { rgba[y * STRIDE + x - 4] } else { 0 };
                let up = if y > 0 { rgba[(y - 1) * STRIDE + x] } else { 0 };
                let corner = if y > 0 && x >= 4 {
                    rgba[(y - 1) * STRIDE + x - 4]
                } else {
                    0
                };
                let predictor = match filter {
                    0 => 0,
                    1 => left,
                    2 => up,
                    3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                    4 => paeth(left, up, corner),
                    _ => 0,
                };
                filtered.push(rgba[y * STRIDE + x].wrapping_sub(predictor));
            }
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(
            &mut png,
            b"IHDR",
            &[0, 0, 0, 160, 0, 0, 0, 205, 8, 6, 0, 0, 0],
        );
        chunk(&mut png, b"IDAT", &stored(&filtered));
        chunk(&mut png, b"IEND", &[]);
        let rom = rgba
            .chunks_exact(4)
            .take(0x8001)
            .map(|p| ((p[0] & 3) << 4) | ((p[1] & 3) << 2) | (p[2] & 3) | ((p[3] & 3) << 6))
            .collect();
        (png, rom)
    }
    #[test]
    fn all_png_row_filters_and_payload_bit_order() {
        for filter in 0..5 {
            let (png, expected) = fixture(filter);
            assert_eq!(cartridge(&png).unwrap(), expected);
        }
    }
    #[test]
    fn crc_truncation_and_adler_corruption_are_errors() {
        let (mut png, _) = fixture(0);
        png[100] ^= 1;
        assert!(cartridge(&png).unwrap_err().contains("CRC"));
        for len in [0, 7, 8, 16, 32, png.len() - 1] {
            assert!(cartridge(&png[..len]).is_err());
        }
        let mut z = stored(b"abc");
        let last = z.len() - 1;
        z[last] ^= 1;
        assert!(inflate_zlib(&z, 10).unwrap_err().contains("Adler"));
    }
    #[test]
    fn fixed_huffman_and_overlapping_backreferences() {
        // Python zlib.compress(b"hello hello hello") uses a fixed tree.
        let z = [
            120, 156, 203, 72, 205, 201, 201, 87, 200, 64, 144, 0, 58, 46, 6, 125,
        ];
        assert_eq!(inflate_zlib(&z, 17).unwrap(), b"hello hello hello");
        assert!(inflate_zlib(&z, 16).is_err());
    }

    #[test]
    fn dynamic_huffman_repeat_codes_and_long_backreferences() {
        // zlib-compressed repeated text; this block uses a dynamic tree.
        let z = [
            120, 156, 237, 202, 219, 21, 64, 48, 20, 5, 209, 86, 78, 5, 122, 34, 130, 120, 93, 34,
            241, 170, 158, 165, 5, 191, 243, 57, 107, 118, 234, 188, 214, 28, 220, 160, 42, 218,
            49, 171, 177, 83, 125, 158, 150, 77, 182, 251, 168, 244, 238, 177, 188, 47, 213, 214,
            22, 95, 129, 193, 96, 48, 24, 12, 6, 131, 193, 96, 48, 24, 12, 6, 131, 127, 227, 7, 26,
            137, 92, 23,
        ];
        let expected = b"the quick brown fox jumps over the lazy dog. ".repeat(100);
        assert_eq!(inflate_zlib(&z, expected.len()).unwrap(), expected);
    }
    #[test]
    fn rejects_invalid_huffman_trees_and_reserved_block() {
        assert!(Huffman::new(&[1, 1, 1], false).is_err());
        assert!(Huffman::new(&[2, 2], false).is_err());
        assert!(inflate_zlib(&[0x78, 0x01, 0x07, 0, 0, 0, 1], 100).is_err());
    }
}
