use crate::{
    AtodFlags, DIGITS_PER_LIMB, FRACTION_MASK, LIMB_BASES, Limbs, RADICES, Radix, round_to_float,
};

const INFINITY: u64 = f64::INFINITY.to_bits();

// Keep enough digits for exact rounding in binary bases. Other bases use
// QuickJS's cutoff, which truncates excess significant digits.
const SIGNIFICANT_DIGITS: [usize; RADICES] = [
    64, 80, 32, 55, 49, 45, 21, 40, 38, 37, 35, 34, 33, 32, 16, 31, 30, 30, 29, 29, 28, 28, 27, 27,
    27, 26, 26, 26, 26, 25, 12, 25, 25, 24, 24,
];

const MAX_EXPONENT: [i32; RADICES] = [
    1024, 647, 512, 442, 397, 365, 342, 324, 309, 297, 286, 277, 269, 263, 256, 251, 246, 242, 237,
    234, 230, 227, 224, 221, 218, 216, 214, 211, 209, 207, 205, 203, 202, 200, 199,
];

const MIN_EXPONENT: [i32; RADICES] = [
    -1075, -679, -538, -463, -416, -383, -359, -340, -324, -311, -300, -291, -283, -276, -269,
    -263, -258, -254, -249, -245, -242, -238, -235, -232, -229, -227, -224, -222, -220, -217, -215,
    -214, -212, -210, -208,
];

fn digit(byte: u8) -> u32 {
    u32::from(match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'z' => byte - b'a' + 10,
        b'A'..=b'Z' => byte - b'A' + 10,
        _ => 36,
    })
}

/// Parses a numeric prefix and returns the value and the number of bytes read.
///
/// Use radix 0 to detect a prefix or default to decimal. Explicit radixes are
/// 2 through 36. Leading whitespace is left for the caller to skip. A missing
/// number or a malformed prefix or exponent yields NaN.
pub fn atod(input: &str, mut radix: u32, flags: AtodFlags) -> (f64, usize) {
    let input = input.as_bytes();
    let byte = |index| input.get(index).copied().unwrap_or(0);
    let negative = byte(0) == b'-';
    let sign = u64::from(negative) << 63;
    let mut p = usize::from(negative || byte(0) == b'+');
    let start = p;
    let mut separators = flags.accept_underscores;

    if byte(p) == b'0' {
        let prefix = match byte(p + 1) {
            b'x' | b'X' if radix == 0 || radix == 16 => Some((2, 16)),
            b'o' | b'O' if radix == 0 && flags.accept_bin_oct => Some((2, 8)),
            b'b' | b'B' if radix == 0 && flags.accept_bin_oct => Some((2, 2)),
            b'0'..=b'9' if radix == 0 && flags.accept_legacy_octal => {
                separators = false;
                let mut i = p + 1;
                while matches!(byte(i), b'0'..=b'7') {
                    i += 1;
                }
                (!matches!(byte(i), b'8' | b'9')).then_some((1, 8))
            }
            _ => None,
        };
        if let Some((skip, base)) = prefix {
            p += skip;
            radix = base;
            if digit(byte(p)) >= radix {
                return (f64::NAN, p);
            }
        }
    } else if !flags.int_only && input[p..].starts_with(b"Infinity") {
        return (f64::from_bits(INFINITY | sign), p + 8);
    }

    if matches!(radix, 0 | 10)
        && !separators
        && let Some((value, end)) = simple_decimal(input, p, flags.int_only)
    {
        return (f64::from_bits(value.to_bits() | sign), end);
    }

    let radix = Radix::new(if radix == 0 { 10 } else { radix });
    let may_read_point =
        |p| !flags.int_only && byte(p) == b'.' && (p > start || digit(byte(p + 1)) < radix.value);
    let mut point = None;
    let mut pos = 0;
    loop {
        if may_read_point(p) {
            if point.is_some() {
                break;
            }
            point = Some(pos);
            p += 1;
        }
        if separators && byte(p) == b'_' && p > start && byte(p + 1) == b'0' {
            p += 1;
        }
        if byte(p) != b'0' {
            break;
        }
        p += 1;
        pos += 1;
    }

    let significant_start = pos;
    let mut mantissa = Limbs::new(0);
    let mut limb = 0;
    let mut limb_digits = 0;
    let mut digits = 0;
    let mut extra_digits = 0;
    loop {
        if may_read_point(p) {
            if point.is_some() {
                break;
            }
            point = Some(pos);
            p += 1;
        }
        if separators && byte(p) == b'_' && p > start && digit(byte(p + 1)) < radix.value {
            p += 1;
        }
        let value = digit(byte(p));
        if value >= radix.value {
            break;
        }
        p += 1;
        pos += 1;
        if digits < SIGNIFICANT_DIGITS[radix.index] {
            limb = limb * radix.value + value;
            limb_digits += 1;
            if limb_digits == DIGITS_PER_LIMB[radix.index] {
                mantissa.mul_add(LIMB_BASES[radix.index], limb);
                limb = 0;
                limb_digits = 0;
            }
            digits += 1;
        } else {
            extra_digits |= value;
        }
    }
    if limb_digits != 0 {
        mantissa.mul_add(u64::from(radix.value).pow(limb_digits), limb);
    }
    let exp_offset =
        ((significant_start + digits) as i32).wrapping_sub(point.unwrap_or(pos) as i32);
    if radix.odd == 1 {
        mantissa.set_sticky(extra_digits != 0);
    }

    let mut exp = 0i32;
    let mut binary_exponent = false;
    if !flags.int_only && p > start {
        let marker = byte(p);
        binary_exponent = radix.binary_exponent() && matches!(marker, b'p' | b'P');
        if binary_exponent
            || radix.value == 10 && matches!(marker, b'e' | b'E')
            || radix.value != 10 && marker == b'@'
        {
            p += 1;
            let exp_negative = byte(p) == b'-';
            if exp_negative || byte(p) == b'+' {
                p += 1;
            }
            if digit(byte(p)) >= 10 {
                return (f64::NAN, p);
            }
            let mut overflow = false;
            loop {
                let value = digit(byte(p));
                if value >= 10 {
                    break;
                }
                if !overflow {
                    if exp > (i32::MAX - 2 - 9) / 10 {
                        overflow = true;
                    } else {
                        exp = exp * 10 + value as i32;
                    }
                }
                p += 1;
                if separators && byte(p) == b'_' && digit(byte(p + 1)) < 10 {
                    p += 1;
                }
            }
            if digits != 0 && overflow {
                return (
                    f64::from_bits(sign | if exp_negative { 0 } else { INFINITY }),
                    p,
                );
            }
            if exp_negative {
                exp = -exp;
            }
        }
    }

    let bits = if p == start {
        return (f64::NAN, p);
    } else if digits == 0 {
        0
    } else {
        to_bits(
            radix,
            &mut mantissa,
            digits as i32,
            exp,
            exp_offset,
            binary_exponent,
        )
    };
    (f64::from_bits(bits | sign), p)
}

