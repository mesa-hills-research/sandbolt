use std::cmp::Ordering;

use qjs_bigint::{BigInt, BigIntError, BigIntHost};
use qjs_bytecode::Opcode;

use crate::heap::{BigIntData, BigIntLimbs, Heap};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedVec};
use crate::object::ErrorKind;
use crate::ops::convert::{self, PreferredType};
use crate::root::{ExecCtx, Handle};
use crate::value::*;

struct Host<'a> {
    ledger: &'a HeapLedger,
    handler: &'a mut Option<Box<dyn FnMut() -> bool>>,
    charged: usize,
    failpoint: bool,
}

impl Host<'_> {
    fn map_alloc(&mut self, error: AllocError) -> BigIntError {
        if error == AllocError::Failpoint {
            self.failpoint = true;
        }
        BigIntError::OutOfMemory
    }
}

impl BigIntHost for Host<'_> {
    fn charge(&mut self, bytes: usize) -> Result<(), BigIntError> {
        self.ledger
            .charge_bytes(bytes)
            .map_err(|e| self.map_alloc(e))?;
        self.charged += bytes;
        Ok(())
    }
    fn release(&mut self, bytes: usize) {
        self.ledger.release_bytes(bytes);
        self.charged -= bytes;
    }
    fn poll_interrupt(&mut self) -> bool {
        self.handler.as_mut().is_some_and(|handler| handler())
    }
}

impl Drop for Host<'_> {
    fn drop(&mut self) {
        self.ledger.release_bytes(self.charged);
    }
}

