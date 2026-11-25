use super::{
    BigInt, BigIntError, BigIntHost, Buffer, MAX_BITS, Magnitude, Scope, TOO_LARGE, WORD_BITS,
    extend, negative, normalize, sign_word,
};

impl BigInt {
    pub fn neg(&self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        add_words(&[0], &self.limbs, true, &Scope::new(host))
    }

    pub fn bitnot(&self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        let mut result = scope.filled(self.limbs.len(), 0)?;
        for (out, &word) in result.iter_mut().zip(&self.limbs) {
            *out = !word;
        }
        Self::finish(result)
    }

    pub fn add(&self, other: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        add_words(&self.limbs, &other.limbs, false, &Scope::new(host))
    }

    pub fn sub(&self, other: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        add_words(&self.limbs, &other.limbs, true, &Scope::new(host))
    }

    pub fn mul(&self, other: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        let limbs = mul_words(&self.limbs, &other.limbs, &scope)?;
        Ok(Self {
            limbs: limbs.into_vec(),
        })
    }

    pub fn divrem(
        &self,
        other: &Self,
        host: &mut dyn BigIntHost,
    ) -> Result<(Self, Self), BigIntError> {
        if other.is_zero() {
            return Err(BigIntError::DivisionByZero);
        }
        let scope = Scope::new(host);
        scope.poll()?;
        let a = Magnitude::new(&self.limbs);
        let b = Magnitude::new(&other.limbs);
        let (na, nb) = (a.len(), b.len());
        let (mut q, mut r);

        if na < nb || self.is_zero() {
            q = scope.filled(1, 0)?;
            r = scope.copy(&self.limbs)?;
        } else if nb == 1 {
            q = scope.filled(na, 0)?;
            copy_words(&a, &mut q, &scope)?;
            let remainder = div_word(&mut q, b.word(0), &scope)?;
            r = scope.filled(1, remainder)?;
            apply_sign(&mut q, a.negative ^ b.negative, &scope)?;
            apply_sign(&mut r, a.negative, &scope)?;
        } else {
            let mut dividend = scope.filled(na + 1, 0)?;
            let mut divisor = scope.filled(nb, 0)?;
            copy_words(&a, &mut dividend[..na], &scope)?;
            copy_words(&b, &mut divisor, &scope)?;

            // Normalize the divisor so its top bit is set for quotient estimates.
            let shift = divisor[nb - 1].leading_zeros();
            if shift != 0 {
                shift_words(&mut divisor, shift, &scope)?;
                dividend[na] = shift_words(&mut dividend[..na], shift, &scope)?;
            }
            q = scope.filled(na - nb + 1, 0)?;
            div_normalized(&mut dividend, &divisor, &mut q, &scope)?;
            drop(divisor);
            r = scope.filled(nb, 0)?;
            for i in 0..nb {
                scope.step()?;
                r[i] = if shift == 0 {
                    dividend[i]
                } else {
                    (dividend[i] >> shift) | (dividend[i + 1] << (u64::BITS - shift))
                };
            }
            drop(dividend);
            apply_sign(&mut q, a.negative ^ b.negative, &scope)?;
            apply_sign(&mut r, a.negative, &scope)?;
        }
        // Keep both charges in the scope until both results are ready.
        normalize(&mut q)?;
        normalize(&mut r)?;
        Ok((
            Self {
                limbs: q.into_vec(),
            },
            Self {
                limbs: r.into_vec(),
            },
        ))
    }

    pub fn pow(&self, exp: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        if exp.is_negative() {
            return Err(BigIntError::NegativeExponent);
        }
        let scope = Scope::new(host);
        scope.poll()?;
        if exp.is_zero() {
            return Self::small(1, &scope);
        }
        if let Some(v @ -1..=1) = self.to_i64_exact() {
            return Self::small(if v == -1 && exp.is_even() { 1 } else { v }, &scope);
        }
        let a = Magnitude::new(&self.limbs);
        let high_bit = a.bit_len() - 1;
        let e = exp.to_i64_exact().ok_or(TOO_LARGE)? as u64;
        if e > ((MAX_BITS - 1) / high_bit) as u64 {
            return Err(TOO_LARGE);
        }
        if !a.any_below(high_bit) {
            let unit = if a.negative && !exp.is_even() {
                u64::MAX
            } else {
                1
            };
            return Self::finish(left_words(&[unit], high_bit * e as usize, &scope)?);
        }
        let mut result = scope.copy(&self.limbs)?;
        for i in (0..e.ilog2()).rev() {
            result = mul_words(&result, &result, &scope)?;
            if (e >> i) & 1 != 0 {
                result = mul_words(&result, &self.limbs, &scope)?;
            }
        }
        Ok(Self {
            limbs: result.into_vec(),
        })
    }

