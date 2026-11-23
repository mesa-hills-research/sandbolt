use std::fmt;
use std::hash::Hash;

const TAG_BASE: u64 = 0xfff8_0000_0000_0000;
const TAG_SHIFT: u32 = 47;
const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;
const INDEX_MASK: u64 = (1 << 31) - 1;
const AUTO_NATIVE_BIT: u64 = 1 << 46;
const AUTO_NATIVE_PREFIX: u64 = TAG_BASE | (5 << TAG_SHIFT) | AUTO_NATIVE_BIT;
const AUTO_NATIVE_MASK: u64 = 0xffff_c000_0000_0000;
pub(crate) const AUTO_NATIVE_MAX_REALM: u32 = (1 << 22) - 1;

pub trait ArenaId: Copy + Eq + Hash + fmt::Debug {
    fn from_parts(index: u32, generation: u16) -> Self;
    fn index(self) -> u32;
    fn generation(self) -> u16;
}

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_parts(index: u32, generation: u16) -> Self {
                assert!(index as u64 <= INDEX_MASK);
                Self(index as u64 | ((generation as u64) << 31))
            }

            pub const fn index(self) -> u32 {
                (self.0 & INDEX_MASK) as u32
            }

            pub const fn generation(self) -> u16 {
                (self.0 >> 31) as u16
            }
        }

        impl ArenaId for $name {
            fn from_parts(index: u32, generation: u16) -> Self {
                Self::from_parts(index, generation)
            }

            fn index(self) -> u32 { self.index() }
            fn generation(self) -> u16 { self.generation() }
        }
    )+};
}

ids!(
    ObjId,
    StrId,
    SymId,
    BigIntId,
    BcId,
    ModId,
    VarRefId,
    AccId,
    ShapeId,
    CoroId,
    RealmId,
    PersistentId
);

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct Value(u64);

impl Value {
    pub const NULL: Self = Self::tag(2, 0);
    pub const UNDEFINED: Self = Self::tag(3, 0);
    pub const UNINITIALIZED: Self = Self::tag(4, 0);
    pub const EMPTY: Self = Self::tag(5, 0);
    pub const SHORT_BIGINT_MIN: i64 = -(1 << 46);
    pub const SHORT_BIGINT_MAX: i64 = (1 << 46) - 1;

    const fn tag(tag: u64, payload: u64) -> Self {
        Self(TAG_BASE | (tag << TAG_SHIFT) | payload)
    }

    pub const fn number(value: f64) -> Self {
        Self(if value.is_nan() {
            0x7ff8_0000_0000_0000
        } else {
            value.to_bits()
        })
    }

    pub const fn int32(value: i32) -> Self {
        Self::tag(0, value as u32 as u64)
    }
    pub const fn bool(value: bool) -> Self {
        Self::tag(1, value as u64)
    }

    pub const fn short_bigint(value: i64) -> Option<Self> {
        if value < Self::SHORT_BIGINT_MIN || value > Self::SHORT_BIGINT_MAX {
            None
        } else {
            Some(Self::tag(6, value as u64 & PAYLOAD_MASK))
        }
    }

    pub const fn catch_offset(value: u32) -> Self {
        Self::tag(7, value as u64)
    }
    pub const fn object(value: ObjId) -> Self {
        Self::tag(8, value.0)
    }
    pub const fn string(value: StrId) -> Self {
        Self::tag(9, value.0)
    }
    pub const fn symbol(value: SymId) -> Self {
        Self::tag(10, value.0)
    }
    pub const fn bigint(value: BigIntId) -> Self {
        Self::tag(11, value.0)
    }
    pub const fn bytecode(value: BcId) -> Self {
        Self::tag(12, value.0)
    }
    pub const fn module(value: ModId) -> Self {
        Self::tag(13, value.0)
    }
    pub const fn var_ref(value: VarRefId) -> Self {
        Self::tag(14, value.0)
    }
    pub const fn accessor(value: AccId) -> Self {
        Self::tag(15, value.0)
    }
    /// Internal sentinel stored only in property slots for lazily materialized builtin methods.
    pub(crate) const fn auto_native(fn_id: u16, length: u8, realm_index: u32) -> Option<Self> {
        if realm_index > AUTO_NATIVE_MAX_REALM {
            return None;
        }
        let payload = AUTO_NATIVE_BIT
            | (fn_id as u64)
            | ((length as u64) << 16)
            | ((realm_index as u64) << 24);
        Some(Self::tag(5, payload))
    }
    #[inline]
    pub(crate) const fn is_auto_native(self) -> bool {
        (self.0 & AUTO_NATIVE_MASK) == AUTO_NATIVE_PREFIX
    }
    #[inline]
    pub(crate) const fn as_auto_native(self) -> Option<(u16, u8, u32)> {
        if !self.is_auto_native() {
            return None;
        }
        let payload = self.0 & (AUTO_NATIVE_BIT - 1);
        let fn_id = (payload & 0xffff) as u16;
        let length = ((payload >> 16) & 0xff) as u8;
        let realm_index = (payload >> 24) as u32;
        Some((fn_id, length, realm_index))
    }
    pub const fn bits(self) -> u64 {
        self.0
    }
    pub const fn is_number(self) -> bool {
        self.0 < 0xfff8_8000_0000_0000
    }
    pub const fn is_gc(self) -> bool {
        self.0 >= 0xfffc_0000_0000_0000
    }

