use std::cmp::Ordering;

use crate::integer::write_limb;
use crate::{
    DIGITS_PER_LIMB, DTOA_MAX_DIGITS, DtoaExp, DtoaFormat, DtoaOptions, FRACTION_BIT,
    FRACTION_MASK, LIMB_BASES, Limbs, RADICES, Radix, Round, u32toa, u64toa_radix,
};

const MAX_FREE_DIGITS: [i32; RADICES] = [
    54, 35, 28, 24, 22, 20, 19, 18, 17, 17, 16, 16, 15, 15, 15, 14, 14, 14, 14, 14, 13, 13, 13, 13,
    13, 13, 13, 12, 12, 12, 12, 12, 12, 12, 12,
];

// round(2^24 / log2(radix)) for each radix that needs an approximation.
const INVERSE_LOG2: [i64; RADICES] = [
    0x000000, 0xa1849d, 0x000000, 0x6e40d2, 0x6308c9, 0x5b3065, 0x000000, 0x50c24e, 0x4d104d,
    0x4a0027, 0x4768ce, 0x452e54, 0x433d00, 0x418677, 0x000000, 0x3ea16b, 0x3d645a, 0x3c43c2,
    0x3b3b9a, 0x3a4899, 0x39680b, 0x3897b3, 0x37d5af, 0x372069, 0x367686, 0x35d6df, 0x354072,
    0x34b261, 0x342bea, 0x33ac62, 0x000000, 0x32bfd9, 0x3251dd, 0x31e8d6, 0x318465,
];

// floor(binary_exponent / log2(radix)) for -2048 <= binary_exponent <= 2047.
fn radix_exponent(binary_exponent: i32, radix: Radix) -> i32 {
    if radix.odd == 1 {
        binary_exponent.div_euclid(radix.binary_shift)
    } else {
        ((i64::from(binary_exponent) * INVERSE_LOG2[radix.index]) >> 24) as i32
    }
}

fn exponent_field(bits: u64) -> i32 {
    ((bits >> 52) & 0x7ff) as i32
}

fn subnormal_shift(mantissa: u64) -> i32 {
    mantissa.leading_zeros() as i32 - 11
}

fn requested_digits(n_digits: usize, format: DtoaFormat) -> i32 {
    match format {
        DtoaFormat::Free => return 0,
        DtoaFormat::Fixed => assert!((1..=DTOA_MAX_DIGITS).contains(&n_digits)),
        DtoaFormat::Frac => assert!(n_digits <= DTOA_MAX_DIGITS),
    }
    n_digits as i32
}

/// Returns an upper bound on the length of [`dtoa`]'s result for the same arguments.
pub fn dtoa_max_len(d: f64, radix: u32, n_digits: usize, opts: DtoaOptions) -> usize {
    max_len(
        d,
        Radix::new(radix),
        requested_digits(n_digits, opts.format),
        opts,
    )
}

fn max_len(d: f64, radix: Radix, n_digits: i32, opts: DtoaOptions) -> usize {
    let bits = d.to_bits();
    let exponent = exponent_field(bits) - 1023;
    let n = match opts.format {
        // Zero in Frac mode still honors an explicit exponent.
        DtoaFormat::Frac if d == 0.0 && opts.exp == DtoaExp::Enabled => n_digits + 6,
        DtoaFormat::Frac if d.is_finite() => {
            let integer = if exponent < 0 {
                1
            } else {
                2 + radix_exponent(exponent - 1, radix)
            };
            integer + 3 + n_digits
        }
        DtoaFormat::Frac => 0,
        format => {
            let digits = if format == DtoaFormat::Free {
                MAX_FREE_DIGITS[radix.index]
            } else {
                n_digits
            };
            if opts.exp != DtoaExp::Disabled {
                digits + 8
            } else if d.is_finite() {
                let bound = digits + 10 + radix_exponent(exponent - 1, radix).abs();
                let fraction = bits & FRACTION_MASK;
                if exponent == -1023 && fraction != 0 {
                    // The encoded exponent alone can undercount the leading zeroes.
                    let point = 1 + radix_exponent(-1022 - subnormal_shift(fraction), radix);
                    bound.max(digits + 3 - point)
                } else {
                    bound
                }
            } else {
                0
            }
        }
    };
    n.max(9) as usize
}

