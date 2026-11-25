/// Why an escape could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeError {
    Malformed,
    Unrecognized,
}

/// Decode an escape starting after the backslash and return its code point
/// and byte count. Mode 0 accepts legacy escapes. Mode 1 also accepts
/// `\u{…}`. Mode 2 combines escaped surrogate pairs and rejects octal escapes.
pub fn parse_escape(bytes: &[u8], allow_utf16: u8) -> Result<(u32, usize), EscapeError> {
    use EscapeError::{Malformed, Unrecognized};
    let c = *bytes.first().ok_or(Unrecognized)?;
    let mut pos = 1;
    let value = match c {
        b'b' => 8,
        b'f' => 12,
        b'n' => 10,
        b'r' => 13,
        b't' => 9,
        b'v' => 11,
        b'x' => {
            pos += 2;
            fixed_hex(bytes.get(1..pos).ok_or(Malformed)?).ok_or(Malformed)?
        }
        b'u' if allow_utf16 != 0 && bytes.get(pos) == Some(&b'{') => {
            pos += 1;
            let mut value = 0;
            loop {
                let digit = hex(*bytes.get(pos).ok_or(Malformed)?).ok_or(Malformed)?;
                value = (value << 4) | digit;
                if value > 0x10ffff {
                    return Err(Malformed);
                }
                pos += 1;
                if bytes.get(pos) == Some(&b'}') {
                    pos += 1;
                    break value;
                }
            }
        }
        b'u' => {
            pos += 4;
            let high = fixed_hex(bytes.get(1..pos).ok_or(Malformed)?).ok_or(Malformed)?;
            if allow_utf16 == 2
                && is_high(high)
                && bytes.get(pos..pos + 2) == Some(b"\\u")
                && let Some(low) = bytes
                    .get(pos + 2..pos + 6)
                    .and_then(fixed_hex)
                    .filter(|&c| is_low(c))
            {
                return Ok((join_surrogates(high, low), pos + 6));
            }
            high
        }
        b'0'..=b'7' => {
            if allow_utf16 == 2 {
                if c != b'0' || bytes.get(pos).is_some_and(u8::is_ascii_digit) {
                    return Err(Malformed);
                }
                0
            } else {
                let mut value = u32::from(c - b'0');
                while pos < 3 && value < 32 {
                    match bytes.get(pos) {
                        Some(d @ b'0'..=b'7') => value = (value << 3) | u32::from(*d - b'0'),
                        _ => break,
                    }
                    pos += 1;
                }
                value
            }
        }
        _ => return Err(Unrecognized),
    };
    Ok((value, pos))
}

fn hex(byte: u8) -> Option<u32> {
    (byte as char).to_digit(16)
}

fn fixed_hex(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0, |n, &b| Some((n << 4) | hex(b)?))
}

pub(crate) fn is_high(c: u32) -> bool {
    (0xd800..=0xdbff).contains(&c)
}

pub(crate) fn is_low(c: u32) -> bool {
    (0xdc00..=0xdfff).contains(&c)
}

pub(crate) fn join_surrogates(high: u32, low: u32) -> u32 {
    0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
}

pub(crate) fn wtf8(bytes: &[u8]) -> (u32, usize) {
    let Some(&first) = bytes.first() else {
        return (0xfffd, 0);
    };
    let (len, mut value, low, high) = match first {
        0..=0x7f => return (u32::from(first), 1),
        0xc2..=0xdf => (2, first & 0x1f, 0x80, 0xbf),
        0xe0 => (3, 0, 0xa0, 0xbf),
        0xe1..=0xef => (3, first & 0x0f, 0x80, 0xbf),
        0xf0 => (4, 0, 0x90, 0xbf),
        0xf1..=0xf3 => (4, first & 7, 0x80, 0xbf),
        0xf4 => (4, 4, 0x80, 0x8f),
        _ => return (0xfffd, 1),
    };
    let Some(rest) = bytes.get(1..len) else {
        return (0xfffd, 1);
    };
    if !(low..=high).contains(&rest[0]) || !rest[1..].iter().all(|b| (0x80..=0xbf).contains(b)) {
        return (0xfffd, 1);
    }
    let mut point = u32::from(value);
    for &b in rest {
        value = b & 0x3f;
        point = (point << 6) | u32::from(value);
    }
    (point, len)
}
