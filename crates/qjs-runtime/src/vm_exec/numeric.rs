use qjs_bigint::{BigInt, BigIntError, BigIntHost};
use qjs_bytecode::Opcode;

use crate::heap::{BigIntData, Heap};
use crate::ledger::{HeapLedger, TrackedVec};
use crate::object::ErrorKind;
use crate::ops::convert::{self, PreferredType};
use crate::root::{ExecCtx, Handle};
use crate::value::*;

struct Host<'a> {
    ledger: &'a HeapLedger,
    handler: &'a mut Option<Box<dyn FnMut() -> bool>>,
    charged: usize,
}

impl BigIntHost for Host<'_> {
    fn charge(&mut self, bytes: usize) -> Result<(), BigIntError> {
        self.ledger
            .charge_bytes(bytes)
            .map_err(|_| BigIntError::OutOfMemory)?;
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
        let result = {
            let ledger = ctx.heap().ledger.clone();
            let mut host = Host {
                ledger: &ledger,
                handler: &mut ctx.runtime.vm.interrupt_handler,
                charged: 0,
            };
            run(&mut ctx.runtime.heap, &mut host)
        };
        match result {
            Ok(value) => return Ok(value),
            Err(BigIntError::OutOfMemory) if attempt == 0 => ctx.collect(),
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
        ValueKind::ShortBigInt(n) => {
            host.charge(size_of::<u64>())?;
            return Ok(BigInt::from_i64(n));
        }
        ValueKind::BigInt(id) => heap.bigints.get(id).unwrap(),
        _ => unreachable!(),
    };
    let mut bytes = TrackedVec::new();
    bytes
        .try_reserve_exact(&heap.ledger, data.limbs.len() * 8 + 1)
        .map_err(|_| BigIntError::OutOfMemory)?;
    if data.negative {
        bytes.push_reserved(b'-');
    }
    if data.limbs.is_empty() {
        bytes.push_reserved(b'0');
    }
    for &word in data.limbs.iter().rev() {
        for shift in (0..32).step_by(4).rev() {
            bytes.push_reserved(b"0123456789abcdef"[((word >> shift) & 15) as usize]);
        }
    }
    BigInt::parse_radix(std::str::from_utf8(&bytes).unwrap(), 16, host)
}

fn store(heap: &mut Heap, bigint: &BigInt) -> Result<Value, BigIntError> {
    if let Some(value) = bigint.to_i64_exact().and_then(Value::short_bigint) {
        return Ok(value);
    }
    let negative = bigint.is_negative();
    let mut carry = negative;
    let mut limbs = TrackedVec::new();
    limbs
        .try_reserve_exact(&heap.ledger, bigint.limbs().len() * 2)
        .map_err(|_| BigIntError::OutOfMemory)?;
    for &word in bigint.limbs() {
        let word = if negative {
            let (word, next) = (!word).overflowing_add(u64::from(carry));
            carry = next;
            word
        } else {
            word
        };
        limbs.push_reserved(word as u32);
        limbs.push_reserved((word >> 32) as u32);
    }
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    let id = heap
        .bigints
        .insert(&heap.ledger, BigIntData { negative, limbs })
        .map_err(|_| BigIntError::OutOfMemory)?;
    Ok(Value::bigint(id))
}

pub(super) fn parse_bigint(ctx: &mut ExecCtx<'_, '_>, text: &str) -> JsResult<Value> {
    with_host(ctx, |heap, host| {
        let value = BigInt::from_str_js(text, host)?;
        store(heap, &value)
    })
}

pub(crate) fn bigint_text(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<Value> {
    with_host(ctx, |heap, host| {
        let value = read(heap, host, value)?;
        let text = value.to_string_radix(10, host)?;
        let string = crate::string::JsString::from_str(&heap.ledger, &text)
            .map_err(|_| BigIntError::OutOfMemory)?;
        let id = heap
            .strings
            .insert(&heap.ledger, string)
            .map_err(|_| BigIntError::OutOfMemory)?;
        Ok(Value::string(id))
    })
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
            if op == Shr {
                return Err(ctx.throw_type_error("BigInt has no unsigned right shift"));
            }
            let (a, b) = (ctx.get(a), ctx.get(b));
            return with_host(ctx, |heap, host| {
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
                store(heap, &result)
            });
        }
        _ => return Err(ctx.throw_type_error("cannot mix BigInt and Number")),
    };
    if matches!(op, Shl | Sar | Shr | And | Or | Xor) {
        let a = convert::number_to_uint32(a);
        let b = convert::number_to_uint32(b);
        return Ok(match op {
            Shl => Value::int32((a << (b & 31)) as i32),
            Sar => Value::int32((a as i32) >> (b & 31)),
            Shr => Value::number(f64::from(a >> (b & 31))),
            And => Value::int32((a & b) as i32),
            Or => Value::int32((a | b) as i32),
            Xor => Value::int32((a ^ b) as i32),
            _ => unreachable!(),
        });
    }
    Ok(Value::number(match op {
        Add => a + b,
        Sub => a - b,
        Mul => a * b,
        Div => a / b,
        Mod => a % b,
        Pow if b.is_nan() || (a.abs() == 1.0 && b.is_infinite()) => f64::NAN,
        Pow => a.powf(b),
        _ => unreachable!(),
    }))
}

pub(super) fn unary<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    op: Opcode,
    value: Handle<'s>,
) -> JsResult<(Value, Value)> {
    use Opcode::*;
    let raw = convert::to_primitive(ctx, value, PreferredType::Number)?;
    let value = ctx.root(raw)?;
    let (before, after) = if bigint(raw) {
        if op == Plus {
            return Err(ctx.throw_type_error("cannot convert BigInt to number"));
        }
        let result = with_host(ctx, |heap, host| {
            let integer = read(heap, host, raw)?;
            let result = match op {
                Neg => integer.neg(host)?,
                Not => integer.bitnot(host)?,
                Inc | PostInc | Dec | PostDec => {
                    host.charge(size_of::<u64>())?;
                    integer.add(
                        &BigInt::from_i64(if matches!(op, Inc | PostInc) { 1 } else { -1 }),
                        host,
                    )?
                }
                _ => unreachable!(),
            };
            store(heap, &result)
        })?;
        (raw, result)
    } else {
        let number = convert::to_number(ctx, value)?;
        let result = match op {
            Neg => -number,
            Plus => number,
            Inc | PostInc => number + 1.0,
            Dec | PostDec => number - 1.0,
            Not => f64::from(!convert::to_int32(ctx, value)?),
            _ => unreachable!(),
        };
        (Value::number(number), Value::number(result))
    };
    Ok((before, after))
}
