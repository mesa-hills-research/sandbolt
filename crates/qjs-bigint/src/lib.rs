//! Pure-Rust port of QuickJS-ng v0.17.0 `JSBigInt` (arbitrary-precision
//! integers with resource charging and interrupt polling).

use std::cmp::Ordering;

/// Matches QuickJS-ng `JS_BIGINT_MAX_SIZE` (1,048,576 bits).
pub const MAX_BITS: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BigIntError {
    OutOfMemory,
    Interrupted,
    RangeError(&'static str),
    DivisionByZero,
    NegativeExponent,
    InvalidString,
}

/// Host accounting and interrupt interface for BigInt operations.
pub trait BigIntHost {
    fn charge(&mut self, bytes: usize) -> Result<(), BigIntError>;
    fn release(&mut self, bytes: usize);
    fn poll_interrupt(&mut self) -> bool;
}

/// Default unlimited host for standalone tests.
#[derive(Default)]
pub struct UnlimitedBigIntHost;

impl BigIntHost for UnlimitedBigIntHost {
    fn charge(&mut self, _bytes: usize) -> Result<(), BigIntError> {
        Ok(())
    }
    fn release(&mut self, _bytes: usize) {}
    fn poll_interrupt(&mut self) -> bool {
        false
    }
}

/// Two's-complement limb representation matching QuickJS-ng's `JSBigInt`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BigInt {
    /// Two's-complement little-endian 64-bit limbs, always normalized to minimum length >= 1.
    limbs: Vec<u64>,
}

impl BigInt {
    pub fn zero() -> Self {
        Self { limbs: vec![0] }
    }

    pub fn from_i64(v: i64) -> Self {
        Self {
            limbs: vec![v as u64],
        }
    }

    pub fn from_u64(v: u64) -> Self {
        if (v as i64) >= 0 {
            Self { limbs: vec![v] }
        } else {
            Self { limbs: vec![v, 0] }
        }
    }

    pub fn limbs(&self) -> &[u64] {
        &self.limbs
    }

    pub fn byte_size(&self) -> usize {
        self.limbs.capacity() * std::mem::size_of::<u64>()
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.len() == 1 && self.limbs[0] == 0
    }

    pub fn is_negative(&self) -> bool {
        (*self.limbs.last().unwrap_or(&0) as i64) < 0
    }

    pub fn to_i64_exact(&self) -> Option<i64> {
        if self.limbs.len() == 1 {
            Some(self.limbs[0] as i64)
        } else {
            None
        }
    }

    pub fn from_f64_trunc(_v: f64, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn to_f64(&self) -> f64 {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn cmp_f64(&self, _d: f64) -> Option<Ordering> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn cmp_bigint(&self, _other: &Self) -> Ordering {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn parse_radix(
        _s: &str,
        _radix: u32,
        _host: &mut dyn BigIntHost,
    ) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn to_string_radix(
        &self,
        _radix: u32,
        _host: &mut dyn BigIntHost,
    ) -> Result<String, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn neg(&self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn bitnot(&self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn add(&self, _other: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn sub(&self, _other: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn mul(&self, _other: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn divrem(
        &self,
        _other: &Self,
        _host: &mut dyn BigIntHost,
    ) -> Result<(Self, Self), BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn pow(&self, _exp: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn shl(&self, _shift: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn sar(&self, _shift: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn bitand(&self, _other: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn bitor(&self, _other: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn bitxor(&self, _other: &Self, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn as_int_n(&self, _bits: u64, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn as_uint_n(&self, _bits: u64, _host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        todo!("implemented by qjs-bigint worker")
    }

    pub fn to_i64_wrapping(&self) -> i64 {
        self.limbs[0] as i64
    }

    pub fn to_u64_wrapping(&self) -> u64 {
        self.limbs[0]
    }
}