fn with_host<R>(
    ctx: &mut ExecCtx<'_, '_>,
    mut run: impl FnMut(&mut Heap, &mut Host<'_>) -> Result<R, BigIntError>,
) -> JsResult<R> {
    ctx.runtime.allocation_point();
    for attempt in 0..2 {
        let (result, failpoint) = {
            let ledger = ctx.heap().ledger.clone();
            let mut host = Host {
                ledger: &ledger,
                handler: &mut ctx.runtime.vm.interrupt_handler,
                charged: 0,
                failpoint: false,
            };
            let result = run(&mut ctx.runtime.heap, &mut host);
            (result, host.failpoint)
        };
        match result {
            Ok(value) => return Ok(value),
            Err(BigIntError::OutOfMemory) if attempt == 0 && !failpoint => ctx.collect(),
            Err(error) => {
                return Err(match error {
                    BigIntError::OutOfMemory => ctx.throw_oom(),
                    BigIntError::Interrupted => {
                        ctx.take_exception();
                        Abrupt::Interrupted
                    }
                    BigIntError::RangeError(text) => ctx.throw_range_error(text),
                    BigIntError::DivisionByZero => ctx.throw_range_error("division by zero"),
                    BigIntError::NegativeExponent => ctx.throw_range_error("negative exponent"),
                    BigIntError::InvalidString => {
                        ctx.throw_error(ErrorKind::Syntax, "invalid BigInt literal")
                    }
                });
            }
        }
    }
    unreachable!()
}

fn read(heap: &Heap, host: &mut Host<'_>, value: Value) -> Result<BigInt, BigIntError> {
    let data = match value.decode() {
        ValueKind::ShortBigInt(n) => return BigInt::from_i64_charged(n, host),
        ValueKind::BigInt(id) => heap.bigints.get(id).unwrap(),
        _ => unreachable!(),
    };
    BigInt::from_sign_and_u32_limbs(data.negative, &data.limbs, host)
}

pub(crate) fn store_bigint(heap: &mut Heap, bigint: &BigInt) -> AllocResult<Value> {
    if let Some(value) = bigint.to_i64_exact().and_then(Value::short_bigint) {
        return Ok(value);
    }
    store_magnitude(heap, bigint.is_negative(), bigint.magnitude_u32_limbs())
}

fn store_magnitude(
    heap: &mut Heap,
    negative: bool,
    words: impl ExactSizeIterator<Item = u32>,
) -> AllocResult<Value> {
    let len = words.len();
    let limbs = if len <= INLINE_WORDS {
        let mut inline = [0; INLINE_WORDS];
        for (dst, word) in inline.iter_mut().zip(words) {
            *dst = word;
        }
        BigIntLimbs::Inline {
            len: len as u8,
            words: inline,
        }
    } else {
        let mut limbs = TrackedVec::new();
        limbs.try_reserve_exact(&heap.ledger, len)?;
        for word in words {
            limbs.push_reserved(word);
        }
        BigIntLimbs::Heap(limbs)
    };
    store_limbs(heap, negative, limbs)
}

fn store_limbs(heap: &mut Heap, negative: bool, limbs: BigIntLimbs) -> AllocResult<Value> {
    heap.bigints
        .insert(&heap.ledger, BigIntData { negative, limbs })
        .map(Value::bigint)
}

fn store(heap: &mut Heap, host: &mut Host<'_>, bigint: &BigInt) -> Result<Value, BigIntError> {
    store_bigint(heap, bigint).map_err(|e| host.map_alloc(e))
}

pub(super) fn parse_bigint(ctx: &mut ExecCtx<'_, '_>, text: &str) -> JsResult<Value> {
    with_host(ctx, |heap, host| {
        let value = BigInt::from_str_js(text, host)?;
        store(heap, host, &value)
    })
}

pub(crate) fn bigint_text(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<Value> {
    bigint_radix_text(ctx, value, 10)
}

pub(crate) fn bigint_radix_text(
    ctx: &mut ExecCtx<'_, '_>,
    value: Value,
    radix: u32,
) -> JsResult<Value> {
    with_host(ctx, |heap, host| {
        let value = read(heap, host, value)?;
        let text = value.to_string_radix(radix, host)?;
        let string = crate::string::JsString::from_latin1(&heap.ledger, text.as_bytes())
            .map_err(|e| host.map_alloc(e))?;
        let id = heap
            .strings
            .insert(&heap.ledger, string)
            .map_err(|e| host.map_alloc(e))?;
        Ok(Value::string(id))
    })
}

pub(crate) fn bigint_as_n(
    ctx: &mut ExecCtx<'_, '_>,
    value: Value,
    signed: bool,
    bits: u64,
) -> JsResult<Value> {
    with_host(ctx, |heap, host| {
        let value = read(heap, host, value)?;
        let result = if signed {
            value.as_int_n(bits, host)?
        } else {
            value.as_uint_n(bits, host)?
        };
        store(heap, host, &result)
    })
}

pub(crate) fn bigint_to_number(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<f64> {
    with_host(ctx, |heap, host| Ok(read(heap, host, value)?.to_f64()))
}

pub(crate) fn compare_big_string(
    ctx: &mut ExecCtx<'_, '_>,
    value: Value,
    string: StrId,
) -> JsResult<Option<std::cmp::Ordering>> {
    with_host(ctx, |heap, host| {
        let mut bytes = TrackedVec::new();
        let mut units = heap
            .string_units(string)
            .skip_while(|&unit| convert::space(unit));
        let mut trimmed = 0;
        for unit in &mut units {
            if unit > 127 {
                if !convert::space(unit) {
                    return Ok(None);
                }
                bytes
                    .push(&heap.ledger, b' ')
                    .map_err(|_| BigIntError::OutOfMemory)?;
            } else {
                bytes
                    .push(&heap.ledger, unit as u8)
                    .map_err(|_| BigIntError::OutOfMemory)?;
            }
            if !convert::space(unit) {
                trimmed = bytes.len();
            }
        }
        let text = std::str::from_utf8(&bytes[..trimmed]).unwrap();
        let other = match BigInt::from_str_js(text, host) {
            Ok(value) => value,
            Err(BigIntError::InvalidString) => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut short = [0; 2];
        if host.handler.is_some() {
            let value = read(heap, host, value)?;
            return Ok(Some(value.cmp_bigint(&other)));
        }
        let Some((negative, words)) = inline_magnitude(heap, value, &mut short) else {
            let value = read(heap, host, value)?;
            return Ok(Some(value.cmp_bigint(&other)));
        };
        Ok(Some(match (negative, other.is_negative()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => {
                let other = other.magnitude_u32_limbs();
                let order = words.len().cmp(&other.len()).then_with(|| {
                    // Words arrive least significant first, so keep the last difference.
                    words
                        .iter()
                        .copied()
                        .zip(other)
                        .fold(Ordering::Equal, |order, (a, b)| a.cmp(&b).then(order))
                });
                if negative { order.reverse() } else { order }
            }
        }))
    })
}

fn bigint(value: Value) -> bool {
    matches!(
        value.decode(),
        ValueKind::BigInt(_) | ValueKind::ShortBigInt(_)
    )
}

enum Numeric<'s> {
    Number(f64),
    BigInt(Handle<'s>),
}

fn to_numeric<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Numeric<'s>> {
    let primitive = convert::to_primitive(ctx, value, PreferredType::Number)?;
    let primitive = ctx.root(primitive)?;
    primitive_to_numeric(ctx, primitive)
}

fn primitive_to_numeric<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Numeric<'s>> {
    if bigint(ctx.get(value)) {
        Ok(Numeric::BigInt(value))
    } else {
        convert::to_number(ctx, value).map(Numeric::Number)
    }
}

pub(super) fn binary<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    op: Opcode,
    left: Handle<'s>,
    right: Handle<'s>,
) -> JsResult<Value> {
    use Opcode::*;
    let (a, b) = (ctx.get(left), ctx.get(right));
    if let (Some(a), Some(b)) = (a.as_number(), b.as_number()) {
        return Ok(number_binary(op, a, b));
    }
    if bigint(a) && bigint(b) {
        return bigint_binary(ctx, op, a, b);
    }
    let (left, right) = if op == Add {
        let a = convert::to_primitive(ctx, left, PreferredType::Default)?;
        let left = ctx.root(a)?;
        let b = convert::to_primitive(ctx, right, PreferredType::Default)?;
        let right = ctx.root(b)?;
        if matches!(a.decode(), ValueKind::String(_)) || matches!(b.decode(), ValueKind::String(_))
        {
            let a = convert::to_string(ctx, left)?;
            let b = convert::to_string(ctx, right)?;
            let joined = ctx.concat_strings(a, b)?;
            return Ok(ctx.get(joined));
        }
        (
            primitive_to_numeric(ctx, left)?,
            primitive_to_numeric(ctx, right)?,
        )
    } else {
        (to_numeric(ctx, left)?, to_numeric(ctx, right)?)
    };
    let (a, b) = match (left, right) {
        (Numeric::Number(a), Numeric::Number(b)) => (a, b),
        (Numeric::BigInt(a), Numeric::BigInt(b)) => {
            return bigint_binary(ctx, op, ctx.get(a), ctx.get(b));
        }
        _ => return Err(ctx.throw_type_error("cannot mix BigInt and Number")),
    };
    Ok(number_binary(op, a, b))
}

#[inline]
pub(super) fn short_binary(op: Opcode, a: i64, b: i64, has_interrupt: bool) -> Option<Value> {
    use Opcode::*;
    // The limb routines for these operators poll even on one-word operands.
    if has_interrupt && matches!(op, Mul | Div | Mod | Pow) {
        return None;
    }
    let result = match op {
        Add => a + b,
        Sub => a - b,
        Mul => a.checked_mul(b)?,
        Div => a.checked_div(b)?,
        Mod => a.checked_rem(b)?,
        Pow => a.checked_pow(u32::try_from(b).ok()?)?,
        And => a & b,
        Or => a | b,
        Xor => a ^ b,
        Shl | Sar => {
            let shift = b.unsigned_abs();
            if (op == Sar) ^ (b < 0) {
                a >> shift.min(63)
            } else if a == 0 {
                0
            } else if shift < 47 {
                i64::try_from(i128::from(a) << shift).ok()?
            } else {
                return None;
            }
        }
        Shr => return None,
        _ => unreachable!(),
    };
    Value::short_bigint(result)
}

const INLINE_WORDS: usize = BigIntLimbs::INLINE_LEN;
type InlineWords = [u32; INLINE_WORDS];

type Magnitude<'a> = (bool, &'a [u32]);

