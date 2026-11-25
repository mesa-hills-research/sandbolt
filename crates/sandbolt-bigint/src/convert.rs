use super::arithmetic::{apply_sign, copy_words, div_word, left_words};
use super::{
    BigInt, BigIntError, BigIntHost, MAX_BITS, MAX_LIMBS, Magnitude, Scope, TOO_LARGE, WORD_BITS,
};
use std::cmp::Ordering;

const FRACTION_BITS: usize = 52;
const PRECISION: usize = FRACTION_BITS + 1;
const HIDDEN_BIT: u64 = 1 << FRACTION_BITS;
const FRACTION_MASK: u64 = HIDDEN_BIT - 1;
const EXP_BIAS: i32 = 1023;
const NOT_FINITE: BigIntError = BigIntError::RangeError("cannot convert NaN or Infinity to BigInt");
const BAD_RADIX: BigIntError = BigIntError::RangeError("BigInt radix must be between 2 and 36");
const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

impl BigInt {
    /// Copies a sign and little-endian 32-bit magnitude into charged storage.
    pub fn from_sign_and_u32_limbs(
        negative: bool,
        limbs: &[u32],
        host: &mut dyn BigIntHost,
    ) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        scope.poll()?;
        let mut limbs = limbs;
        while let Some((&0, rest)) = limbs.split_last() {
            scope.step()?;
            limbs = rest;
        }
        if limbs.is_empty() {
            return Self::small(0, &scope);
        }
        if limbs.len() > MAX_BITS / u32::BITS as usize {
            return Err(TOO_LARGE);
        }
        let mut words = scope.filled(limbs.len().div_ceil(2), 0)?;
        for (word, chunk) in words.iter_mut().zip(limbs.chunks(2)) {
            scope.step()?;
            *word = u64::from(chunk[0]) | (u64::from(chunk.get(1).copied().unwrap_or(0)) << 32);
        }
        apply_sign(&mut words, negative, &scope)?;
        Self::finish(words)
    }

    /// Iterates over the little-endian 32-bit words of the absolute value.
    pub fn magnitude_u32_limbs(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        let magnitude = Magnitude::new(&self.limbs);
        let len = magnitude.bit_len().div_ceil(u32::BITS as usize);
        (0..len).map(move |i| (magnitude.word(i / 2) >> (32 * (i % 2))) as u32)
    }

    /// Copies a signed 64-bit integer into charged storage.
    pub fn from_i64_charged(v: i64, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        Self::small(v, &Scope::new(host))
    }

    /// Converts a finite number to an integer by rounding toward zero.
    pub fn from_f64_trunc(v: f64, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        if !v.is_finite() {
            return Err(NOT_FINITE);
        }
        let scope = Scope::new(host);
        let raw = v.to_bits();
        let e = exponent(raw);
        if e < 0 {
            return Self::small(0, &scope);
        }
        let mant = (raw & FRACTION_MASK) | HIDDEN_BIT;
        let shift = e - FRACTION_BITS as i32;
        let (word, shift) = if shift < 0 {
            (mant >> -shift, 0)
        } else {
            (mant, shift as usize)
        };
        let word = if v.is_sign_negative() {
            word.wrapping_neg()
        } else {
            word
        };
        Self::finish(left_words(&[word], shift, &scope)?)
    }

    /// Converts a number as `BigInt(number)` does, rejecting fractions.
    pub fn from_f64_exact(v: f64, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        if v.is_finite() && v.trunc() != v {
            return Err(BigIntError::RangeError(
                "cannot convert to BigInt: not an integer",
            ));
        }
        Self::from_f64_trunc(v, host)
    }

    /// Rounds to the nearest binary64 value, breaking ties toward even.
    pub fn to_f64(&self) -> f64 {
        if let Some(v) = self.to_i64_exact() {
            return v as f64;
        }
        let m = Magnitude::new(&self.limbs);
        let bits = m.bit_len();
        let sign = u64::from(m.negative) << 63;
        if bits > EXP_BIAS as usize + 1 {
            return f64::from_bits(sign | f64::INFINITY.to_bits());
        }
        let (mut mant, low) = significand(&m, bits);
        if low != 0 && m.bits_at(low - 1) & 1 != 0 && (mant & 1 != 0 || m.any_below(low - 1)) {
            mant += 1;
        }
        let mut biased_exp = bits - 1 + EXP_BIAS as usize;
        if mant == HIDDEN_BIT * 2 {
            mant >>= 1;
            biased_exp += 1;
        }
        f64::from_bits(sign | (biased_exp as u64) << FRACTION_BITS | (mant & FRACTION_MASK))
    }

    /// Compares without first rounding the BigInt to a float.
    pub fn cmp_f64(&self, d: f64) -> Option<Ordering> {
        if d.is_nan() {
            return None;
        }
        if d.is_infinite() {
            return Some(if d.is_sign_negative() {
                Ordering::Greater
            } else {
                Ordering::Less
            });
        }
        let b_sign = if d == 0.0 {
            0
        } else if d.is_sign_negative() {
            -1
        } else {
            1
        };
        let a_sign = self.signum();
        if a_sign != b_sign || a_sign == 0 {
            return Some(a_sign.cmp(&b_sign));
        }
        let a = Magnitude::new(&self.limbs);
        let bits = a.bit_len();
        let raw = d.to_bits();
        let mut cmp = (bits as i32 - 1).cmp(&exponent(raw));
        if cmp == Ordering::Equal {
            let (mant, low) = significand(&a, bits);
            cmp = mant.cmp(&((raw & FRACTION_MASK) | HIDDEN_BIT));
            if cmp == Ordering::Equal && a.any_below(low) {
                cmp = Ordering::Greater;
            }
        }
        Some(if a_sign < 0 { cmp.reverse() } else { cmp })
    }

    /// Parses ASCII digits in the given radix, preceded by an optional sign.
    pub fn parse_radix(
        s: &str,
        radix: u32,
        host: &mut dyn BigIntHost,
    ) -> Result<Self, BigIntError> {
        check_radix(radix)?;
        let scope = Scope::new(host);
        scope.poll()?;
        parse(s, radix, &scope)
    }

    /// Parses a string as `BigInt(string)` does.
    pub fn from_str_js(s: &str, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        scope.poll()?;
        let mut chars = s.char_indices();
        let start = loop {
            match chars.next() {
                Some((i, c)) if !js_whitespace(c) => break i,
                Some(_) => scope.step()?,
                None => return Self::small(0, &scope),
            }
        };
        let mut end = s.len();
        for (i, c) in chars.rev() {
            scope.step()?;
            if !js_whitespace(c) {
                break;
            }
            end = i;
        }
        let s = &s[start..end];
        match s.as_bytes() {
            [b'0', b'x' | b'X', ..] => parse_unsigned(&s.as_bytes()[2..], 16, false, &scope),
            [b'0', b'o' | b'O', ..] => parse_unsigned(&s.as_bytes()[2..], 8, false, &scope),
            [b'0', b'b' | b'B', ..] => parse_unsigned(&s.as_bytes()[2..], 2, false, &scope),
            _ => parse(s, 10, &scope),
        }
    }

    pub fn to_string_radix(
        &self,
        radix: u32,
        host: &mut dyn BigIntHost,
    ) -> Result<String, BigIntError> {
        check_radix(radix)?;
        let scope = Scope::new(host);
        scope.poll()?;
        let mag = Magnitude::new(&self.limbs);
        let bits = mag.bit_len();
        let digit_bits = radix.ilog2() as usize;
        let digits = bits.div_ceil(digit_bits).max(1);
        let mut out = scope.buffer(digits + usize::from(mag.negative))?;
        if bits == 0 {
            out.push(b'0')?;
        } else if radix.is_power_of_two() {
            for i in 0..digits {
                scope.step()?;
                let d = mag.bits_at(i * digit_bits) & (u64::from(radix) - 1);
                out.push(DIGITS[d as usize])?;
            }
        } else {
            let (chunk_pow, chunk_len) = radix_chunk(radix);
            let mut n = mag.len();
            let mut work = scope.filled(n, 0)?;
            copy_words(&mag, &mut work, &scope)?;
            while n != 0 {
                let mut rem = div_word(&mut work[..n], chunk_pow, &scope)?;
                while n != 0 && work[n - 1] == 0 {
                    n -= 1;
                }
                for _ in 0..chunk_len {
                    let d = rem % u64::from(radix);
                    out.push(DIGITS[d as usize])?;
                    rem /= u64::from(radix);
                    if n == 0 && rem == 0 {
                        break;
                    }
                }
            }
        }
        if mag.negative {
            out.push(b'-')?;
        }
        out.reverse();
        Ok(String::from_utf8(out.into_vec()).expect("radix digits are ASCII"))
    }
}

