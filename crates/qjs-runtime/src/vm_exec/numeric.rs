use qjs_bigint::{BigInt, BigIntError, BigIntHost};
use qjs_bytecode::Opcode;

use crate::heap::{BigIntData, Heap};
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
    let mut limbs = TrackedVec::new();
    limbs.try_reserve_exact(&heap.ledger, words.len())?;
    for word in words {
        limbs.push_reserved(word);
    }
    let id = heap
        .bigints
        .insert(&heap.ledger, BigIntData { negative, limbs })?;
    Ok(Value::bigint(id))
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
        let value = read(heap, host, value)?;
        Ok(Some(value.cmp_bigint(&other)))
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

fn small_bigint(heap: &Heap, value: Value) -> Option<i128> {
    match value.decode() {
        ValueKind::ShortBigInt(n) => Some(i128::from(n)),
        ValueKind::BigInt(id) => {
            let data = heap.bigints.get(id).unwrap();
            if data.limbs.len() > 4 {
                return None;
            }
            let magnitude = data
                .limbs
                .iter()
                .rev()
                .fold(0u128, |n, &word| (n << 32) | u128::from(word));
            if data.negative {
                (magnitude <= i128::MIN.unsigned_abs()).then(|| magnitude.wrapping_neg() as i128)
            } else {
                i128::try_from(magnitude).ok()
            }
        }
        _ => unreachable!(),
    }
}

fn small_bigint_binary(op: Opcode, a: i128, b: i128) -> Option<(bool, u128)> {
    use Opcode::*;
    let value = match op {
        Add => a.checked_add(b)?,
        Sub => a.checked_sub(b)?,
        Mul => {
            return Some((
                (a < 0) ^ (b < 0),
                a.unsigned_abs().checked_mul(b.unsigned_abs())?,
            ));
        }
        Div => a.checked_div(b)?,
        Mod => a.checked_rem(b)?,
        Pow => a.checked_pow(u32::try_from(b).ok()?)?,
        And => a & b,
        Or => a | b,
        Xor => a ^ b,
        Shl | Sar => {
            let shift = b.unsigned_abs();
            if (op == Sar) ^ (b < 0) {
                a >> shift.min(127)
            } else if a == 0 {
                0
            } else {
                let shift = u32::try_from(shift).ok()?;
                a.checked_shl(shift).filter(|result| result >> shift == a)?
            }
        }
        _ => return None,
    };
    Some((value < 0, value.unsigned_abs()))
}

fn store_small_bigint(
    ctx: &mut ExecCtx<'_, '_>,
    negative: bool,
    magnitude: u128,
) -> JsResult<Value> {
    if let Ok(magnitude) = i64::try_from(magnitude)
        && let Some(value) = Value::short_bigint(if negative { -magnitude } else { magnitude })
    {
        return Ok(value);
    }
    let bits = (u128::BITS - magnitude.leading_zeros()) as usize;
    let words = (0..bits.div_ceil(32)).map(|i| (magnitude >> (i * 32)) as u32);
    ctx.allocate(|rt| store_magnitude(&mut rt.heap, negative, words.clone()))
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
    if ctx.runtime.vm.interrupt_handler.is_none()
        && let (Some(a), Some(b)) = (small_bigint(ctx.heap(), a), small_bigint(ctx.heap(), b))
        && let Some((negative, magnitude)) = small_bigint_binary(op, a, b)
    {
        return store_small_bigint(ctx, negative, magnitude);
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
    use crate::EngineLimits;
    use crate::api::{EvalOptions, JsEngine};

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
