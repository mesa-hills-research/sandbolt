use qjs_bigint::{BigInt, BigIntError, BigIntHost};

use crate::heap::BigIntData;
use crate::object::ErrorKind;
use crate::value::{Abrupt, BigIntId};

use super::*;

pub(super) const START: u16 = 0x80;
pub(super) const END: u16 = START + 5;

#[repr(u16)]
enum MethodId {
    Constructor = START,
    AsIntN,
    AsUintN,
    ToString,
    ToLocaleString,
    ValueOf,
}

impl MethodId {
    const fn id(self) -> NativeFnId {
        NativeFnId(NUM_DATE_BASE + self as u16)
    }
}

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    use MethodId::*;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let id = ctx.realm().bigint_ctor;
    let ctor = constructor(ctx, global, id, Constructor.id(), "BigInt", 1)?;
    install_methods(
        ctx,
        ctor,
        &[("asIntN", 2, AsIntN.id()), ("asUintN", 2, AsUintN.id())],
    )?;
    let proto = ctx.root(Value::object(ctx.realm().bigint_proto))?;
    set_tag(ctx, proto, "BigInt")?;
    install_methods(
        ctx,
        proto,
        &[
            ("toString", 0, ToString.id()),
            ("toLocaleString", 0, ToLocaleString.id()),
            ("valueOf", 0, ValueOf.id()),
        ],
    )
}

pub(super) fn dispatch<'s>(
    offset: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    use MethodId::*;
    let id = NativeFnId(NUM_DATE_BASE + offset);
    if id == Constructor.id() {
        if ctx.new_target().is_some() {
            return Err(ctx.throw_type_error("BigInt is not a constructor"));
        }
        let arg = argument(ctx, args, 0)?;
        return convert(ctx, arg, true);
    }
    if id == AsIntN.id() || id == AsUintN.id() {
        let bits = argument(ctx, args, 0)?;
        let bits = to_integer_or_infinity(ctx, bits)?;
        if !(0.0..=MAX_SAFE).contains(&bits) {
            return Err(ctx.throw_range_error("invalid BigInt bit count"));
        }
        let arg = argument(ctx, args, 1)?;
        let value = convert(ctx, arg, false)?;
        let value = ctx.root(value)?;
        let number = from_value(ctx, value)?;
        return with_host(ctx, |host| {
            let result = if id == AsIntN.id() {
                number.0.as_int_n(bits as u64, host)
            } else {
                number.0.as_uint_n(bits as u64, host)
            }?;
            store(host.ctx, &result).map_err(HostError::Js)
        });
    }
    let value = receiver_value(ctx, this, Wrapper::BigInt)?;
    if id == ValueOf.id() {
        return Ok(value);
    }
    let value = ctx.root(value)?;
    let radix = if id == ToString.id() {
        let arg = argument(ctx, args, 0)?;
        radix(ctx, arg)?
    } else {
        10
    };
    let number = from_value(ctx, value)?;
    with_host(ctx, |host| {
        let text = number.0.to_string_radix(radix, host)?;
        let string = host.ctx.new_string(&text).map_err(HostError::Js)?;
        Ok(host.ctx.get(string))
    })
}

fn convert<'s>(ctx: &mut ExecCtx<'_, 's>, arg: Handle<'s>, allow_number: bool) -> JsResult<Value> {
    let value = to_primitive(ctx, arg, PreferredType::Number)?;
    match value.decode() {
        ValueKind::ShortBigInt(_) | ValueKind::BigInt(_) => Ok(value),
        ValueKind::Bool(value) => Ok(Value::short_bigint(i64::from(value)).unwrap()),
        ValueKind::String(id) => {
            ctx.root(value)?;
            let bytes = ascii_text(ctx, id, false)?;
            with_host(ctx, |host| {
                let number = BigInt::from_str_js(text_ref(&bytes), host)?;
                store(host.ctx, &number).map_err(HostError::Js)
            })
        }
        v if allow_number && matches!(v, ValueKind::Int32(_) | ValueKind::Float64(_)) => {
            with_host(ctx, |host| {
                let number = BigInt::from_f64_exact(value.as_number().unwrap(), host)?;
                store(host.ctx, &number).map_err(HostError::Js)
            })
        }
        _ => Err(ctx.throw_type_error("cannot convert value to BigInt")),
    }
}

