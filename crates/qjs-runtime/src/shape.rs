use std::ops::{BitOr, BitOrAssign};

use crate::arena::ArenaItem;
use crate::atom::PropKey;
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::value::{ObjId, ShapeId};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PropFlags(u8);

impl PropFlags {
    pub const WRITABLE: Self = Self(1);
    pub const ENUMERABLE: Self = Self(2);
    pub const CONFIGURABLE: Self = Self(4);
    pub const ACCESSOR: Self = Self(8);
    pub const DEFAULT_DATA: Self = Self(7);

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub fn set(&mut self, flag: Self, on: bool) {
        if on {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

impl BitOr for PropFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for PropFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShapeProp {
    pub key: PropKey,
    pub flags: PropFlags,
}

#[derive(Debug)]
pub struct Shape {
    pub proto: Option<ObjId>,
    pub props: TrackedVec<ShapeProp>,
    pub dictionary: bool,
    pub(crate) churn: u8,
    lookup: TrackedVec<u32>,
    pub(crate) transitions: TrackedVec<(ShapeProp, ShapeId)>,
}

impl ArenaItem for Shape {
    type Id = ShapeId;
}
impl Trace for Shape {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.proto.trace(tracer);
        for prop in self.props.iter() {
            prop.key.trace(tracer);
        }
    }
}

impl Shape {
    pub const fn empty(proto: Option<ObjId>) -> Self {
        Self {
            proto,
            props: TrackedVec::new(),
            dictionary: false,
            churn: 0,
            lookup: TrackedVec::new(),
            transitions: TrackedVec::new(),
        }
    }

    pub fn find(&self, key: PropKey) -> Option<(usize, PropFlags)> {
        if self.lookup.is_empty() {
            return None;
        }
        let mask = self.lookup.len() - 1;
        let mut pos = hash(key) & mask;
        loop {
            let entry = self.lookup[pos].checked_sub(1)? as usize;
            let prop = self.props[entry];
            if prop.key == key {
                return Some((entry, prop.flags));
            }
            pos = (pos + 1) & mask;
        }
    }

    pub fn build(
        ledger: &HeapLedger,
        proto: Option<ObjId>,
        count: usize,
        props: impl Iterator<Item = ShapeProp>,
        churn: u8,
        dictionary: bool,
    ) -> AllocResult<Self> {
        let mut shape = Self::empty(proto);
        shape.dictionary = dictionary || churn >= 8 || count > 1024;
        shape.churn = churn;
        if count == 0 {
            return Ok(shape);
        }
        let len = count
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .filter(|_| u32::try_from(count).is_ok())
            .ok_or(AllocError::Limit)?;
        shape.props.try_reserve_exact(ledger, count)?;
        shape.lookup.try_reserve_exact(ledger, len)?;
        for _ in 0..len {
            shape.lookup.push_reserved(0);
        }
        for prop in props {
            let mut pos = hash(prop.key) & (len - 1);
            while shape.lookup[pos] != 0 {
                pos = (pos + 1) & (len - 1);
            }
            shape.lookup[pos] = shape.props.len() as u32 + 1;
            shape.props.push_reserved(prop);
        }
        Ok(shape)
    }
}

fn hash(key: PropKey) -> usize {
    let bits = key.bits();
    ((bits ^ (bits >> 16)).wrapping_mul(0x9e37_79b9) ^ (bits >> 8)) as usize
}
