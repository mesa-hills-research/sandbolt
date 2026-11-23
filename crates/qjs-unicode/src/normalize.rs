use std::cmp::Ordering;

use crate::props::{byte, indexed_offset};
use crate::tables::{
    UNICODE_CC_INDEX, UNICODE_CC_TABLE, UNICODE_COMP_TABLE, UNICODE_DECOMP_DATA,
    UNICODE_DECOMP_TABLE1, UNICODE_DECOMP_TABLE2,
};
use crate::{UnicodeError, UnicodeNormalization};

const DECOMP_LEN_MAX: usize = 18;
const HANGUL: u32 = 0xac00;
const HANGUL_END: u32 = 0xd7a4;
const JAMO_LEADING: u32 = 0x1100;
const JAMO_VOWEL: u32 = 0x1161;
const JAMO_TRAILING: u32 = 0x11a7;
const JAMO_VOWEL_COUNT: u32 = 21;
const JAMO_TRAILING_COUNT: u32 = 28;
const JAMO_BLOCK: u32 = JAMO_VOWEL_COUNT * JAMO_TRAILING_COUNT;

fn short_code(c: u8) -> u32 {
    match c {
        0..=0x7f => c.into(),
        0x80..=0xcf => u32::from(c) - 0x80 + 0x300,
        0xd0 => 0x2044,
        0xd1 => 0x2215,
        _ => unreachable!("invalid short code in decomposition table"),
    }
}

fn le16(data: &[u8], offset: usize) -> u32 {
    u16::from_le_bytes([data[offset], data[offset + 1]]).into()
}

#[derive(Clone, Copy)]
struct Entry(u32);

impl Entry {
    fn start(self) -> u32 {
        self.0 >> 14
    }

    fn len(self) -> usize {
        ((self.0 >> 7) & 0x7f) as usize
    }

    fn kind(self) -> usize {
        ((self.0 >> 1) & 0x3f) as usize
    }

    fn is_compat(self) -> bool {
        self.0 & 1 != 0
    }

    fn expand(self, result: &mut [u32; DECOMP_LEN_MAX], idx: usize, offset: usize) -> usize {
        let base = usize::from(UNICODE_DECOMP_TABLE2[idx]);
        let kind = self.kind();
        if kind == 0 {
            result[0] = base as u32;
            return 1;
        }
        let data = &UNICODE_DECOMP_DATA[base..];
        match kind {
            // Arrays of 16-bit, 18-bit, or short code points. A zero marks a hole.
            1..=14 => {
                let len = match kind {
                    1..=7 => kind,
                    8..=9 => kind - 7,
                    _ => kind - 9,
                };
                let upper = self.len() * len * 2;
                for (i, dst) in result[..len].iter_mut().enumerate() {
                    let k = offset * len + i;
                    *dst = match kind {
                        1..=7 => le16(data, 2 * k),
                        8..=9 => {
                            le16(data, 2 * k)
                                | (u32::from((data[upper + k / 4] >> (2 * (k % 4))) & 3) << 16)
                        }
                        _ => short_code(data[k]),
                    };
                    if *dst == 0 {
                        return 0;
                    }
                }
                len
            }
            // A fixed sequence with one code point incremented by the offset.
            15..=21 => {
                let (len, pos) = if kind == 15 {
                    (1, 0)
                } else {
                    let len = 2 + ((kind - 16) >> 1);
                    (len, ((kind - 16) & 1) + usize::from(len > 2))
                };
                for (i, dst) in result[..len].iter_mut().enumerate() {
                    *dst = le16(data, 2 * i) + if i == pos { offset as u32 } else { 0 };
                }
                len
            }
            // Bytes relative to a 16-bit base, with 0xff reserved for space.
            22..=30 => {
                let len = if kind == 30 { 18 } else { kind - 21 };
                let base = le16(data, 0);
                let data = &data[2 + offset * len..];
                for (dst, &b) in result[..len].iter_mut().zip(data) {
                    *dst = if b == 0xff { 0x20 } else { base + u32::from(b) };
                }
                len
            }
            31 => {
                let pos = 3 * offset;
                result[0] = le16(data, pos);
                if result[0] == 0 {
                    return 0;
                }
                result[1] = short_code(data[pos + 2]);
                2
            }
            32 => {
                result[0] = le16(data, 0);
                result[1] = le16(data, 4 + 2 * offset);
                result[2] = le16(data, 2);
                3
            }
            33 | 34 => {
                // The same entry encodes both upper and lower case.
                let (mut c, pos) = if kind == 33 {
                    let p = offset & !1;
                    (short_code(data[p]), p + 1)
                } else {
                    let p = 3 * (offset >> 1);
                    (le16(data, p), p + 2)
                };
                if offset & 1 != 0 {
                    c += if c < 0x100 || (0x410..=0x42f).contains(&c) {
                        0x20
                    } else {
                        1
                    };
                }
                result[0] = c;
                result[1] = short_code(data[pos]);
                2
            }
            _ => unreachable!("invalid decomposition kind"),
        }
    }
}

