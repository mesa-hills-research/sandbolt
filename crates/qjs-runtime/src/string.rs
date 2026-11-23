use crate::arena::{Arena, ArenaItem};
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::value::StrId;

pub const MAX_ROPE_DEPTH: u8 = 32;

#[derive(Debug)]
pub enum JsString {
    Latin1 {
        data: TrackedVec<u8>,
        is_ascii: bool,
    },
    Utf16(TrackedVec<u16>),
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
            Self::Latin1 { .. } | Self::Utf16(_) => {}
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
        Ok(Self::Latin1 {
            data: TrackedVec::from_slice(ledger, bytes)?,
            is_ascii: bytes.is_ascii(),
        })
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
        if wide {
            let mut data = TrackedVec::new();
            data.try_reserve_exact(ledger, len)?;
            for unit in units {
                data.push_reserved(unit);
            }
            Ok(Self::Utf16(data))
        } else {
            let mut data = TrackedVec::new();
            data.try_reserve_exact(ledger, len)?;
            let mut is_ascii = true;
            for unit in units {
                is_ascii &= unit <= 127;
                data.push_reserved(u8::try_from(unit).expect("wide code unit in Latin-1 string"));
            }
            Ok(Self::Latin1 { data, is_ascii })
        }
    }

    pub fn len(&self) -> u32 {
        match self {
            Self::Latin1 { data, .. } => data.len() as u32,
            Self::Utf16(data) => data.len() as u32,
            Self::Rope { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn is_wide(&self) -> bool {
        match self {
            Self::Latin1 { .. } => false,
            Self::Utf16(_) => true,
            Self::Rope { is_wide, .. } => *is_wide,
        }
    }
    pub fn depth(&self) -> u8 {
        match self {
            Self::Rope { depth, .. } => *depth,
            _ => 0,
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
                JsString::Latin1 { data, .. } => self.leaf = Leaf::Latin1(data.iter()),
                JsString::Utf16(data) => self.leaf = Leaf::Utf16(data.iter()),
                JsString::Rope { left, right, .. } => {
                    self.push(*right);
                    self.push(*left);
                }
            }
        }
    }
}