fn exponent(raw: u64) -> i32 {
    ((raw >> FRACTION_BITS) & 0x7ff) as i32 - EXP_BIAS
}

// Returns the top 53 bits and the number of bits left below them.
fn significand(m: &Magnitude<'_>, bits: usize) -> (u64, usize) {
    if bits < PRECISION {
        (m.word(0) << (PRECISION - bits), 0)
    } else {
        let low = bits - PRECISION;
        (m.bits_at(low), low)
    }
}

fn check_radix(radix: u32) -> Result<(), BigIntError> {
    if (2..=36).contains(&radix) {
        Ok(())
    } else {
        Err(BAD_RADIX)
    }
}

fn parse(s: &str, radix: u32, scope: &Scope<'_>) -> Result<BigInt, BigIntError> {
    match s.as_bytes() {
        [b'-', digits @ ..] => parse_unsigned(digits, radix, true, scope),
        [b'+', digits @ ..] => parse_unsigned(digits, radix, false, scope),
        digits => parse_unsigned(digits, radix, false, scope),
    }
}

fn parse_unsigned(
    digits: &[u8],
    radix: u32,
    neg: bool,
    scope: &Scope<'_>,
) -> Result<BigInt, BigIntError> {
    if digits.is_empty() {
        return Err(BigIntError::InvalidString);
    }
    let mut start = None;
    for (i, &c) in digits.iter().enumerate() {
        scope.step()?;
        if digit(c) >= radix {
            return Err(BigIntError::InvalidString);
        }
        if c != b'0' && start.is_none() {
            start = Some(i);
        }
    }
    let Some(start) = start else {
        return BigInt::small(0, scope);
    };
    let digits = &digits[start..];
    let digit_bits = radix.ilog2() as usize;
    if digits.len() > (MAX_BITS - 1) / digit_bits + 1 {
        return Err(TOO_LARGE);
    }
    let mut words;
    if radix.is_power_of_two() {
        let bits = (digits.len() - 1) * digit_bits + digit(digits[0]).ilog2() as usize + 1;
        if bits > MAX_BITS {
            return Err(TOO_LARGE);
        }
        words = scope.filled(bits.div_ceil(WORD_BITS), 0)?;
        for (i, &c) in digits.iter().rev().enumerate() {
            scope.step()?;
            let d = u64::from(digit(c));
            let offset = i * digit_bits;
            let word = offset / WORD_BITS;
            let shift = offset % WORD_BITS;
            words[word] |= d << shift;
            if shift + digit_bits > WORD_BITS && word + 1 < words.len() {
                words[word + 1] |= d >> (WORD_BITS - shift);
            }
        }
    } else {
        let (chunk_pow, chunk_len) = radix_chunk(radix);
        words = scope.filled(1, 0)?;
        for chunk in digits.chunks(chunk_len) {
            let mut carry = 0;
            for &c in chunk {
                carry = carry * u64::from(radix) + u64::from(digit(c));
            }
            let pow = if chunk.len() == chunk_len {
                chunk_pow
            } else {
                u64::from(radix).pow(chunk.len() as u32)
            };
            for word in words.iter_mut() {
                scope.step()?;
                let product = *word as u128 * pow as u128 + carry as u128;
                *word = product as u64;
                carry = (product >> WORD_BITS) as u64;
            }
            if carry != 0 {
                if words.len() == MAX_LIMBS {
                    return Err(TOO_LARGE);
                }
                if words.len() == words.capacity() {
                    let cap = words.len() + (words.len() / 2).max(1);
                    words.reserve(cap.min(MAX_LIMBS))?;
                }
                words.push(carry)?;
            }
        }
    }
    apply_sign(&mut words, neg, scope)?;
    BigInt::finish(words)
}

fn radix_chunk(radix: u32) -> (u64, usize) {
    let mut power = 1u64;
    let mut len = 0;
    while let Some(next) = power.checked_mul(u64::from(radix)) {
        power = next;
        len += 1;
    }
    (power, len)
}

fn digit(c: u8) -> u32 {
    u32::from(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'z' => c - b'a' + 10,
        b'A'..=b'Z' => c - b'A' + 10,
        _ => u8::MAX,
    })
}

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