/// Formats an IEEE 754 double using QuickJS's number conversion rules.
///
/// `radix` is 2 through 36. `Fixed` takes 1 through [`DTOA_MAX_DIGITS`]
/// significant digits, `Frac` takes 0 through [`DTOA_MAX_DIGITS`] fractional
/// digits, and `Free` ignores `n_digits`.
pub fn dtoa(d: f64, radix: u32, n_digits: usize, opts: DtoaOptions) -> String {
    let radix = Radix::new(radix);
    let n_digits = requested_digits(n_digits, opts.format);
    let bits = d.to_bits();
    let negative = bits >> 63 != 0;
    let mut exponent = exponent_field(bits);
    let mut mantissa = bits & FRACTION_MASK;

    if exponent == 0x7ff {
        return if mantissa != 0 {
            "NaN"
        } else if negative {
            "-Infinity"
        } else {
            "Infinity"
        }
        .to_owned();
    }

    let mut buf = Vec::with_capacity(max_len(d, radix, n_digits, opts));
    let zero = d == 0.0;
    if negative && (!zero || opts.minus_zero) {
        buf.push(b'-');
    }

    let mut scaled = Limbs::new(0);
    let (digits, point) = if zero {
        let digits = match opts.format {
            DtoaFormat::Free => 1,
            DtoaFormat::Fixed => n_digits,
            DtoaFormat::Frac => n_digits + 1,
        };
        (digits, 1)
    } else {
        if exponent == 0 {
            let shift = subnormal_shift(mantissa);
            exponent = 1 - shift;
            mantissa <<= shift;
        } else {
            mantissa |= FRACTION_BIT;
        }
        exponent -= 1022;

        if opts.format == DtoaFormat::Free
            && opts.exp != DtoaExp::Enabled
            && (1..=53).contains(&exponent)
            && mantissa.trailing_zeros() as i32 >= 53 - exponent
        {
            let mut digits = [0; 64];
            let len = u64toa_radix(&mut digits, mantissa >> (53 - exponent), radix.value);
            buf.extend_from_slice(&digits[..len]);
            return ascii(buf);
        }

        let mut point = 1 + radix_exponent(exponent - 1, radix);
        match opts.format {
            DtoaFormat::Free => shortest(&mut scaled, radix, mantissa, exponent, point),
            DtoaFormat::Frac => {
                round_scaled(
                    &mut scaled,
                    radix,
                    mantissa,
                    exponent,
                    n_digits,
                    Round::Away,
                );
                let point = (point + 1).max(1);
                let start = buf.len();
                output_digits(&mut buf, &mut scaled, radix, point + n_digits, point);
                // Leave room for a rounding carry, then drop the extra zero if unused.
                if buf[start] == b'0' && buf.len() > start + 1 && buf[start + 1] != b'.' {
                    buf.remove(start);
                }
                return ascii(buf);
            }
            DtoaFormat::Fixed => {
                let mut limit = Limbs::new(1);
                let offset = radix.scale(&mut limit, n_digits, false, 0);
                limit.round_shift(offset, Round::Truncate);
                loop {
                    round_scaled(
                        &mut scaled,
                        radix,
                        mantissa,
                        exponent,
                        n_digits - point,
                        Round::Away,
                    );
                    if scaled.cmp(&limit) == Ordering::Less {
                        break;
                    }
                    point += 1;
                }
                (n_digits, point)
            }
        }
    };

    let max_point = if opts.format == DtoaFormat::Fixed {
        n_digits
    } else {
        MAX_FREE_DIGITS[radix.index] + 4
    };
    if opts.exp == DtoaExp::Enabled
        || (opts.exp == DtoaExp::Auto && (point <= -6 || point > max_point))
    {
        output_digits(&mut buf, &mut scaled, radix, digits, 1);
        let mut exp = point - 1;
        buf.push(if radix.value == 10 {
            b'e'
        } else if radix.binary_exponent() {
            exp *= radix.binary_shift;
            b'p'
        } else {
            b'@'
        });
        buf.push(if exp < 0 { b'-' } else { b'+' });
        let mut digits = [0; 10];
        let len = u32toa(&mut digits, exp.unsigned_abs());
        buf.extend_from_slice(&digits[..len]);
    } else if point <= 0 {
        buf.extend_from_slice(b"0.");
        buf.resize(buf.len() + (-point) as usize, b'0');
        output_digits(&mut buf, &mut scaled, radix, digits, digits);
    } else {
        output_digits(&mut buf, &mut scaled, radix, digits, digits.min(point));
        buf.resize(buf.len() + (point - digits).max(0) as usize, b'0');
    }
    ascii(buf)
}

fn ascii(buf: Vec<u8>) -> String {
    String::from_utf8(buf).expect("number formatting writes only ASCII")
}

fn round_scaled(
    value: &mut Limbs,
    radix: Radix,
    mantissa: u64,
    exponent: i32,
    power: i32,
    rounding: Round,
) {
    value.set(mantissa);
    let exponent = exponent - 53;
    let offset = radix.scale(value, power, true, exponent);
    value.round_shift(offset - exponent, rounding);
}

// Start with enough digits to round-trip and reduce the precision until it fails.
fn shortest(
    value: &mut Limbs,
    radix: Radix,
    mantissa: u64,
    exponent: i32,
    initial_point: i32,
) -> (i32, i32) {
    let mut digits = MAX_FREE_DIGITS[radix.index];
    let base = u64::from(radix.value);
    let mut best = None;
    loop {
        let limit = base.pow(digits as u32);
        let mut point = initial_point;
        let mut scaled = loop {
            round_scaled(
                value,
                radix,
                mantissa,
                exponent,
                digits - point,
                Round::Even,
            );
            let scaled = value.as_u64();
            if scaled < limit {
                break scaled;
            }
            point += 1;
        };
        while scaled % base == 0 {
            scaled /= base;
            digits -= 1;
        }

        if let Some((last, last_digits, last_point)) = best {
            value.set(scaled);
            if radix.scale_to_float(value, point - digits) != (mantissa, exponent) {
                value.set(last);
                return (last_digits, last_point);
            }
        }
        if digits == 1 {
            value.set(scaled);
            return (digits, point);
        }
        best = Some((scaled, digits, point));
        digits -= 1;
    }
}

fn output_digits(buf: &mut Vec<u8>, value: &mut Limbs, radix: Radix, digits: i32, point: i32) {
    let start = buf.len();
    let digits = digits as usize;
    let point = point as usize;
    buf.resize(start + digits, b'0');

    let group = DIGITS_PER_LIMB[radix.index] as usize;
    let shift = if radix.odd == 1 {
        radix.binary_shift
    } else {
        0
    };
    let mut tail = digits;
    while tail != 0 && !value.is_zero() {
        let head = tail.saturating_sub(group);
        let limb = if shift != 0 {
            let limb = value.low_limb();
            value.round_shift(group as i32 * shift, Round::Truncate);
            limb
        } else {
            value.div_rem(LIMB_BASES[radix.index] as u32)
        };
        write_limb(
            &mut buf[start + head..start + tail],
            limb,
            radix.value,
            shift,
        );
        tail = head;
    }
    if point != digits {
        buf.insert(start + point, b'.');
    }
}