fn trim(words: &[u32]) -> &[u32] {
    let len = words
        .iter()
        .rposition(|&word| word != 0)
        .map_or(0, |i| i + 1);
    &words[..len]
}

fn inline_magnitude<'a>(
    heap: &'a Heap,
    value: Value,
    short: &'a mut [u32; 2],
) -> Option<Magnitude<'a>> {
    match value.decode() {
        ValueKind::ShortBigInt(n) => {
            let m = n.unsigned_abs();
            *short = [m as u32, (m >> 32) as u32];
            let len = usize::from(m != 0) + usize::from(short[1] != 0);
            Some((n < 0, &short[..len]))
        }
        ValueKind::BigInt(id) => {
            let data = heap.bigints.get(id).unwrap();
            let words = &*data.limbs;
            if words.len() > INLINE_WORDS {
                return None;
            }
            let words = if words.last() == Some(&0) {
                trim(words)
            } else {
                words
            };
            Some((data.negative && !words.is_empty(), words))
        }
        _ => unreachable!(),
    }
}

fn store_inline(ctx: &mut ExecCtx<'_, '_>, negative: bool, words: InlineWords) -> JsResult<Value> {
    let len = trim(&words).len();
    if len <= 2 && words[1] <= (1 << 14) {
        let m = low_u64(&words) as i64;
        if let Some(v) = Value::short_bigint(if negative { -m } else { m }) {
            return Ok(v);
        }
    }
    ctx.allocate(|rt| {
        store_limbs(
            &mut rt.heap,
            negative,
            BigIntLimbs::Inline {
                len: len as u8,
                words,
            },
        )
    })
}

