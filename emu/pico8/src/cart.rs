//! Text cartridge decoding. The ROM uses the hardware's packed memory layout,
//! rather than the human-readable nibble order in a `.p8` file.

use std::fmt;

#[derive(Clone, Debug)]
pub struct Cartridge {
    /// The Lua code, in P8SCII bytes.
    pub code: Vec<u8>,
    pub rom: Box<[u8; 0x4300]>,
    /// A text cartridge's label (__label__), palette indices, if it has one.
    pub label: Option<Box<[u8; 128 * 128]>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Lua,
    Gfx,
    Gff,
    Label,
    Map,
    Sfx,
    Music,
}

impl Cartridge {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Self::from_rom(&crate::png::cartridge(bytes).map_err(Error)?);
        }
        if bytes.len() > 1024 * 1024 {
            return Err(Error("text cartridge exceeds 1 MiB".into()));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| Error("cartridge is not UTF-8 text".into()))?;
        let mut lines = text.trim_start_matches('\u{feff}').lines();
        if lines.next() != Some("pico-8 cartridge // http://www.pico-8.com") {
            return Err(Error("missing PICO-8 cartridge header".into()));
        }
        let version = lines
            .next()
            .and_then(|l| l.strip_prefix("version "))
            .and_then(|n| n.parse::<u8>().ok());
        if version.is_none() {
            return Err(Error("invalid or missing cartridge version".into()));
        }
        let mut cart = Self {
            label: None,
            code: Vec::new(),
            rom: Box::new([0; 0x4300]),
        };
        // Omitted text SFX rows retain the editor's default tempo.
        for id in 0..64 {
            cart.rom[0x3200 + id * 68 + 65] = 16;
        }
        let mut section = None;
        let mut skipping = false;
        let mut seen = [false; 7];
        let mut row = 0;
        for (line_index, line) in lines.enumerate() {
            let line_no = line_index + 3;
            if line.starts_with("__") && line.ends_with("__") {
                let next = match line {
                    "__lua__" => Section::Lua,
                    "__gfx__" => Section::Gfx,
                    "__gff__" => Section::Gff,
                    "__label__" => Section::Label,
                    "__map__" => Section::Map,
                    "__sfx__" => Section::Sfx,
                    "__music__" => Section::Music,
                    // Newer sections (__meta:…__, __change_mask__): not for running.
                    _ => {
                        section = Some(Section::Label);
                        skipping = true;
                        continue;
                    }
                };
                skipping = false;
                let index = next as usize;
                if seen[index] {
                    return Err(Error(format!("line {line_no}: duplicate section {line}")));
                }
                seen[index] = true;
                section = Some(next);
                row = 0;
                continue;
            }
            let Some(current) = section else {
                continue;
            };
            if skipping {
                continue;
            }
            if current == Section::Lua {
                // #include needs the files beside the cartridge: a
                // cartridge for the console has them in it already.
                if !line.trim_start().starts_with("#include") {
                    cart.code.extend(encode_p8scii(line));
                }
                cart.code.push(b'\n');
                continue;
            }
            if line.is_empty() {
                continue;
            }
            let fail = |message: &str| Error(format!("line {line_no}: {message}"));
            let (width, rows) = match current {
                Section::Gfx | Section::Label => (128, 128),
                Section::Gff => (256, 2),
                Section::Map => (256, 32),
                Section::Sfx => (168, 64),
                Section::Music => (11, 64),
                Section::Lua => unreachable!(),
            };
            if row >= rows || line.len() < width {
                continue;
            }
            let line = &line[..width];
            if current == Section::Music {
                if line.as_bytes()[2] != b' ' {
                    return Err(fail("music flags must be followed by a space"));
                }
                let flags =
                    hex_byte(&line.as_bytes()[..2]).ok_or_else(|| fail("invalid music flags"))?;
                for channel in 0..4 {
                    let start = 3 + channel * 2;
                    let value = hex_byte(&line.as_bytes()[start..start + 2])
                        .ok_or_else(|| fail("invalid music SFX"))?;
                    cart.rom[0x3100 + row * 4 + channel] =
                        value | if flags & (1 << channel) != 0 { 0x80 } else { 0 };
                }
            } else if current == Section::Label {
                // 0-f, and g-v for the secret colours.
                let label = cart.label.get_or_insert_with(|| Box::new([0; 128 * 128]));
                for (x, c) in line.bytes().take(128).enumerate() {
                    label[row * 128 + x] = match c {
                        b'0'..=b'9' => c - b'0',
                        b'a'..=b'f' => c - b'a' + 10,
                        b'g'..=b'v' => c - b'g' + 16,
                        _ => 0,
                    };
                }
            } else {
                let nibbles: Vec<u8> = line
                    .bytes()
                    .map(hex_nibble)
                    .collect::<Option<_>>()
                    .ok_or_else(|| fail("non-hexadecimal section data"))?;
                match current {
                    Section::Gfx => {
                        for x in 0..64 {
                            cart.rom[row * 64 + x] = nibbles[x * 2] | nibbles[x * 2 + 1] << 4;
                        }
                    }
                    Section::Gff | Section::Map => {
                        let base = if current == Section::Gff {
                            0x3000
                        } else {
                            0x2000
                        };
                        for x in 0..128 {
                            cart.rom[base + row * 128 + x] =
                                nibbles[x * 2] << 4 | nibbles[x * 2 + 1];
                        }
                    }
                    Section::Sfx => {
                        let base = 0x3200 + row * 68;
                        for i in 0..4 {
                            cart.rom[base + 64 + i] = nibbles[2 * i] << 4 | nibbles[2 * i + 1];
                        }
                        for note in 0..32 {
                            let a = 8 + note * 5;
                            let pitch = nibbles[a] << 4 | nibbles[a + 1];
                            let instrument = nibbles[a + 2];
                            let volume = nibbles[a + 3];
                            let effect = nibbles[a + 4];
                            let packed = u16::from(pitch & 63)
                                | (u16::from(instrument & 7) << 6)
                                | (u16::from(volume) << 9)
                                | (u16::from(effect) << 12)
                                | (u16::from(instrument >> 3) << 15);
                            cart.rom[base + 2 * note..base + 2 * note + 2]
                                .copy_from_slice(&packed.to_le_bytes());
                        }
                    }
                    Section::Label => {}
                    _ => unreachable!(),
                }
            }
            row += 1;
        }
        if !seen[Section::Lua as usize] {
            return Err(Error("missing __lua__ section".into()));
        }
        Ok(cart)
    }

    /// Decodes the raw 32 KiB data hidden in a PNG cartridge. A trailing
    /// version byte is accepted, but is not part of the runtime ROM.
    pub fn from_rom(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < 0x8000 {
            return Err(Error("truncated binary cartridge".into()));
        }
        let input = &bytes[0x4300..0x8000];
        let code = if input.starts_with(b":c:\0") {
            legacy_code(input)?
        } else if input.starts_with(b"\0pxa") {
            pxa_code(input)?
        } else {
            input[..input.iter().position(|&b| b == 0).unwrap_or(input.len())].to_vec()
        };
        let mut rom = Box::new([0; 0x4300]);
        rom.copy_from_slice(&bytes[..0x4300]);
        Ok(Self { code, rom, label: None })
    }
}

