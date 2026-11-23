//! BigInt arithmetic and conversions for QuickJS-ng values.

#![deny(unsafe_code)]

mod arithmetic;
mod convert;
mod storage;
#[cfg(test)]
mod tests;

use crate::storage::{Buffer, Scope};
use std::cmp::Ordering;

/// Maximum bit length of a BigInt's magnitude.
pub const MAX_BITS: usize = 1 << 20;
const WORD_BITS: usize = u64::BITS as usize;
const MAX_LIMBS: usize = MAX_BITS / WORD_BITS;
const TOO_LARGE: BigIntError = BigIntError::RangeError("BigInt is too large");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BigIntError {
    OutOfMemory,
    Interrupted,
    RangeError(&'static str),
    DivisionByZero,
    NegativeExponent,
    InvalidString,
}

/// Accounts for storage and lets long operations be interrupted.
///
/// A failed charge must leave the balance unchanged. Operations release scratch
/// storage on success or failure. The caller owns the charge for returned
/// storage (`BigInt::byte_size()` for an integer or `String::capacity()` for
/// a string) and releases it when that storage is freed.
///
/// Constructors without a host and `Clone` do not charge storage.
pub trait BigIntHost {
    fn charge(&mut self, bytes: usize) -> Result<(), BigIntError>;
    fn release(&mut self, bytes: usize);
    fn poll_interrupt(&mut self) -> bool;
}

/// Host with no memory or interrupt limits.
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

/// Arbitrary-precision signed integer in two's-complement form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BigInt {
    // Little-endian with the fewest limbs needed to preserve the sign.
    limbs: Vec<u64>,
}

impl BigInt {
    pub fn zero() -> Self {
        Self::from_i64(0)
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
        self.limbs.capacity() * size_of::<u64>()
    }

    pub fn is_zero(&self) -> bool {
        self.limbs == [0]
    }

    pub fn is_negative(&self) -> bool {
        negative(&self.limbs)
    }

    pub fn signum(&self) -> i8 {
        if self.is_negative() {
            -1
        } else {
            i8::from(!self.is_zero())
        }
    }

    pub fn is_even(&self) -> bool {
        self.limbs[0] & 1 == 0
    }

    /// Returns the number of bits in the absolute value. Zero has no bits.
    pub fn bit_len(&self) -> usize {
        Magnitude::new(&self.limbs).bit_len()
    }

    pub fn to_i64_exact(&self) -> Option<i64> {
        (self.limbs.len() == 1).then(|| self.to_i64_wrapping())
    }

    pub fn cmp_bigint(&self, other: &Self) -> Ordering {
        match self.is_negative().cmp(&other.is_negative()) {
            Ordering::Equal => {
                let by_len = self.limbs.len().cmp(&other.limbs.len());
                if by_len == Ordering::Equal {
                    self.limbs.iter().rev().cmp(other.limbs.iter().rev())
                } else if self.is_negative() {
                    by_len.reverse()
                } else {
                    by_len
                }
            }
            by_sign => by_sign.reverse(),
        }
    }

    pub fn to_i64_wrapping(&self) -> i64 {
        self.limbs[0] as i64
    }

    pub fn to_u64_wrapping(&self) -> u64 {
        self.limbs[0]
    }

    fn small(v: i64, scope: &Scope<'_>) -> Result<Self, BigIntError> {
        Ok(Self {
            limbs: scope.filled(1, v as u64)?.into_vec(),
        })
    }

    fn copy(&self, scope: &Scope<'_>) -> Result<Self, BigIntError> {
        Ok(Self {
            limbs: scope.copy(&self.limbs)?.into_vec(),
        })
    }

    fn finish(mut limbs: Buffer<'_, '_, u64>) -> Result<Self, BigIntError> {
        normalize(&mut limbs)?;
        Ok(Self {
            limbs: limbs.into_vec(),
        })
    }
}

fn negative(limbs: &[u64]) -> bool {
    (limbs[limbs.len() - 1] as i64) < 0
}

fn sign_word(limbs: &[u64]) -> u64 {
    0u64.wrapping_sub(u64::from(negative(limbs)))
}

fn extend(limbs: &[u64], i: usize) -> u64 {
    limbs.get(i).copied().unwrap_or_else(|| sign_word(limbs))
}

fn normalize(limbs: &mut Buffer<'_, '_, u64>) -> Result<(), BigIntError> {
    let mut len = limbs.len();
    while len > 1 && limbs[len - 1] == sign_word(&limbs[..len - 1]) {
        len -= 1;
    }
    limbs.truncate(len);
    if len >= MAX_LIMBS && Magnitude::new(limbs).bit_len() > MAX_BITS {
        return Err(TOO_LARGE);
    }
    Ok(())
}

/// Views the absolute value without making a copy of the limbs.
struct Magnitude<'a> {
    limbs: &'a [u64],
    negative: bool,
    first_nonzero: usize,
}

impl<'a> Magnitude<'a> {
    fn new(limbs: &'a [u64]) -> Self {
        let negative = negative(limbs);
        // In a negative value, zero limbs keep the carry from negation.
        let first_nonzero = if negative {
            limbs.iter().take_while(|&&word| word == 0).count()
        } else {
            0
        };
        Self {
            limbs,
            negative,
            first_nonzero,
        }
    }

    fn word(&self, i: usize) -> u64 {
        let Some(&word) = self.limbs.get(i) else {
            return 0;
        };
        if !self.negative {
            return word;
        }
        match i.cmp(&self.first_nonzero) {
            Ordering::Less => 0,
            Ordering::Equal => word.wrapping_neg(),
            Ordering::Greater => !word,
        }
    }

    fn len(&self) -> usize {
        let n = self.limbs.len();
        n - usize::from(n > 1 && self.word(n - 1) == 0)
    }

    fn bit_len(&self) -> usize {
        let n = self.len();
        n * WORD_BITS - self.word(n - 1).leading_zeros() as usize
    }

    fn bits_at(&self, offset: usize) -> u64 {
        let i = offset / WORD_BITS;
        let shift = offset % WORD_BITS;
        let low = self.word(i) >> shift;
        if shift == 0 {
            low
        } else {
            low | (self.word(i + 1) << (WORD_BITS - shift))
        }
    }

    fn any_below(&self, bits: usize) -> bool {
        let words = bits / WORD_BITS;
        let tail = bits % WORD_BITS;
        (0..words).any(|i| self.word(i) != 0)
            || (tail != 0 && self.word(words) << (WORD_BITS - tail) != 0)
    }
}