fn low_u64(words: &[u32]) -> u64 {
    match words {
        [lo, hi, ..] => u64::from(*lo) | (u64::from(*hi) << 32),
        [lo] => u64::from(*lo),
        [] => 0,
    }
}

fn write_u128(words: &mut [u32], n: u128) {
    words[..4].copy_from_slice(&[
        n as u32,
        (n >> 32) as u32,
        (n >> 64) as u32,
        (n >> 96) as u32,
    ]);
}

fn compare_words(a: &[u32], b: &[u32]) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn inline_add(
    out: &mut InlineWords,
    (a_neg, a): Magnitude<'_>,
    (b_neg, b): Magnitude<'_>,
) -> Option<bool> {
    if a_neg == b_neg {
        let (long, short) = if a.len() < b.len() { (b, a) } else { (a, b) };
        let (head, tail) = long.split_at(short.len());
        let mut carry = 0;
        for ((&a, &b), word) in head.iter().zip(short).zip(&mut *out) {
            carry += u64::from(a) + u64::from(b);
            *word = carry as u32;
            carry >>= 32;
        }
        out[head.len()..long.len()].copy_from_slice(tail);
        if carry != 0 {
            for word in out.get_mut(head.len()..)? {
                *word = word.wrapping_add(1);
                if *word != 0 {
                    return Some(a_neg);
                }
            }
            return None;
        }
        return Some(a_neg);
    }
    let (negative, long, short) = if compare_words(a, b) == Ordering::Less {
        (b_neg, b, a)
    } else {
        (a_neg, a, b)
    };
    let mut borrow = 0;
    for (i, &word) in long.iter().enumerate() {
        borrow =
            u64::from(word).wrapping_sub(u64::from(short.get(i).copied().unwrap_or(0)) + borrow);
        out[i] = borrow as u32;
        borrow >>= 63;
    }
    Some(negative)
}

fn inline_mul(
    out: &mut InlineWords,
    (a_neg, a): Magnitude<'_>,
    (b_neg, b): Magnitude<'_>,
) -> Option<bool> {
    if a.is_empty() || b.is_empty() {
        return Some(false);
    }
    if a.len().max(b.len()) <= 4 {
        let a_lo = u128::from(low_u64(a));
        let b_lo = u128::from(low_u64(b));
        let lo = a_lo * b_lo;
        if a.len().max(b.len()) <= 2 {
            write_u128(out, lo);
        } else {
            let a_hi = u128::from(low_u64(a.get(2..).unwrap_or(&[])));
            let b_hi = u128::from(low_u64(b.get(2..).unwrap_or(&[])));
            let ab = a_lo * b_hi;
            let ba = a_hi * b_lo;
            let mid = (lo >> 64) + u128::from(ab as u64) + u128::from(ba as u64);
            let hi = a_hi * b_hi + (ab >> 64) + (ba >> 64) + (mid >> 64);
            write_u128(out, u128::from(lo as u64) | (mid << 64));
            write_u128(&mut out[4..], hi);
        }
        return Some(a_neg ^ b_neg);
    }
    if a.len() + b.len() > INLINE_WORDS + 1 {
        return None;
    }
    for (i, &a) in a.iter().enumerate() {
        let mut carry = 0;
        for (&b, word) in b.iter().zip(&mut out[i..]) {
            carry += u64::from(a) * u64::from(b) + u64::from(*word);
            *word = carry as u32;
            carry >>= 32;
        }
        if carry != 0 {
            *out.get_mut(i + b.len())? = carry as u32;
        }
    }
    Some(a_neg ^ b_neg)
}