fn legacy_code(input: &[u8]) -> Result<Vec<u8>, Error> {
    const LETTERS: &[u8] = b"\n 0123456789abcdefghijklmnopqrstuvwxyz!#%(){}[]<>+=/*:;.,~_";
    let length = usize::from(u16::from_be_bytes([input[4], input[5]]));
    let mut output = Vec::with_capacity(length);
    let mut pos = 8;
    while output.len() < length {
        let byte = *input
            .get(pos)
            .ok_or_else(|| Error("truncated legacy compressed code".into()))?;
        pos += 1;
        if byte == 0 {
            output.push(
                *input
                    .get(pos)
                    .ok_or_else(|| Error("truncated legacy code literal".into()))?,
            );
            pos += 1;
        } else if byte < 0x3c {
            output.push(LETTERS[usize::from(byte - 1)]);
        } else {
            let next = *input
                .get(pos)
                .ok_or_else(|| Error("truncated legacy code reference".into()))?;
            pos += 1;
            let offset = usize::from(byte - 0x3c) * 16 + usize::from(next & 15);
            let count = usize::from(next >> 4) + 2;
            copy_reference(&mut output, offset, count, length)?;
        }
    }
    Ok(output)
}

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Bits<'_> {
    fn get(&mut self, count: usize) -> Result<usize, Error> {
        if self.pos + count > self.bytes.len() * 8 {
            return Err(Error("truncated PXA bitstream".into()));
        }
        let mut value = 0;
        for i in 0..count {
            value |= usize::from((self.bytes[self.pos / 8] >> (self.pos % 8)) & 1) << i;
            self.pos += 1;
        }
        Ok(value)
    }
}
fn pxa_code(input: &[u8]) -> Result<Vec<u8>, Error> {
    let length = usize::from(u16::from_be_bytes([input[4], input[5]]));
    let compressed = usize::from(u16::from_be_bytes([input[6], input[7]]));
    if compressed < 8 || compressed > input.len() {
        return Err(Error("invalid PXA compressed length".into()));
    }
    let mut bits = Bits {
        bytes: &input[..compressed],
        pos: 64,
    };
    let mut alphabet = [0u8; 256];
    for (index, value) in alphabet.iter_mut().enumerate() {
        *value = index as u8;
    }
    let mut output = Vec::with_capacity(length);
    while output.len() < length {
        if bits.get(1)? != 0 {
            let mut width = 4;
            while bits.get(1)? != 0 {
                width += 1;
                if width > 8 {
                    return Err(Error("invalid PXA alphabet index".into()));
                }
            }
            let index = bits.get(width)? + (1 << width) - 16;
            if index >= alphabet.len() {
                return Err(Error("invalid PXA alphabet index".into()));
            }
            let literal = alphabet[index];
            if literal == 0 {
                return Err(Error("PXA code ended before declared length".into()));
            }
            alphabet[..=index].rotate_right(1);
            output.push(literal);
        } else {
            let width = if bits.get(1)? == 0 {
                15
            } else if bits.get(1)? != 0 {
                5
            } else {
                10
            };
            let offset = bits.get(width)? + 1;
            if width == 10 && offset == 1 {
                loop {
                    let literal = bits.get(8)? as u8;
                    if literal == 0 {
                        break;
                    }
                    if output.len() >= length {
                        return Err(Error("PXA literal run exceeds declared code length".into()));
                    }
                    output.push(literal);
                }
            } else {
                let mut count = 3;
                loop {
                    let more = bits.get(3)?;
                    count += more;
                    if count > length {
                        return Err(Error("PXA reference exceeds declared code length".into()));
                    }
                    if more != 7 {
                        break;
                    }
                }
                copy_reference(&mut output, offset, count, length)?;
            }
        }
    }
    Ok(output)
}
fn copy_reference(
    output: &mut Vec<u8>,
    offset: usize,
    count: usize,
    limit: usize,
) -> Result<(), Error> {
    if offset == 0 || offset > output.len() || count > limit - output.len() {
        return Err(Error("invalid compressed-code back reference".into()));
    }
    for _ in 0..count {
        output.push(output[output.len() - offset]);
    }
    Ok(())
}

