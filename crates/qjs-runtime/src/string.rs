use crate::arena::{Arena, ArenaItem};
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::value::StrId;

pub const MAX_ROPE_DEPTH: u8 = 32;
pub(crate) const INLINE_LATIN1_CAPACITY: usize = 30;
pub(crate) const INLINE_UTF16_CAPACITY: usize = 15;

#[derive(Debug)]
pub enum JsString {
    Latin1 {
        data: TrackedVec<u8>,
        is_ascii: bool,
    },
    InlineLatin1 {
        len: u8,
        is_ascii: bool,
        data: [u8; INLINE_LATIN1_CAPACITY],
    },
    Utf16(TrackedVec<u16>),
    InlineUtf16 {
        len: u8,
        data: [u16; INLINE_UTF16_CAPACITY],
    },
    Slice {
        base: StrId,
        start: u32,
        len: u32,
        is_wide: bool,
    },
    Rope {
        left: StrId,
        right: StrId,
        len: u32,
        is_wide: bool,
        depth: u8,
    },
}

impl ArenaItem for JsString {
    type Id = StrId;
}

impl Trace for JsString {
    fn trace(&self, tracer: &Tracer<'_>) {
        match self {
            Self::Rope { left, right, .. } => {
                left.trace(tracer);
                right.trace(tracer);
            }
            Self::Slice { base, .. } => base.trace(tracer),
            Self::Latin1 { .. }
            | Self::InlineLatin1 { .. }
            | Self::Utf16(_)
            | Self::InlineUtf16 { .. } => {}
        }
    }
}

impl JsString {
    pub fn from_str(ledger: &HeapLedger, text: &str) -> AllocResult<Self> {
        if text.is_ascii() {
            return Self::from_latin1(ledger, text.as_bytes());
        }
        if text.chars().all(|c| c <= '\u{ff}') {
            Self::from_units(
                ledger,
                text.chars().count(),
                false,
                text.chars().map(|c| c as u16),
            )
        } else {
            Self::from_units(
                ledger,
                text.encode_utf16().count(),
                true,
                text.encode_utf16(),
            )
        }
    }

    pub fn from_latin1(ledger: &HeapLedger, bytes: &[u8]) -> AllocResult<Self> {
        u32::try_from(bytes.len()).map_err(|_| AllocError::Limit)?;
        let is_ascii = bytes.is_ascii();
        if bytes.len() <= INLINE_LATIN1_CAPACITY {
            let mut data = [0; INLINE_LATIN1_CAPACITY];
            data[..bytes.len()].copy_from_slice(bytes);
            Ok(Self::InlineLatin1 {
                len: bytes.len() as u8,
                is_ascii,
                data,
            })
        } else {
            Ok(Self::Latin1 {
                data: TrackedVec::from_slice(ledger, bytes)?,
                is_ascii,
            })
        }
    }

    pub fn from_utf16(ledger: &HeapLedger, units: &[u16]) -> AllocResult<Self> {
        Self::from_units(
            ledger,
            units.len(),
            units.iter().any(|&u| u > 255),
            units.iter().copied(),
        )
    }

    pub(crate) fn from_units(
        ledger: &HeapLedger,
        len: usize,
        wide: bool,
        units: impl Iterator<Item = u16>,
    ) -> AllocResult<Self> {
        u32::try_from(len).map_err(|_| AllocError::Limit)?;
        if wide && len <= INLINE_UTF16_CAPACITY {
            let mut data = [0; INLINE_UTF16_CAPACITY];
            for (i, unit) in units.enumerate() {
                data[i] = unit;
            }
            Ok(Self::InlineUtf16 {
                len: len as u8,
                data,
            })
        } else if wide {
            let mut data = TrackedVec::new();
            data.try_reserve_exact(ledger, len)?;
            for unit in units {
                data.push_reserved(unit);
            }
            Ok(Self::Utf16(data))
        } else {
            Self::from_latin1_iter(
                ledger,
                len,
                units.map(|unit| u8::try_from(unit).expect("wide code unit in Latin-1 string")),
            )
        }
    }

    pub(crate) fn from_latin1_iter(
        ledger: &HeapLedger,
        len: usize,
        bytes: impl Iterator<Item = u8>,
    ) -> AllocResult<Self> {
        u32::try_from(len).map_err(|_| AllocError::Limit)?;
        if len <= INLINE_LATIN1_CAPACITY {
            let mut data = [0; INLINE_LATIN1_CAPACITY];
            for (i, byte) in bytes.enumerate() {
                data[i] = byte;
            }
            Ok(Self::InlineLatin1 {
                len: len as u8,
                is_ascii: data[..len].is_ascii(),
                data,
            })
        } else {
            let mut data = TrackedVec::new();
            data.try_reserve_exact(ledger, len)?;
            let mut is_ascii = true;
            for byte in bytes {
                is_ascii &= byte.is_ascii();
                data.push_reserved(byte);
            }
            Ok(Self::Latin1 { data, is_ascii })
        }
    }

    pub fn len(&self) -> u32 {
        match self {
            Self::Latin1 { data, .. } => data.len() as u32,
            Self::InlineLatin1 { len, .. } | Self::InlineUtf16 { len, .. } => u32::from(*len),
            Self::Utf16(data) => data.len() as u32,
            Self::Rope { len, .. } | Self::Slice { len, .. } => *len,
        }
    }

    pub fn as_latin1(&self) -> Option<&[u8]> {
        match self {
            Self::Latin1 { data, .. } => Some(data),
            Self::InlineLatin1 { len, data, .. } => Some(&data[..usize::from(*len)]),
            _ => None,
        }
    }