fn decompose_char(c: u32, compat: bool, result: &mut [u32; DECOMP_LEN_MAX]) -> usize {
    let idx = UNICODE_DECOMP_TABLE1.binary_search_by(|&entry| {
        let entry = Entry(entry);
        if c < entry.start() {
            Ordering::Greater
        } else if c >= entry.start() + entry.len() as u32 {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    });
    let Ok(idx) = idx else {
        return 0;
    };
    let entry = Entry(UNICODE_DECOMP_TABLE1[idx]);
    if !compat && entry.is_compat() {
        return 0;
    }
    entry.expand(result, idx, (c - entry.start()) as usize)
}

fn ccc(c: u32) -> u32 {
    let Some((mut start, pos)) = indexed_offset(c, &UNICODE_CC_INDEX) else {
        return 0;
    };
    let mut data = &UNICODE_CC_TABLE[pos..];
    loop {
        let b = byte(&mut data);
        let kind = b >> 6;
        let len = match b & 0x3f {
            n @ 0..=47 => n,
            n @ 48..=55 => 48 + (((n - 48) << 8) | byte(&mut data)),
            n => 48 + (1 << 11) + (((n - 56) << 8) | (byte(&mut data) << 8) | byte(&mut data)),
        } + 1;
        let class = match kind {
            0 | 1 => byte(&mut data),
            2 => 0,
            _ => 230,
        };
        let end = start + len;
        if c < end {
            return class + if kind == 1 { c - start } else { 0 };
        }
        start = end;
    }
}

fn sort_cc(buf: &mut [u32]) {
    let mut i = 0;
    while i < buf.len() {
        if ccc(buf[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < buf.len() {
            let c = buf[i];
            let class = ccc(c);
            if class == 0 {
                break;
            }
            let mut j = i;
            while j > start && class < ccc(buf[j - 1]) {
                buf[j] = buf[j - 1];
                j -= 1;
            }
            buf[j] = c;
            i += 1;
        }
        i += 1;
    }
}

fn decompose(buf: &mut Vec<u32>, src: &[u32], compat: bool) -> Result<(), UnicodeError> {
    let mut result = [0; DECOMP_LEN_MAX];
    for &c in src {
        if (HANGUL..HANGUL_END).contains(&c) {
            let offset = c - HANGUL;
            let trailing = offset % JAMO_TRAILING_COUNT;
            buf.try_reserve(2 + usize::from(trailing != 0))?;
            buf.push(JAMO_LEADING + offset / JAMO_BLOCK);
            buf.push(JAMO_VOWEL + (offset % JAMO_BLOCK) / JAMO_TRAILING_COUNT);
            if trailing != 0 {
                buf.push(JAMO_TRAILING + trailing);
            }
        } else {
            let len = decompose_char(c, compat, &mut result);
            if len != 0 {
                decompose(buf, &result[..len], compat)?;
            } else {
                buf.try_reserve(1)?;
                buf.push(c);
            }
        }
    }
    Ok(())
}

fn compose_pair(c0: u32, c1: u32) -> Option<u32> {
    if (JAMO_LEADING..JAMO_LEADING + 19).contains(&c0)
        && (JAMO_VOWEL..JAMO_VOWEL + JAMO_VOWEL_COUNT).contains(&c1)
    {
        return Some(
            HANGUL + (c0 - JAMO_LEADING) * JAMO_BLOCK + (c1 - JAMO_VOWEL) * JAMO_TRAILING_COUNT,
        );
    }
    if (HANGUL..HANGUL_END).contains(&c0)
        && (c0 - HANGUL).is_multiple_of(JAMO_TRAILING_COUNT)
        && (JAMO_TRAILING..JAMO_TRAILING + JAMO_TRAILING_COUNT).contains(&c1)
    {
        return Some(c0 + c1 - JAMO_TRAILING);
    }
    let mut result = [0; DECOMP_LEN_MAX];
    let mut composed = None;
    UNICODE_COMP_TABLE
        .binary_search_by(|&packed| {
            let idx = usize::from(packed >> 6);
            let offset = usize::from(packed & 0x3f);
            let entry = Entry(UNICODE_DECOMP_TABLE1[idx]);
            entry.expand(&mut result, idx, offset);
            let cmp = (c0.wrapping_sub(result[0]) as i32)
                .cmp(&0)
                .then_with(|| (c1.wrapping_sub(result[1]) as i32).cmp(&0));
            if cmp == Ordering::Equal {
                composed = Some(entry.start() + offset as u32);
            }
            cmp.reverse()
        })
        .ok()?;
    composed
}

fn compose(buf: &mut Vec<u32>) {
    let mut out_len = 1;
    'next: for i in 1..buf.len() {
        let c = buf[i];
        let mut last_class = ccc(c);
        for j in (0..out_len).rev() {
            let class = ccc(buf[j]);
            if class == 0 {
                if let Some(composed) = compose_pair(buf[j], c) {
                    buf[j] = composed;
                    continue 'next;
                }
                break;
            }
            if class >= last_class {
                break;
            }
            last_class = 256;
        }
        buf[out_len] = c;
        out_len += 1;
    }
    buf.truncate(out_len);
}

/// Normalize code points, preserving values such as unpaired UTF-16 surrogates.
pub fn normalize(src: &[u32], form: UnicodeNormalization) -> Result<Vec<u32>, UnicodeError> {
    use UnicodeNormalization::{Nfc, Nfd, Nfkc, Nfkd};
    let mut buf = Vec::new();
    buf.try_reserve_exact(src.len())?;
    if form == Nfc && src.iter().all(|&c| c < 0x100) {
        buf.extend_from_slice(src);
        return Ok(buf);
    }
    decompose(&mut buf, src, matches!(form, Nfkc | Nfkd))?;
    sort_cc(&mut buf);
    if buf.len() > 1 && !matches!(form, Nfd | Nfkd) {
        compose(&mut buf);
    }
    Ok(buf)
}