/// P8SCII as the text format writes it, a string a character (some are
/// two code points, an emoji with its variation selector, dropped here).
const P8SCII: [&str; 256] = {
    const LOW: [&str; 32] = [
        "\0", "¹", "²", "³", "⁴", "⁵", "⁶", "⁷", "⁸", "\t", "\n", "ᵇ", "ᶜ", "\r", "ᵉ", "ᶠ", "▮", "■", "□", "⁙", "⁘", "‖", "◀", "▶",
        "「", "」", "¥", "•", "、", "。", "゛", "゜",
    ];
    const HIGH: [&str; 128] = [
        "█", "▒", "🐱", "⬇", "░", "✽", "●", "♥", "☉", "웃", "⌂", "⬅", "😐", "♪", "🅾", "◆", "…", "➡", "★", "⧗", "⬆", "ˇ", "∧", "❎", "▤",
        "▥", "あ", "い", "う", "え", "お", "か", "き", "く", "け", "こ", "さ", "し", "す", "せ", "そ", "た", "ち", "つ", "て", "と", "な", "に",
        "ぬ", "ね", "の", "は", "ひ", "ふ", "へ", "ほ", "ま", "み", "む", "め", "も", "や", "ゆ", "よ", "ら", "り", "る", "れ", "ろ", "わ", "を",
        "ん", "っ", "ゃ", "ゅ", "ょ", "ア", "イ", "ウ", "エ", "オ", "カ", "キ", "ク", "ケ", "コ", "サ", "シ", "ス", "セ", "ソ", "タ", "チ", "ツ",
        "テ", "ト", "ナ", "ニ", "ヌ", "ネ", "ノ", "ハ", "ヒ", "フ", "ヘ", "ホ", "マ", "ミ", "ム", "メ", "モ", "ヤ", "ユ", "ヨ", "ラ", "リ", "ル",
        "レ", "ロ", "ワ", "ヲ", "ン", "ッ", "ャ", "ュ", "ョ", "◜", "◝",
    ];
    let mut t = [""; 256];
    let mut i = 0;
    while i < 32 {
        t[i] = LOW[i];
        i += 1;
    }
    t[127] = "○";
    while i < 128 {
        i += 1;
    }
    let mut j = 0;
    while j < 128 {
        t[128 + j] = HIGH[j];
        j += 1;
    }
    t
};

