use super::*;
use crate::heap::BigIntData;
use crate::ledger::TrackedVec;
use crate::value::ValueKind;

pub(super) fn encode<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    kind: TypedArrayKind,
    value: Handle<'s>,
) -> JsResult<u64> {
    if kind.is_bigint() {
        let int = to_bigint(ctx, value)?;
        Ok(bigint_bits(ctx.heap(), int))
    } else {
        Ok(number_bits(kind, convert::to_number(ctx, value)?))
    }
}

pub(super) fn number_bits(kind: TypedArrayKind, n: f64) -> u64 {
    use TypedArrayKind::*;
    match kind {
        Float16 => u64::from(to_half(n)),
        Float32 => u64::from((n as f32).to_bits()),
        Float64 => n.to_bits(),
        Uint8Clamped => {
            (if n.is_nan() {
                0.0
            } else {
                n.clamp(0.0, 255.0).round_ties_even()
            }) as u64
        }
        _ => {
            let modulus = (1_u64 << (kind.size() * 8)) as f64;
            let rem = n.trunc() % modulus;
            (if rem < 0.0 { rem + modulus } else { rem }) as u64
        }
    }
}

#[inline]
pub(super) fn encode_immediate(kind: TypedArrayKind, value: Value) -> Option<u64> {
    use TypedArrayKind::*;
    match (kind, value.decode()) {
        (BigInt64 | BigUint64, ValueKind::ShortBigInt(n)) => Some(n as u64),
        (BigInt64 | BigUint64, _) => None,
        (Int8 | Uint8 | Int16 | Uint16 | Int32 | Uint32, ValueKind::Int32(n)) => Some(n as u64),
        (_, ValueKind::Int32(n)) => Some(number_bits(kind, f64::from(n))),
        (_, ValueKind::Float64(n)) => Some(number_bits(kind, n)),
        _ => None,
    }
}

#[inline]
pub(super) fn decode_immediate(kind: TypedArrayKind, bits: u64) -> Option<Value> {
    use TypedArrayKind::*;
    Some(match kind {
        Int8 => Value::int32(bits as i8 as i32),
        Uint8 | Uint8Clamped => Value::int32(bits as u8 as i32),
        Int16 => Value::int32(bits as i16 as i32),
        Uint16 => Value::int32(bits as u16 as i32),
        Int32 => Value::int32(bits as i32),
        Uint32 => Value::number(f64::from(bits as u32)),
        BigInt64 => return Value::short_bigint(bits as i64),
        BigUint64 => return i64::try_from(bits).ok().and_then(Value::short_bigint),
        Float16 => Value::number(from_half(bits as u16)),
        Float32 => Value::number(f64::from(f32::from_bits(bits as u32))),
        Float64 => Value::number(f64::from_bits(bits)),
    })
}

pub(super) fn decode(
    ctx: &mut ExecCtx<'_, '_>,
    kind: TypedArrayKind,
    bits: u64,
) -> JsResult<Value> {
    if let Some(value) = decode_immediate(kind, bits) {
        return Ok(value);
    }
    match kind {
        TypedArrayKind::BigInt64 => {
            make_bigint(ctx, (bits as i64) < 0, (bits as i64).unsigned_abs())
        }
        TypedArrayKind::BigUint64 => make_bigint(ctx, false, bits),
        _ => unreachable!(),
    }
}

pub(super) fn to_bigint<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Value> {
    let primitive = convert::to_primitive(ctx, value, convert::PreferredType::Number)?;
    let _primitive_root = ctx.root(primitive)?;
    match primitive.decode() {
        ValueKind::BigInt(_) | ValueKind::ShortBigInt(_) => Ok(primitive),
        ValueKind::Bool(v) => Ok(Value::short_bigint(i64::from(v)).unwrap()),
        ValueKind::String(id) => parse_bigint(ctx, id),
        _ => Err(ctx.throw_type_error("Expected a BigInt")),
    }
}

pub(super) fn bigint_bits(heap: &Heap, value: Value) -> u64 {
    match value.decode() {
        ValueKind::ShortBigInt(n) => n as u64,
        ValueKind::BigInt(id) => {
            let n = heap.bigints.get(id).unwrap();
            let low = u64::from(n.limbs.first().copied().unwrap_or(0));
            let high = u64::from(n.limbs.get(1).copied().unwrap_or(0));
            let bits = low | (high << 32);
            if n.negative {
                bits.wrapping_neg()
            } else {
                bits
            }
        }
        _ => unreachable!(),
    }
}