    pub fn shl(&self, shift: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.shift(shift, false, host)
    }

    pub fn sar(&self, shift: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.shift(shift, true, host)
    }

    fn shift(
        &self,
        shift: &Self,
        right: bool,
        host: &mut dyn BigIntHost,
    ) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        let amount = shift.to_i64_exact().map_or(u64::MAX, i64::unsigned_abs);
        if self.is_zero() {
            return Self::small(0, &scope);
        }
        if right ^ shift.is_negative() {
            if amount >= (self.limbs.len() * WORD_BITS) as u64 {
                return Self::small(-i64::from(self.is_negative()), &scope);
            }
            let amount = amount as usize;
            let whole = amount / WORD_BITS;
            let part = amount % WORD_BITS;
            let mut result = scope.filled(self.limbs.len() - whole, 0)?;
            for (i, out) in result.iter_mut().enumerate() {
                *out = self.limbs[i + whole] >> part;
                if part != 0 {
                    *out |= extend(&self.limbs, i + whole + 1) << (WORD_BITS - part);
                }
            }
            Self::finish(result)
        } else {
            if amount > (MAX_BITS - self.bit_len()) as u64 {
                return Err(TOO_LARGE);
            }
            Self::finish(left_words(&self.limbs, amount as usize, &scope)?)
        }
    }

    pub fn bitand(&self, other: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.logic(other, |a, b| a & b, host)
    }

    pub fn bitor(&self, other: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.logic(other, |a, b| a | b, host)
    }

    pub fn bitxor(&self, other: &Self, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.logic(other, |a, b| a ^ b, host)
    }

    fn logic(
        &self,
        other: &Self,
        op: impl Fn(u64, u64) -> u64,
        host: &mut dyn BigIntHost,
    ) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        let mut result = scope.filled(self.limbs.len().max(other.limbs.len()), 0)?;
        for (i, out) in result.iter_mut().enumerate() {
            *out = op(extend(&self.limbs, i), extend(&other.limbs, i));
        }
        Self::finish(result)
    }

    pub fn as_int_n(&self, bits: u64, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.as_n(bits, true, host)
    }

    pub fn as_uint_n(&self, bits: u64, host: &mut dyn BigIntHost) -> Result<Self, BigIntError> {
        self.as_n(bits, false, host)
    }

    fn as_n(
        &self,
        bits: u64,
        signed: bool,
        host: &mut dyn BigIntHost,
    ) -> Result<Self, BigIntError> {
        let scope = Scope::new(host);
        if bits == 0 {
            return Self::small(0, &scope);
        }
        if (bits >= (self.limbs.len() * WORD_BITS) as u64 || bits > MAX_BITS as u64)
            && (signed || !self.is_negative())
        {
            return self.copy(&scope);
        }
        if bits > MAX_BITS as u64 {
            return Err(TOO_LARGE);
        }
        let bits = bits as usize;
        let len = bits.div_ceil(WORD_BITS);
        let mut result = scope.filled(len, 0)?;
        for (i, out) in result.iter_mut().enumerate() {
            *out = extend(&self.limbs, i);
        }
        let shift = len * WORD_BITS - bits;
        let top = result[len - 1] << shift;
        result[len - 1] = if signed {
            ((top as i64) >> shift) as u64
        } else {
            top >> shift
        };
        if !signed && negative(&result) {
            result.push(0)?;
        }
        Self::finish(result)
    }
}

fn add_words(
    a: &[u64],
    b: &[u64],
    subtract: bool,
    scope: &Scope<'_>,
) -> Result<BigInt, BigIntError> {
    let n = a.len().max(b.len());
    let mut result = scope.filled(n, 0)?;
    let inv = if subtract { u64::MAX } else { 0 };
    let mut carry = u128::from(subtract);
    for (i, out) in result.iter_mut().enumerate() {
        let sum = extend(a, i) as u128 + (extend(b, i) ^ inv) as u128 + carry;
        *out = sum as u64;
        carry = sum >> WORD_BITS;
    }
    let extra = sign_word(a)
        .wrapping_add(sign_word(b) ^ inv)
        .wrapping_add(carry as u64);
    if extra != sign_word(&result) {
        result.push(extra)?;
    }
    BigInt::finish(result)
}

pub(super) fn left_words<'s, 'h>(
    a: &[u64],
    amount: usize,
    scope: &'s Scope<'h>,
) -> Result<Buffer<'s, 'h, u64>, BigIntError> {
    let whole = amount / WORD_BITS;
    let part = amount % WORD_BITS;
    let len = a.len() + whole;
    let mut result = scope.filled(len, 0)?;
    result[whole..].copy_from_slice(a);
    if part != 0 {
        let high = shift_words(&mut result[whole..], part as u32, scope)?;
        let extra = high | (sign_word(a) << part);
        if extra != sign_word(&result) {
            result.push(extra)?;
        }
    }
    Ok(result)
}