fn complement_word(word: u32, carry: &mut bool) -> u32 {
    let (word, next) = (!word).overflowing_add(u32::from(*carry));
    *carry = next;
    word
}

fn inline_logic(
    out: &mut InlineWords,
    op: Opcode,
    (a_neg, a): Magnitude<'_>,
    (b_neg, b): Magnitude<'_>,
) -> Option<bool> {
    let logic = |a, b| match op {
        Opcode::And => a & b,
        Opcode::Or => a | b,
        Opcode::Xor => a ^ b,
        _ => unreachable!(),
    };
    let negative = logic(a_neg as u32, b_neg as u32) != 0;
    let (mut a_carry, mut b_carry, mut carry) = (true, true, true);
    let len = a.len().max(b.len());
    for (i, word) in out[..len].iter_mut().enumerate() {
        let mut x = a.get(i).copied().unwrap_or(0);
        let mut y = b.get(i).copied().unwrap_or(0);
        if a_neg {
            x = complement_word(x, &mut a_carry);
        }
        if b_neg {
            y = complement_word(y, &mut b_carry);
        }
        let result = logic(x, y);
        *word = if negative {
            complement_word(result, &mut carry)
        } else {
            result
        };
    }
    if negative && carry {
        *out.get_mut(len)? = 1;
    }
    Some(negative)
}

fn inline_shift(
    out: &mut InlineWords,
    right: bool,
    (negative, words): Magnitude<'_>,
    (reverse, shift): Magnitude<'_>,
) -> Option<bool> {
    let Some(&high) = words.last() else {
        return Some(false);
    };
    let bit_len = words.len() * 32 - high.leading_zeros() as usize;
    let shift = match shift {
        [] => 0,
        [n] => *n as usize,
        _ => usize::MAX,
    };
    let whole = shift / 32;
    let bits = shift % 32;
    if right ^ reverse {
        if shift >= bit_len {
            out[0] = u32::from(negative);
            return Some(negative);
        }
        let words_high = &words[whole..];
        for (i, &word) in words_high.iter().enumerate() {
            out[i] = word >> bits;
            if bits != 0 {
                out[i] |= words_high.get(i + 1).copied().unwrap_or(0) << (32 - bits);
            }
        }
        // A negative arithmetic shift rounds toward minus infinity.
        if negative
            && ((bits != 0 && words_high[0] << (32 - bits) != 0)
                || words[..whole].iter().any(|&n| n != 0))
        {
            for word in out {
                *word = word.wrapping_add(1);
                if *word != 0 {
                    break;
                }
            }
        }
    } else {
        if shift > INLINE_WORDS * 32 - bit_len {
            return None;
        }
        for (i, &word) in words.iter().enumerate() {
            out[i + whole] |= word << bits;
            if bits != 0
                && let Some(next) = out.get_mut(i + whole + 1)
            {
                *next = word >> (32 - bits);
            }
        }
    }
    Some(negative)
}

fn inline_binary(heap: &Heap, op: Opcode, a: Value, b: Value) -> Option<(bool, InlineWords)> {
    let (mut a_short, mut b_short) = ([0; 2], [0; 2]);
    let a = inline_magnitude(heap, a, &mut a_short)?;
    let b = inline_magnitude(heap, b, &mut b_short)?;
    inline_binary_words(op, a, b)
}

fn inline_binary_words(
    op: Opcode,
    a: Magnitude<'_>,
    b: Magnitude<'_>,
) -> Option<(bool, InlineWords)> {
    use Opcode::*;
    let mut words = [0; INLINE_WORDS];
    let negative = match op {
        Add => inline_add(&mut words, a, b),
        Sub => inline_add(&mut words, a, (!b.0, b.1)),
        Mul => inline_mul(&mut words, a, b),
        And | Or | Xor => inline_logic(&mut words, op, a, b),
        Shl | Sar => inline_shift(&mut words, op == Sar, a, b),
        _ => return None,
    }?;
    Some((negative, words))
}