// The heap stores a magnitude in 32-bit words. The arithmetic crate uses
// two's complement in 64-bit words. A hexadecimal bridge keeps that layout
// private and takes linear time in both directions.
fn hex(ctx: &mut ExecCtx<'_, '_>, id: BigIntId) -> JsResult<TrackedVec<u8>> {
    ctx.allocate(|rt| {
        let heap = &rt.heap;
        let data = heap.bigints.get(id).unwrap();
        let mut bytes = TrackedVec::new();
        bytes.try_reserve_exact(&heap.ledger, 1 + 8 * data.limbs.len().max(1))?;
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
        Ok(bytes)
    })
}

pub(super) fn to_number(ctx: &mut ExecCtx<'_, '_>, id: BigIntId) -> JsResult<f64> {
    let bytes = hex(ctx, id)?;
    Ok(atod(
        text_ref(&bytes),
        16,
        AtodFlags {
            int_only: true,
            ..AtodFlags::default()
        },
    )
    .0)
}

// The result from qjs-bigint retains its ledger charge until it is dropped.
struct TrackedBigInt(BigInt, HeapLedger);

impl Drop for TrackedBigInt {
    fn drop(&mut self) {
        self.1.release_bytes(self.0.byte_size());
    }
}

fn from_value<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<TrackedBigInt> {
    match ctx.get(value).decode() {
        ValueKind::ShortBigInt(n) => with_host(ctx, |host| {
            let number = BigInt::from_f64_exact(n as f64, host)?;
            Ok(host.retain(number))
        }),
        ValueKind::BigInt(id) => {
            let bytes = hex(ctx, id)?;
            with_host(ctx, |host| {
                let number = BigInt::parse_radix(text_ref(&bytes), 16, host)?;
                Ok(host.retain(number))
            })
        }
        _ => unreachable!(),
    }
}

fn store(ctx: &mut ExecCtx<'_, '_>, number: &BigInt) -> JsResult<Value> {
    if let Some(n) = number.to_i64_exact().and_then(Value::short_bigint) {
        return Ok(n);
    }
    let id = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let negative = number.is_negative();
        let mut limbs = TrackedVec::new();
        limbs.try_reserve_exact(&heap.ledger, number.limbs().len() * 2)?;
        let mut carry = negative;
        for &word in number.limbs() {
            let word = if negative {
                let (word, next_carry) = (!word).overflowing_add(u64::from(carry));
                carry = next_carry;
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
        heap.bigints
            .insert(&heap.ledger, BigIntData { negative, limbs })
    })?;
    let value = Value::bigint(id);
    ctx.root(value)?;
    Ok(value)
}

struct Host<'a, 'rt, 's> {
    ctx: &'a mut ExecCtx<'rt, 's>,
    charged: usize,
}

impl Host<'_, '_, '_> {
    fn retain(&mut self, number: BigInt) -> TrackedBigInt {
        self.charged -= number.byte_size();
        TrackedBigInt(number, self.ctx.heap().ledger.clone())
    }
}

impl BigIntHost for Host<'_, '_, '_> {
    fn charge(&mut self, bytes: usize) -> Result<(), BigIntError> {
        self.ctx
            .heap()
            .ledger
            .charge_bytes(bytes)
            .map_err(|_| BigIntError::OutOfMemory)?;
        self.charged += bytes;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.ctx.heap().ledger.release_bytes(bytes);
        self.charged -= bytes;
    }

    fn poll_interrupt(&mut self) -> bool {
        self.ctx.check_interrupt().is_err()
    }
}

impl Drop for Host<'_, '_, '_> {
    fn drop(&mut self) {
        self.release(self.charged);
    }
}

enum HostError {
    BigInt(BigIntError),
    Js(Abrupt),
}

impl From<BigIntError> for HostError {
    fn from(error: BigIntError) -> Self {
        Self::BigInt(error)
    }
}

fn with_host<T>(
    ctx: &mut ExecCtx<'_, '_>,
    f: impl FnOnce(&mut Host<'_, '_, '_>) -> Result<T, HostError>,
) -> JsResult<T> {
    ctx.runtime.allocation_point();
    let result = f(&mut Host { ctx, charged: 0 });
    result.map_err(|err| match err {
        HostError::Js(error) => error,
        HostError::BigInt(error) => match error {
            BigIntError::Interrupted => Abrupt::Interrupted,
            BigIntError::OutOfMemory => ctx.throw_oom(),
            BigIntError::InvalidString => {
                ctx.throw_error(ErrorKind::Syntax, "invalid BigInt string")
            }
            BigIntError::RangeError(message) => ctx.throw_range_error(message),
            _ => unreachable!(),
        },
    })
}