fn shift_words(words: &mut [u64], shift: u32, scope: &Scope<'_>) -> Result<u64, BigIntError> {
    let mut carry = 0;
    for word in words {
        scope.step()?;
        let next = *word >> (u64::BITS - shift);
        *word = (*word << shift) | carry;
        carry = next;
    }
    Ok(carry)
}

fn mul_words<'s, 'h>(
    a: &[u64],
    b: &[u64],
    scope: &'s Scope<'h>,
) -> Result<Buffer<'s, 'h, u64>, BigIntError> {
    scope.poll()?;
    if a == [0] || b == [0] {
        return scope.filled(1, 0);
    }
    if Magnitude::new(a).bit_len() + Magnitude::new(b).bit_len() - 1 > MAX_BITS {
        return Err(TOO_LARGE);
    }
    let mut result = scope.filled(a.len() + b.len(), 0)?;
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0;
        for (r, &y) in result[i..].iter_mut().zip(b) {
            scope.step()?;
            let product = x as u128 * y as u128 + *r as u128 + carry;
            *r = product as u64;
            carry = product >> WORD_BITS;
        }
        result[i + b.len()] = carry as u64;
    }
    // Correct the unsigned product for the implicit negative high limbs.
    if negative(a) {
        sub_mul(&mut result[a.len()..], b, 1, scope)?;
    }
    if negative(b) {
        sub_mul(&mut result[b.len()..], a, 1, scope)?;
    }
    normalize(&mut result)?;
    Ok(result)
}

fn sub_mul(
    out: &mut [u64],
    words: &[u64],
    factor: u64,
    scope: &Scope<'_>,
) -> Result<u64, BigIntError> {
    let mut borrow = 0;
    for (r, &word) in out.iter_mut().zip(words) {
        scope.step()?;
        let product = word as u128 * factor as u128 + borrow;
        let (diff, underflow) = r.overflowing_sub(product as u64);
        *r = diff;
        borrow = (product >> WORD_BITS) + u128::from(underflow);
    }
    Ok(borrow as u64)
}

pub(super) fn div_word(words: &mut [u64], d: u64, scope: &Scope<'_>) -> Result<u64, BigIntError> {
    let mut rem = 0;
    for word in words.iter_mut().rev() {
        scope.step()?;
        let n = (rem << WORD_BITS) | *word as u128;
        *word = (n / d as u128) as u64;
        rem = n % d as u128;
    }
    Ok(rem as u64)
}

fn div_normalized(
    dividend: &mut [u64],
    divisor: &[u64],
    quotient: &mut [u64],
    scope: &Scope<'_>,
) -> Result<(), BigIntError> {
    let n = divisor.len();
    let d = divisor[n - 1];
    for (i, q) in quotient.iter_mut().enumerate().rev() {
        let high = dividend[i + n];
        let next = dividend[i + n - 1];
        let mut estimate = if high >= d {
            u64::MAX
        } else {
            ((((high as u128) << WORD_BITS) | next as u128) / d as u128) as u64
        };
        let window = &mut dividend[i..=i + n];
        let borrow = sub_mul(&mut window[..n], divisor, estimate, scope)?;
        let (top, mut underflow) = high.overflowing_sub(borrow);
        window[n] = top;
        // The estimate can be too high. Add back the divisor until it fits.
        while underflow {
            estimate -= 1;
            let mut carry = 0;
            for (word, &v) in window.iter_mut().zip(divisor) {
                scope.step()?;
                let sum = *word as u128 + v as u128 + carry;
                *word = sum as u64;
                carry = sum >> WORD_BITS;
            }
            let (top, overflow) = window[n].overflowing_add(carry as u64);
            window[n] = top;
            underflow = !overflow;
        }
        *q = estimate;
    }
    Ok(())
}

pub(super) fn copy_words(
    a: &Magnitude<'_>,
    out: &mut [u64],
    scope: &Scope<'_>,
) -> Result<(), BigIntError> {
    for (i, word) in out.iter_mut().enumerate() {
        scope.step()?;
        *word = a.word(i);
    }
    Ok(())
}

pub(super) fn apply_sign(
    words: &mut Buffer<'_, '_, u64>,
    neg: bool,
    scope: &Scope<'_>,
) -> Result<(), BigIntError> {
    if neg {
        let mut carry = true;
        for word in words.iter_mut() {
            scope.step()?;
            (*word, carry) = (!*word).overflowing_add(u64::from(carry));
        }
        if !carry && !negative(words) {
            words.push(u64::MAX)?;
        }
    } else if negative(words) {
        words.push(0)?;
    }
    Ok(())
}
