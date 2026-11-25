use sandbolt_bigint::{BigInt, BigIntError, BigIntHost};

use crate::object::ErrorKind;
use crate::value::{Abrupt, BigIntId};
use crate::vm::exec::numeric;

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
        ctx.root(value)?;
        return numeric::bigint_as_n(ctx, value, id == AsIntN.id(), bits as u64);
    }
    let value = receiver_value(ctx, this, Wrapper::BigInt)?;
    if id == ValueOf.id() {
        return Ok(value);
    }
    let radix = if id == ToString.id() {
        radix(ctx, args.get(0))?
    } else {
        10
    };
    numeric::bigint_radix_text(ctx, value, radix)
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

pub(super) fn to_number(ctx: &mut ExecCtx<'_, '_>, id: BigIntId) -> JsResult<f64> {
    numeric::bigint_to_number(ctx, Value::bigint(id))
}

fn store(ctx: &mut ExecCtx<'_, '_>, number: &BigInt) -> JsResult<Value> {
    if let Some(value) = number.to_i64_exact().and_then(Value::short_bigint) {
        return Ok(value);
    }
    let value = ctx.allocate(|rt| numeric::store_bigint(&mut rt.heap, number))?;
    ctx.root(value)?;
    Ok(value)
}

struct Host<'a, 'rt, 's> {
    ctx: &'a mut ExecCtx<'rt, 's>,
    charged: usize,
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
