//! QuickJS-ng v0.17.0 number parsing and formatting in safe Rust.

#![deny(unsafe_code)]

mod format;
mod integer;
mod limbs;
mod parse;

#[cfg(test)]
mod tests;

pub use format::{dtoa, dtoa_max_len};
pub use integer::{i32toa, i64toa, i64toa_radix, u32toa, u64toa, u64toa_radix};
pub use parse::atod;

use limbs::{Limbs, Round};

/// The maximum precision for [`DtoaFormat::Fixed`] and [`DtoaFormat::Frac`].
pub const DTOA_MAX_DIGITS: usize = 101;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtoaFormat {
    /// Use as few digits as needed to round-trip the value.
    Free = 0,
    /// Set the number of significant digits.
    Fixed = 1,
    /// Set the number of digits after the point.
    Frac = 2,
}

/// Exponent notation for `Free` and `Fixed` formats, or zero in `Frac` format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtoaExp {
    /// Use an exponent outside the plain notation range for this radix and precision.
    Auto = 0,
    /// Use an exponent for every finite value.
    Enabled = 1,
    /// Use plain notation for every finite value.
    Disabled = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtoaOptions {
    pub format: DtoaFormat,
    pub exp: DtoaExp,
    /// Keep the leading minus sign on negative zero.
    pub minus_zero: bool,
}

impl Default for DtoaOptions {
    fn default() -> Self {
        Self {
            format: DtoaFormat::Free,
            exp: DtoaExp::Auto,
            minus_zero: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtodFlags {
    /// Read only integer digits, without a point, an exponent, or `Infinity`.
    pub int_only: bool,
    /// Recognize `0b` and `0o` prefixes when the radix is zero.
    pub accept_bin_oct: bool,
    /// Recognize Annex B leading-zero octal when the radix is zero.
    pub accept_legacy_octal: bool,
    /// Accept underscores as digit separators.
    pub accept_underscores: bool,
}

const RADICES: usize = 35;
const FRACTION_BIT: u64 = 1 << 52;
const FRACTION_MASK: u64 = FRACTION_BIT - 1;

const DIGITS_PER_LIMB: [u32; RADICES] = [
    32, 20, 16, 13, 12, 11, 10, 10, 9, 9, 8, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6,
];

const LIMB_BASES: [u64; RADICES] = {
    let mut bases = [0; RADICES];
    let mut i = 0;
    while i < RADICES {
        bases[i] = (i as u64 + 2).pow(DIGITS_PER_LIMB[i]);
        i += 1;
    }
    bases
};

fn radix_index(radix: u32) -> usize {
    assert!((2..=36).contains(&radix), "radix must be in 2..=36");
    (radix - 2) as usize
}

#[derive(Clone, Copy)]
struct Radix {
    value: u32,
    index: usize,
    odd: u32,
    binary_shift: i32,
}

impl Radix {
    fn new(value: u32) -> Self {
        let index = radix_index(value);
        let binary_shift = value.trailing_zeros() as i32;
        Self {
            value,
            index,
            odd: value >> binary_shift,
            binary_shift,
        }
    }

    fn binary_exponent(self) -> bool {
        self.odd == 1 && self.binary_shift <= 4
    }

    // Multiply by radix^power. The new integer times 2^-offset is the result.
    // Division keeps enough bits to round and records any remainder in the low bit.
    fn scale(self, value: &mut Limbs, power: i32, for_integer: bool, exponent: i32) -> i32 {
        let mut offset = -power * self.binary_shift;
        if self.odd == 1 || power == 0 {
            return offset;
        }

        let i = (self.odd - 2) as usize;
        let group = DIGITS_PER_LIMB[i];
        let base = LIMB_BASES[i];
        let count = power.unsigned_abs();
        let (groups, tail) = (count / group, count % group);
        if power > 0 {
            for _ in 0..groups {
                value.mul_add(base, 0);
            }
            if tail != 0 {
                value.mul_add(u64::from(self.odd.pow(tail)), 0);
            }
        } else {
            let shift = (count.div_ceil(group) * u32::BITS) as i32;
            offset += shift;
            let extra = if for_integer {
                2 + exponent - offset
            } else {
                exponent - value.floor_log2()
            }
            .max(0);
            offset += extra;
            value.round_shift(-shift - extra, Round::Truncate);
            let mut remainder = 0;
            for _ in 0..groups {
                remainder |= value.div_rem(base as u32);
            }
            if tail != 0 {
                remainder |= value.div_rem(self.odd.pow(tail));
            }
            value.set_sticky(remainder != 0);
        }
        offset
    }

    fn scale_to_float(self, value: &mut Limbs, power: i32) -> (u64, i32) {
        let offset = self.scale(value, power, false, 55);
        round_to_float(value, offset)
    }
}

// Return a mantissa and exponent such that the value is mantissa * 2^(exponent - 53).
fn round_to_float(value: &mut Limbs, offset: i32) -> (u64, i32) {
    if value.is_zero() {
        return (0, 0);
    }
    let mut exponent = value.floor_log2() + 1 - offset;
    let missing_bits = (-1021 - exponent).max(0);
    value.round_shift(exponent + offset - 53 + missing_bits, Round::Even);
    let mut mantissa = value.as_u64() << missing_bits;
    if mantissa >= 1 << 53 {
        mantissa >>= 1;
        exponent += 1;
    }
    (mantissa, exponent)
}