const POW10: [f64; 16] = {
    let mut powers = [1.0; 16];
    let mut i = 1;
    while i < powers.len() {
        powers[i] = powers[i - 1] * 10.0;
        i += 1;
    }
    powers
};

/// Parses a whole ASCII decimal with at most 15 digits and no exponent.
/// Returns `None` if the input needs the general parser.
#[inline]
pub fn parse_simple_decimal(input: &[u8]) -> Option<f64> {
    let (negative, start) = match input.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let (value, end) = simple_decimal(input, start, false)?;
    (end == input.len()).then_some(if negative { -value } else { value })
}

// Both operands of the division are exact, so it rounds just once.
#[inline]
fn simple_decimal(input: &[u8], mut pos: usize, int_only: bool) -> Option<(f64, usize)> {
    let start = pos;
    let mut mantissa = 0u64;
    while let Some(&byte @ b'0'..=b'9') = input.get(pos) {
        if pos - start == 15 {
            return None;
        }
        mantissa = mantissa * 10 + u64::from(byte - b'0');
        pos += 1;
    }
    let mut frac = 0;
    if !int_only && input.get(pos) == Some(&b'.') {
        pos += 1;
        while let Some(&byte @ b'0'..=b'9') = input.get(pos) {
            if pos - start > 15 {
                return None;
            }
            mantissa = mantissa * 10 + u64::from(byte - b'0');
            frac += 1;
            pos += 1;
        }
        if pos - start == 1 {
            return None;
        }
    }
    if pos == start || (!int_only && matches!(input.get(pos), Some(b'e' | b'E'))) {
        return None;
    }
    let value = mantissa as f64;
    Some((
        if frac == 0 {
            value
        } else {
            value / POW10[frac]
        },
        pos,
    ))
}

fn to_bits(
    radix: Radix,
    mantissa: &mut Limbs,
    digits: i32,
    exp: i32,
    exp_offset: i32,
    binary_exponent: bool,
) -> u64 {
    let (mantissa, e) = if radix.odd == 1 {
        // QuickJS's 32-bit exponent arithmetic wraps when a radix exponent
        // is multiplied by the number of bits per digit.
        let shift = radix.binary_shift;
        let exp = if binary_exponent {
            exp
        } else {
            exp.wrapping_mul(shift)
        };
        let exp = exp.wrapping_sub(exp_offset.wrapping_mul(shift));
        let leading_exp = exp.wrapping_add(digits * shift);
        if leading_exp >= 1024 + shift {
            return INFINITY;
        }
        if leading_exp <= -1075 {
            return 0;
        }
        round_to_float(mantissa, exp.wrapping_neg())
    } else {
        let exp = exp.wrapping_sub(exp_offset);
        let leading_exp = exp.wrapping_add(digits);
        if leading_exp > MAX_EXPONENT[radix.index] {
            return INFINITY;
        }
        if leading_exp <= MIN_EXPONENT[radix.index] {
            return 0;
        }
        radix.scale_to_float(mantissa, exp)
    };

    match e {
        _ if mantissa == 0 || e < -1073 => 0,
        _ if e > 1024 => INFINITY,
        _ if e < -1021 => mantissa >> (-e - 1021),
        _ => ((e + 1022) as u64) << 52 | (mantissa & FRACTION_MASK),
    }
}