    pub fn as_utf16(&self) -> Option<&[u16]> {
        match self {
            Self::Utf16(data) => Some(data),
            Self::InlineUtf16 { len, data } => Some(&data[..usize::from(*len)]),
            _ => None,
        }
    }

    /// Reports whether this is a flat ASCII string stored in Latin-1.
    pub fn is_ascii(&self) -> bool {
        match self {
            Self::Latin1 { is_ascii, .. } | Self::InlineLatin1 { is_ascii, .. } => *is_ascii,
            _ => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn is_wide(&self) -> bool {
        match self {
            Self::Latin1 { .. } | Self::InlineLatin1 { .. } => false,
            Self::Utf16(_) | Self::InlineUtf16 { .. } => true,
            Self::Rope { is_wide, .. } | Self::Slice { is_wide, .. } => *is_wide,
        }
    }
    pub fn depth(&self) -> u8 {
        match self {
            Self::Rope { depth, .. } => *depth,
            Self::Slice { .. } => 1,
            _ => 0,
        }
    }

    pub(crate) fn unit_at(arena: &Arena<Self>, mut id: StrId, mut index: u32) -> Option<u16> {
        loop {
            match arena.get(id).expect("missing string") {
                Self::Latin1 { data, .. } => {
                    return data.get(index as usize).copied().map(u16::from);
                }
                Self::InlineLatin1 { data, len, .. } => {
                    return data[..usize::from(*len)]
                        .get(index as usize)
                        .copied()
                        .map(u16::from);
                }
                Self::Utf16(data) => return data.get(index as usize).copied(),
                Self::InlineUtf16 { len, data } => {
                    return data[..usize::from(*len)].get(index as usize).copied();
                }
                Self::Slice {
                    base, start, len, ..
                } => {
                    if index >= *len {
                        return None;
                    }
                    id = *base;
                    index += *start;
                }
                Self::Rope { left, right, .. } => {
                    let left_len = arena.get(*left).expect("missing string").len();
                    if index < left_len {
                        id = *left;
                    } else {
                        id = *right;
                        index -= left_len;
                    }
                }
            }
        }
    }
}

pub struct StringUnits<'a> {
    arena: &'a Arena<JsString>,
    pending: [Option<StrId>; MAX_ROPE_DEPTH as usize + 1],
    count: usize,
    leaf: Leaf<'a>,
}

enum Leaf<'a> {
    Empty,
    Latin1(std::slice::Iter<'a, u8>),
    Utf16(std::slice::Iter<'a, u16>),
}

impl<'a> Leaf<'a> {
    fn flat(s: &'a JsString, start: usize, len: usize) -> Self {
        let range = start..start + len;
        if let Some(bytes) = s.as_latin1() {
            Self::Latin1(bytes[range].iter())
        } else {
            Self::Utf16(s.as_utf16().unwrap()[range].iter())
        }
    }
}

impl<'a> StringUnits<'a> {
    pub fn new(arena: &'a Arena<JsString>, id: StrId) -> Self {
        let mut result = Self {
            arena,
            pending: [None; MAX_ROPE_DEPTH as usize + 1],
            count: 0,
            leaf: Leaf::Empty,
        };
        result.push(id);
        result
    }

    fn push(&mut self, id: StrId) {
        self.pending[self.count] = Some(id);
        self.count += 1;
    }
}

impl Iterator for StringUnits<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next = match &mut self.leaf {
                Leaf::Empty => None,
                Leaf::Latin1(iter) => iter.next().map(|&b| u16::from(b)),
                Leaf::Utf16(iter) => iter.next().copied(),
            };
            if next.is_some() {
                return next;
            }
            self.count = self.count.checked_sub(1)?;
            let id = self.pending[self.count].take().unwrap();
            match self.arena.get(id).expect("missing string") {
                JsString::Slice {
                    base, start, len, ..
                } => {
                    let base = self.arena.get(*base).expect("missing string slice base");
                    self.leaf = Leaf::flat(base, *start as usize, *len as usize);
                }
                JsString::Rope { left, right, .. } => {
                    self.push(*right);
                    self.push(*left);
                }
                flat => self.leaf = Leaf::flat(flat, 0, flat.len() as usize),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_latin1_fits_without_a_buffer_charge() {
        assert!(size_of::<JsString>() <= 40);
        let ledger = HeapLedger::new(4096, 0).unwrap();
        let baseline = ledger.used_bytes();
        let small = [0xff; INLINE_LATIN1_CAPACITY];
        let string = JsString::from_latin1(&ledger, &small).unwrap();
        assert_eq!(string.as_latin1(), Some(small.as_slice()));
        assert!(!string.is_ascii());
        assert_eq!(ledger.used_bytes(), baseline);

        let wide = [0xd800; INLINE_UTF16_CAPACITY];
        let string = JsString::from_utf16(&ledger, &wide).unwrap();
        assert_eq!(string.as_utf16(), Some(wide.as_slice()));
        assert_eq!(ledger.used_bytes(), baseline);

        let large = [b'x'; INLINE_LATIN1_CAPACITY + 1];
        let string = JsString::from_latin1(&ledger, &large).unwrap();
        assert_eq!(string.as_latin1(), Some(large.as_slice()));
        assert!(string.is_ascii());
        assert_eq!(ledger.used_bytes() - baseline, large.len());
        drop(string);
        assert_eq!(ledger.used_bytes(), baseline);
    }
}