fn inline_unary(heap: &Heap, op: Opcode, value: Value) -> Option<(bool, InlineWords)> {
    use Opcode::*;
    let mut short = [0; 2];
    let (negative, magnitude) = inline_magnitude(heap, value, &mut short)?;
    let mut words = [0; INLINE_WORDS];
    let negative = match op {
        Neg => {
            words[..magnitude.len()].copy_from_slice(magnitude);
            !negative
        }
        Not => inline_add(&mut words, (!negative, magnitude), (true, &[1]))?,
        Inc | PostInc => inline_add(&mut words, (negative, magnitude), (false, &[1]))?,
        Dec | PostDec => inline_add(&mut words, (negative, magnitude), (true, &[1]))?,
        _ => unreachable!(),
    };
    Some((negative, words))
}

fn bigint_binary(ctx: &mut ExecCtx<'_, '_>, op: Opcode, a: Value, b: Value) -> JsResult<Value> {
    use Opcode::*;
    if op == Shr {
        return Err(ctx.throw_type_error("BigInt has no unsigned right shift"));
    }
    if let (ValueKind::ShortBigInt(a), ValueKind::ShortBigInt(b)) = (a.decode(), b.decode())
        && let Some(result) = short_binary(op, a, b, ctx.runtime.vm.interrupt_handler.is_some())
    {
        return Ok(result);
    }
    // The general bridge polls when reading arena values.
    if ctx.runtime.vm.interrupt_handler.is_none()
        && let Some((negative, words)) = inline_binary(ctx.heap(), op, a, b)
    {
        return store_inline(ctx, negative, words);
    }
    with_host(ctx, |heap, host| {
        let a = read(heap, host, a)?;
        let b = read(heap, host, b)?;
        let result = match op {
            Add => a.add(&b, host)?,
            Sub => a.sub(&b, host)?,
            Mul => a.mul(&b, host)?,
            Div => a.divrem(&b, host)?.0,
            Mod => a.divrem(&b, host)?.1,
            Pow => a.pow(&b, host)?,
            Shl => a.shl(&b, host)?,
            Sar => a.sar(&b, host)?,
            And => a.bitand(&b, host)?,
            Or => a.bitor(&b, host)?,
            Xor => a.bitxor(&b, host)?,
            _ => unreachable!(),
        };
        store(heap, host, &result)
    })
}

#[inline]
pub(super) fn number_binary(op: Opcode, a: f64, b: f64) -> Value {
    use Opcode::*;
    if matches!(op, Shl | Sar | Shr | And | Or | Xor) {
        let a = convert::number_to_uint32(a);
        let b = convert::number_to_uint32(b);
        return match op {
            Shl => Value::int32((a << (b & 31)) as i32),
            Sar => Value::int32((a as i32) >> (b & 31)),
            Shr => Value::number(f64::from(a >> (b & 31))),
            And => Value::int32((a & b) as i32),
            Or => Value::int32((a | b) as i32),
            Xor => Value::int32((a ^ b) as i32),
            _ => unreachable!(),
        };
    }
    Value::number(match op {
        Add => a + b,
        Sub => a - b,
        Mul => a * b,
        Div => a / b,
        Mod => a % b,
        Pow if b.is_nan() || (a.abs() == 1.0 && b.is_infinite()) => f64::NAN,
        Pow => a.powf(b),
        _ => unreachable!(),
    })
}