    pub const fn as_number(self) -> Option<f64> {
        match self.decode() {
            ValueKind::Float64(n) => Some(n),
            ValueKind::Int32(n) => Some(n as f64),
            _ => None,
        }
    }

    pub const fn as_object(self) -> Option<ObjId> {
        if let ValueKind::Object(id) = self.decode() {
            Some(id)
        } else {
            None
        }
    }

    pub const fn decode(self) -> ValueKind {
        if self.0 < TAG_BASE {
            return ValueKind::Float64(f64::from_bits(self.0));
        }
        let data = self.0 & PAYLOAD_MASK;
        match ((self.0 >> TAG_SHIFT) & 15) as u8 {
            0 => ValueKind::Int32(data as u32 as i32),
            1 => ValueKind::Bool(data != 0),
            2 => ValueKind::Null,
            3 => ValueKind::Undefined,
            4 => ValueKind::Uninitialized,
            5 => ValueKind::Empty,
            6 => ValueKind::ShortBigInt(((data << 17) as i64) >> 17),
            7 => ValueKind::CatchOffset(data as u32),
            8 => ValueKind::Object(ObjId(data)),
            9 => ValueKind::String(StrId(data)),
            10 => ValueKind::Symbol(SymId(data)),
            11 => ValueKind::BigInt(BigIntId(data)),
            12 => ValueKind::Bytecode(BcId(data)),
            13 => ValueKind::Module(ModId(data)),
            14 => ValueKind::VarRef(VarRefId(data)),
            15 => ValueKind::Accessor(AccId(data)),
            _ => unreachable!(),
        }
    }
}

impl Default for Value {
    fn default() -> Self {
        Self::UNDEFINED
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.decode().fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueKind {
    Float64(f64),
    Int32(i32),
    Bool(bool),
    Null,
    Undefined,
    Uninitialized,
    Empty,
    ShortBigInt(i64),
    CatchOffset(u32),
    Object(ObjId),
    String(StrId),
    Symbol(SymId),
    BigInt(BigIntId),
    Bytecode(BcId),
    Module(ModId),
    VarRef(VarRefId),
    Accessor(AccId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Abrupt {
    Thrown,
    Interrupted,
}

pub type JsResult<T> = Result<T, Abrupt>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_bits_and_tag_boundaries() {
        assert_eq!(size_of::<Value>(), 8);
        for bits in [
            0,
            1,
            0x8000_0000_0000_0000,
            0x8000_0000_0000_0001,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
        ] {
            let value = Value::number(f64::from_bits(bits));
            assert_eq!(value.bits(), bits);
            assert!(value.is_number() && !value.is_gc());
        }
        for bits in [
            0x7ff0_0000_0000_0001,
            0xffff_ffff_ffff_ffff,
            0xfff8_0000_0000_0000,
        ] {
            assert_eq!(
                Value::number(f64::from_bits(bits)).bits(),
                0x7ff8_0000_0000_0000
            );
        }
        for n in [i32::MIN, -1, 0, i32::MAX] {
            assert_eq!(Value::int32(n).decode(), ValueKind::Int32(n));
            assert!(Value::int32(n).is_number());
        }
        assert!(!Value::bool(false).is_number());
        assert!(!Value::catch_offset(u32::MAX).is_gc());
        assert!(Value::object(ObjId::from_parts(0, 0)).is_gc());
        let id = AccId::from_parts((1 << 31) - 1, u16::MAX);
        assert_eq!(Value::accessor(id).bits(), u64::MAX);
        assert_eq!(Value::accessor(id).decode(), ValueKind::Accessor(id));
    }

    #[test]
    fn short_bigint_sign_extension() {
        for n in [Value::SHORT_BIGINT_MIN, -1, 0, Value::SHORT_BIGINT_MAX] {
            assert_eq!(
                Value::short_bigint(n).unwrap().decode(),
                ValueKind::ShortBigInt(n)
            );
        }
        assert_eq!(Value::short_bigint(Value::SHORT_BIGINT_MIN - 1), None);
        assert_eq!(Value::short_bigint(Value::SHORT_BIGINT_MAX + 1), None);
    }

    #[test]
    fn auto_native_sentinel_round_trip() {
        assert!(!Value::EMPTY.is_auto_native());
        assert_eq!(Value::EMPTY.as_auto_native(), None);
        for &(fn_id, length, realm) in &[
            (0u16, 0u8, 0u32),
            (0x1000, 1, 0),
            (0x1fff, 7, 1),
            (u16::MAX, u8::MAX, AUTO_NATIVE_MAX_REALM),
        ] {
            let v = Value::auto_native(fn_id, length, realm).unwrap();
            assert!(v.is_auto_native());
            assert!(!v.is_number());
            assert!(!v.is_gc());
            assert_ne!(v, Value::EMPTY);
            assert_eq!(v.as_auto_native(), Some((fn_id, length, realm)));
        }
        assert_eq!(Value::auto_native(0x1000, 0, AUTO_NATIVE_MAX_REALM + 1), None);
    }
}