fn make_bigint(ctx: &mut ExecCtx<'_, '_>, negative: bool, magnitude: u64) -> JsResult<Value> {
    if magnitude <= (1 << 46) {
        let n = if negative {
            -(magnitude as i64)
        } else {
            magnitude as i64
        };
        if let Some(value) = Value::short_bigint(n) {
            return Ok(value);
        }
    }
    let id = ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let words = [magnitude as u32, (magnitude >> 32) as u32];
        let limbs =
            TrackedVec::from_slice(&heap.ledger, &words[..if words[1] == 0 { 1 } else { 2 }])?;
        heap.bigints
            .insert(&heap.ledger, BigIntData { negative, limbs })
    })?;
    Ok(Value::bigint(id))
}

fn parse_bigint(ctx: &mut ExecCtx<'_, '_>, id: crate::value::StrId) -> JsResult<Value> {
    let units = ctx.heap().string_units(id);
    let mut chars = units.skip_while(|&c| convert::space(c)).peekable();
    let first = chars.next();
    let negative = first == Some(b'-' as u16);
    let signed = negative || first == Some(b'+' as u16);
    let radix = if first == Some(b'0' as u16) {
        match chars
            .peek()
            .copied()
            .and_then(|c| u8::try_from(c).ok())
            .map(|c| c.to_ascii_lowercase())
        {
            Some(b'x') => 16,
            Some(b'o') => 8,
            Some(b'b') => 2,
            _ => 10,
        }
    } else {
        10
    };
    if radix != 10 {
        chars.next();
    }
    let initial = first.filter(|_| !signed && radix == 10);
    let digits = initial.into_iter().chain(chars);
    let mut limbs = TrackedVec::<u32>::new();
    let mut seen = false;
    let mut trailing = false;
    for c in digits {
        if convert::space(c) {
            trailing = true;
            continue;
        }
        let digit = char::from_u32(u32::from(c))
            .and_then(|c| c.to_digit(radix))
            .filter(|_| !trailing);
        let Some(digit) = digit else {
            return Err(ctx.throw_error(crate::object::ErrorKind::Syntax, "Invalid BigInt"));
        };
        seen = true;
        let mut carry = u64::from(digit);
        for word in &mut *limbs {
            carry += u64::from(*word) * u64::from(radix);
            *word = carry as u32;
            carry >>= 32;
        }
        if carry != 0 && limbs.push(&ctx.heap().ledger, carry as u32).is_err() {
            return Err(ctx.throw_oom());
        }
    }
    if !seen && first.is_some() {
        return Err(ctx.throw_error(crate::object::ErrorKind::Syntax, "Invalid BigInt"));
    }
    if limbs.len() <= 2 {
        let low = u64::from(limbs.first().copied().unwrap_or(0));
        let high = u64::from(limbs.get(1).copied().unwrap_or(0));
        return make_bigint(ctx, negative, low | (high << 32));
    }
    ctx.allocate(|runtime| runtime.heap.bigints.try_reserve_slot(&runtime.heap.ledger))?;
    Ok(Value::bigint(
        ctx.runtime
            .heap
            .bigints
            .insert_reserved(BigIntData { negative, limbs }),
    ))
}

pub(super) fn to_half(n: f64) -> u16 {
    let bits = n.to_bits();
    let sign = ((bits >> 48) as u16) & 0x8000;
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let frac = bits & ((1 << 52) - 1);
    if exp == 1024 && frac != 0 {
        return 0x7e00;
    }
    if exp > 15 {
        return sign | 0x7c00;
    }
    if exp < -25 {
        return sign;
    }
    let significand = (1 << 52) | frac;
    let shift = if exp < -14 { 28 - exp } else { 42 };
    let integer = significand >> shift;
    let remainder = significand & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    let rounded = integer + u64::from(remainder > half || (remainder == half && integer & 1 != 0));
    let exponent = (exp + 14).max(0) as u16;
    sign | ((exponent << 10) + rounded as u16)
}

pub(super) fn from_half(bits: u16) -> f64 {
    let sign = u64::from(bits & 0x8000) << 48;
    let exp = (bits >> 10) & 31;
    let frac = u64::from(bits & 1023);
    match exp {
        31 => f64::from_bits(sign | (0x7ff << 52) | (frac << 42)),
        0 if frac == 0 => f64::from_bits(sign),
        0 => {
            let highest = 63 - frac.leading_zeros();
            let fraction = (frac ^ (1 << highest)) << (52 - highest);
            f64::from_bits(sign | (u64::from(999 + highest) << 52) | fraction)
        }
        _ => f64::from_bits(sign | (u64::from(1008 + exp) << 52) | (frac << 42)),
    }
}