pub(super) fn unary<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    op: Opcode,
    value: Handle<'s>,
) -> JsResult<(Value, Value)> {
    use Opcode::*;
    let raw = convert::to_primitive(ctx, value, PreferredType::Number)?;
    if let Some(number) = raw.as_number() {
        return Ok(number_unary(op, number));
    }
    if let ValueKind::ShortBigInt(n) = raw.decode()
        && let Some(result) = short_unary(op, n)
    {
        return Ok((raw, result));
    }
    let value = ctx.root(raw)?;
    if bigint(raw) {
        if op == Plus {
            return Err(ctx.throw_type_error("cannot convert BigInt to number"));
        }
        if ctx.runtime.vm.interrupt_handler.is_none()
            && let Some((negative, words)) = inline_unary(ctx.heap(), op, raw)
        {
            return Ok((raw, store_inline(ctx, negative, words)?));
        }
        let result = with_host(ctx, |heap, host| {
            let integer = read(heap, host, raw)?;
            let result = match op {
                Neg => integer.neg(host)?,
                Not => integer.bitnot(host)?,
                Inc | PostInc | Dec | PostDec => {
                    let one = BigInt::from_i64_charged(
                        if matches!(op, Inc | PostInc) { 1 } else { -1 },
                        host,
                    )?;
                    integer.add(&one, host)?
                }
                _ => unreachable!(),
            };
            store(heap, host, &result)
        })?;
        Ok((raw, result))
    } else {
        Ok(number_unary(op, convert::to_number(ctx, value)?))
    }
}

#[inline]
pub(super) fn short_unary(op: Opcode, number: i64) -> Option<Value> {
    use Opcode::*;
    Value::short_bigint(match op {
        Neg => -number,
        Inc | PostInc | IncLoc => number + 1,
        Dec | PostDec | DecLoc => number - 1,
        Not => !number,
        Plus => return None,
        _ => unreachable!(),
    })
}

#[inline]
pub(super) fn number_unary(op: Opcode, number: f64) -> (Value, Value) {
    use Opcode::*;
    let result = match op {
        Neg => -number,
        Plus => number,
        Inc | PostInc => number + 1.0,
        Dec | PostDec => number - 1.0,
        Not => f64::from(!(convert::number_to_uint32(number) as i32)),
        _ => unreachable!(),
    };
    (Value::number(number), Value::number(result))
}

#[cfg(test)]
mod tests {
    use qjs_bigint::UnlimitedBigIntHost;

    use super::*;
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

