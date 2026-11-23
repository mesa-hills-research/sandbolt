/// A half-open range in the source's byte encoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
}

/// UTF-8 for file input, or CESU-8 for UTF-16 input. Both retain the original UTF-16 units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceText {
    pub filename: String,
    pub units: Vec<u16>,
    pub line_starts: Vec<u32>,
    bytes: Vec<u8>,
}

impl SourceText {
    pub fn from_utf8(filename: impl Into<String>, src: &str) -> Self {
        Self::new(
            filename.into(),
            src.encode_utf16().collect(),
            src.as_bytes().to_vec(),
        )
    }

    pub fn from_utf16(filename: impl Into<String>, units: Vec<u16>) -> Self {
        let mut bytes = Vec::with_capacity(units.len());
        for &unit in &units {
            match unit {
                0..=0x7f => bytes.push(unit as u8),
                0x80..=0x7ff => {
                    bytes.extend([(0xc0 | (unit >> 6)) as u8, (0x80 | (unit & 63)) as u8])
                }
                _ => bytes.extend([
                    (0xe0 | (unit >> 12)) as u8,
                    (0x80 | ((unit >> 6) & 63)) as u8,
                    (0x80 | (unit & 63)) as u8,
                ]),
            }
        }
        Self::new(filename.into(), units, bytes)
    }

    fn new(filename: String, units: Vec<u16>, bytes: Vec<u8>) -> Self {
        let mut line_starts = vec![0];
        let mut i = 0;
        while i < bytes.len() {
            let (c, n) = decode(&bytes, i);
            i += n;
            if c == 13 && bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
            if is_line(c) {
                line_starts.push(i as u32);
            }
        }
        Self {
            filename,
            units,
            line_starts,
            bytes,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Exact bytes, including line endings and encoded lone surrogates.
    pub fn slice(&self, span: Span) -> &[u8] {
        &self.bytes[span.start as usize..span.end as usize]
    }

    /// Decode a byte span back to its original UTF-16 units.
    pub fn slice_utf16(&self, span: Span) -> Vec<u16> {
        let bytes = self.slice(span);
        let mut units = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let (c, n) = decode(bytes, i);
            push_utf16(&mut units, c);
            i += n;
        }
        units
    }

    /// One-based line and byte column.
    pub fn line_col(&self, byte_offset: u32) -> (u32, u32) {
        let idx = self
            .line_starts
            .partition_point(|&start| start <= byte_offset)
            .saturating_sub(1);
        (
            idx as u32 + 1,
            byte_offset.saturating_sub(self.line_starts[idx]) + 1,
        )
    }
}

pub(crate) fn is_line(c: u32) -> bool {
    matches!(c, 10 | 13 | 0x2028 | 0x2029)
}

pub(crate) fn decode(bytes: &[u8], at: usize) -> (u32, usize) {
    let b = bytes[at];
    let (mut value, n) = match b {
        0..=0x7f => return (u32::from(b), 1),
        0xc0..=0xdf => (u32::from(b & 31), 2),
        0xe0..=0xef => (u32::from(b & 15), 3),
        _ => (u32::from(b & 7), 4),
    };
    for &b in &bytes[at + 1..at + n] {
        value = (value << 6) | u32::from(b & 63);
    }
    (value, n)
}

pub(crate) fn push_utf16(out: &mut Vec<u16>, c: u32) {
    if c <= 0xffff {
        out.push(c as u16);
    } else {
        out.push((0xd800 + ((c - 0x10000) >> 10)) as u16);
        out.push((0xdc00 + ((c - 0x10000) & 1023)) as u16);
    }
}