/// A line of the text format's code to P8SCII bytes.
fn encode_p8scii(line: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    for ch in line.chars() {
        if ch == '\u{fe0f}' {
            continue;
        }
        if (ch as u32) < 127 {
            out.push(ch as u8);
            continue;
        }
        let mut buf = [0u8; 4];
        let s: &str = ch.encode_utf8(&mut buf);
        match P8SCII.iter().position(|&p| p == s) {
            Some(i) => out.push(i as u8),
            None => out.push(b'?'),
        }
    }
    out
}

fn hex_nibble(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|n| n as u8)
}
fn hex_byte(chars: &[u8]) -> Option<u8> {
    Some(hex_nibble(chars[0])? << 4 | hex_nibble(chars[1])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cart(data: &str) -> Vec<u8> {
        format!("pico-8 cartridge // http://www.pico-8.com\nversion 42\n__lua__\nx=1\n{data}")
            .into_bytes()
    }
    #[test]
    fn decodes_shared_graphics_and_big_endian_map_hex() {
        let gfx = format!("12{}", "0".repeat(126));
        let map = format!("ab{}", "0".repeat(254));
        let c = Cartridge::parse(&cart(&format!("__gfx__\n{gfx}\n__map__\n{map}\n"))).unwrap();
        assert_eq!(c.rom[0], 0x21);
        assert_eq!(c.rom[0x2000], 0xab);
        assert_eq!(c.code, b"x=1\n");
    }
    #[test]
    fn packs_notes_and_music_flags() {
        let sfx = format!("01100204{}{}", "18375", "00000".repeat(31));
        let c =
            Cartridge::parse(&cart(&format!("__sfx__\n{sfx}\n__music__\n07 00414243\n"))).unwrap();
        assert_eq!(c.rom[0x3240..0x3244], [1, 16, 2, 4]);
        assert_eq!(
            u16::from_le_bytes([c.rom[0x3200], c.rom[0x3201]]),
            24 | 3 << 6 | 7 << 9 | 5 << 12
        );
        assert_eq!(c.rom[0x3100..0x3104], [0x80, 0xc1, 0xc2, 0x43]);
    }
    #[test]
    fn rejects_what_is_no_cartridge_and_skips_what_it_cannot_use() {
        for data in ["__gfx__\n0\n", "#include foo.lua\n", "__meta:title__\nhello\n", "__change_mask__\n00\n"] {
            assert!(Cartridge::parse(&cart(data)).is_ok(), "{data}");
        }
        assert!(Cartridge::parse(b"\x89PNG\r\n\x1a\n").is_err());
        assert!(Cartridge::parse(b"bad").is_err());
    }
    #[test]
    fn legacy_code_literals_and_overlapping_references() {
        let input = [b':', b'c', b':', 0, 0, 6, 0, 0, 0, b'a', 0x3c, 0x31];
        assert_eq!(legacy_code(&input).unwrap(), b"aaaaaa");
        let mut bad = input;
        bad[11] = 0x30;
        assert!(legacy_code(&bad).is_err());
    }
    #[test]
    fn pxa_literal_run_and_reference() {
        let mut stream = vec![0u8; 16];
        stream[..8].copy_from_slice(&[0, b'p', b'x', b'a', 0, 6, 0, 16]);
        let mut bitpos = 64;
        let mut put = |value: usize, count: usize| {
            for bit in 0..count {
                stream[bitpos / 8] |= (((value >> bit) & 1) as u8) << (bitpos % 8);
                bitpos += 1;
            }
        };
        put(0, 1);
        put(1, 1);
        put(0, 1);
        put(0, 10);
        put(b'a' as usize, 8);
        put(b'b' as usize, 8);
        put(0, 8);
        put(0, 1);
        put(1, 1);
        put(1, 1);
        put(1, 5);
        put(1, 3);
        assert_eq!(pxa_code(&stream).unwrap(), b"ababab");
        stream[4] = 255;
        assert!(pxa_code(&stream).is_err());
    }
    #[test]
    fn text_code_comes_back_as_p8scii() {
        assert_eq!(encode_p8scii("btn(⬅️) ❎ █◝ ¹ x"), [b"btn(".as_slice(), &[139, b')', b' ', 151, b' ', 128, 255, b' ', 1, b' ', b'x']].concat());
    }
    #[test]
    fn omitted_sound_rows_use_default_speed() {
        let c = Cartridge::parse(&cart("")).unwrap();
        for id in 0..64 {
            assert_eq!(c.rom[0x3200 + id * 68 + 65], 16);
        }
    }
}