    #[test]
    fn inline_bigint_binary_matches_general_arithmetic() {
        use Opcode::*;

        let host = &mut UnlimitedBigIntHost;
        let mut state = 0xd032_7356_b8c5_669fu64;
        let mut random = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u32
        };
        let mut check = |op, a: Magnitude<'_>, b: Magnitude<'_>| {
            let a_big = BigInt::from_sign_and_u32_limbs(a.0, a.1, host).unwrap();
            let b_big = BigInt::from_sign_and_u32_limbs(b.0, b.1, host).unwrap();
            let result = match op {
                Add => a_big.add(&b_big, host),
                Sub => a_big.sub(&b_big, host),
                Mul => a_big.mul(&b_big, host),
                And => a_big.bitand(&b_big, host),
                Or => a_big.bitor(&b_big, host),
                Xor => a_big.bitxor(&b_big, host),
                Shl => a_big.shl(&b_big, host),
                Sar => a_big.sar(&b_big, host),
                _ => unreachable!(),
            }
            .unwrap();
            let expected = result.magnitude_u32_limbs();
            match inline_binary_words(op, a, b) {
                Some((negative, words)) => {
                    let got = trim(&words);
                    assert_eq!(
                        negative && !got.is_empty(),
                        result.is_negative(),
                        "{op:?} {a:?} {b:?}"
                    );
                    assert!(
                        got.iter().copied().eq(expected),
                        "{op:?} {a:?} {b:?}: {got:?}"
                    );
                }
                None => assert!(expected.len() > INLINE_WORDS, "{op:?} {a:?} {b:?}"),
            }
        };
        let all = [u32::MAX; INLINE_WORDS];
        check(Add, (false, &all), (false, &[1]));
        check(And, (true, &all), (true, &[2]));
        check(Mul, (false, &all[..4]), (false, &all[..4]));
        check(Sar, (true, &all), (false, &[32]));
        check(Shl, (true, &all), (true, &[31]));
        for i in 0..4096 {
            let a = [(); INLINE_WORDS].map(|_| random());
            let b = [(); INLINE_WORDS].map(|_| random());
            let a = trim(&a[..i % 9]);
            let b = trim(&b[..(i / 9) % 9]);
            let x = (i & 1 != 0 && !a.is_empty(), a);
            let y = (i & 2 != 0 && !b.is_empty(), b);
            for op in [Add, Sub, Mul, And, Or, Xor] {
                check(op, x, y);
            }
            let shift = [random() % 300];
            let shift = (i & 2 != 0 && shift[0] != 0, trim(&shift));
            check(Shl, x, shift);
            check(Sar, x, shift);
        }
    }

    #[test]
    fn inline_bigint_charges_only_the_arena_slot() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let realm = engine.default_realm();
        engine.runtime_mut().with_scope(realm, |ctx| {
            let heap = &mut ctx.runtime.heap;
            assert_eq!(heap.bigints.capacity(), 0);
            heap.bigints.try_reserve_slot(&heap.ledger).unwrap();
            let charged = heap.ledger.used_bytes();
            let words = [u32::MAX; INLINE_WORDS];
            heap.ledger.fail_after(Some(0));
            let value = store_magnitude(heap, true, words.into_iter()).unwrap();
            let ValueKind::BigInt(id) = value.decode() else {
                unreachable!()
            };
            let data = heap.bigints.get(id).unwrap();
            assert!(matches!(data.limbs, BigIntLimbs::Inline { .. }));
            assert_eq!(&*data.limbs, &words);
            assert_eq!(heap.ledger.used_bytes(), charged);
            assert!(heap.ledger.failpoint_armed());
            let large = [u32::MAX; INLINE_WORDS + 1];
            assert_eq!(
                store_magnitude(heap, false, large.into_iter()),
                Err(AllocError::Failpoint)
            );
            assert_eq!(heap.ledger.used_bytes(), charged);
        });
    }

    #[test]
    fn inline_bigint_operations_keep_the_read_polls() {
        use std::{cell::Cell, rc::Rc};

        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let realm = engine.default_realm();
        engine.runtime_mut().with_scope(realm, |ctx| {
            let a = store_inline(ctx, false, [0, 0, 1, 0, 0, 0, 0, 0]).unwrap();
            ctx.root(a).unwrap();
            let b = store_inline(ctx, false, [5, 0, 1, 0, 0, 0, 0, 0]).unwrap();
            ctx.root(b).unwrap();
            for op in [Opcode::Add, Opcode::Mul, Opcode::And, Opcode::Sar] {
                let polls = Rc::new(Cell::new(0));
                ctx.runtime.set_interrupt_handler(Some(Box::new({
                    let polls = polls.clone();
                    move || {
                        polls.set(polls.get() + 1);
                        polls.get() == 2
                    }
                })));
                assert_eq!(bigint_binary(ctx, op, a, b), Err(Abrupt::Interrupted));
                assert_eq!(polls.get(), 2);
            }
        });
    }

    #[test]
    fn bigint_unary_and_comparisons_cross_inline_storage_boundaries() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let result = engine
            .eval_to_string(
                r#"
            const base = 1n << 255n;
            const limit = 1n << 256n;
            let pos = limit - 1n;
            let neg = -(base + 1n);
            const box = { valueOf() { return (1n << 255n) + 3n; } };
            [pos++ === limit - 1n, pos === limit, --pos === limit - 1n,
             neg++ === -base - 1n, neg === -base, -box === -base - 3n,
             ~(-base) === base - 1n, ~pos === -limit, (pos & -limit) === 0n,
             -((1n << 256n) - 1n) < String(-base), String(base + 1n) > base,
             String(base) == base, -1n >> 123456789123456789n === -1n].every(Boolean)
        "#,
                EvalOptions::default(),
            )
            .unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn bigint_binary_crosses_signed_128_bit_boundary() {
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        let result = engine
            .eval_to_string(
                r#"
            const x = 1n << 63n;
            const y = 1n << 127n;
            const z = 1n << 255n;
            [x*x === (1n<<126n), (-y)/(-1n) === y, (-y)%(-1n) === 0n,
             x<<64n === y, -x>>999n === -1n, (z+1n).toString(16).length,
             BigInt.asIntN(128, y) === -y, BigInt.asUintN(128, -y) === y,
             Number((1n<<53n)+1n), Number((1n<<53n)+3n)].join('|');
        "#,
                EvalOptions::default(),
            )
            .unwrap();
        assert_eq!(
            result,
            "true|true|true|true|true|64|true|true|9007199254740992|9007199254740996"
        );
    }
}
