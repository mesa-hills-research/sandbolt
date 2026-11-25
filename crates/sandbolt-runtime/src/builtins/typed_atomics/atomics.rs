use std::time::Duration;

use super::*;
use crate::object::WaitResult;

const ADD: u16 = 0;
const AND: u16 = 1;
const COMPARE: u16 = 2;
const EXCHANGE: u16 = 3;
const LOCK_FREE: u16 = 4;
const LOAD: u16 = 5;
const NOTIFY: u16 = 6;
const OR: u16 = 7;
const PAUSE: u16 = 8;
const STORE: u16 = 9;
const SUB: u16 = 10;
const WAIT: u16 = 11;
const XOR: u16 = 12;

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let atomics = ctx.new_object()?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    put(ctx, global, "Atomics", ctx.get(atomics), method_flags())?;
    tag(ctx, atomics, "Atomics")?;
    methods(
        ctx,
        atomics,
        ATOMIC_START,
        &[
            ("add", 3),
            ("and", 3),
            ("compareExchange", 4),
            ("exchange", 3),
            ("isLockFree", 1),
            ("load", 2),
            ("notify", 3),
            ("or", 3),
            ("pause", 0),
            ("store", 3),
            ("sub", 3),
            ("wait", 4),
            ("xor", 3),
        ],
    )
}

pub(super) fn dispatch<'s>(op: u16, ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let first = item(ctx, args, 0)?;
    if op == PAUSE {
        let n = ctx.get(first);
        if n != Value::UNDEFINED
            && !n
                .as_number()
                .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        {
            return Err(ctx.throw_type_error("Expected an integral number"));
        }
        std::hint::spin_loop();
        return Ok(Value::UNDEFINED);
    }
    if op == LOCK_FREE {
        return Ok(Value::bool(matches!(
            convert::to_number(ctx, first)?,
            1.0 | 2.0 | 4.0 | 8.0
        )));
    }
    let obj = ctx.object_id(first)?;
    let (view, initial_len) = typed::validate(ctx, obj)?;
    if !matches!(op, LOAD | WAIT | NOTIFY) {
        buffer::require_mutable(ctx, view.buffer)?;
    }
    let waitable = matches!(op, WAIT | NOTIFY);
    use TypedArrayKind::*;
    if (waitable && !matches!(view.kind, Int32 | BigInt64))
        || matches!(view.kind, Uint8Clamped | Float16 | Float32 | Float64)
    {
        return Err(ctx.throw_type_error("Expected an integer TypedArray"));
    }
    let shared = buffer::shared(ctx.heap(), view.buffer).cloned();
    if op == WAIT && shared.is_none() {
        return Err(ctx.throw_type_error("Atomics.wait requires a SharedArrayBuffer"));
    }
    let index = item(ctx, args, 1)?;
    let index = index_arg(ctx, index)?;
    let current = typed::length(ctx.heap(), view).unwrap_or(0);
    if index >= initial_len || index >= current {
        return Err(ctx.throw_range_error("Atomic index is out of bounds"));
    }
    let size = view.kind.size();
    let offset = view.byte_offset + size * index;
    if op == NOTIFY {
        let count = item(ctx, args, 2)?;
        let count = if ctx.get(count) == Value::UNDEFINED {
            usize::MAX
        } else {
            convert::to_integer_or_infinity(ctx, count)? as usize
        };
        check_bounds(ctx, view, index)?;
        return Ok(number(
            shared.map_or(0, |backing| backing.notify(offset, count)),
        ));
    }
    let mut stored = Value::UNDEFINED;
    let bits = if op == LOAD {
        0
    } else {
        let value = item(ctx, args, 2)?;
        if op == STORE {
            if view.kind.is_bigint() {
                stored = elements::to_bigint(ctx, value)?;
                elements::bigint_bits(ctx.heap(), stored)
            } else {
                let n = convert::to_integer_or_infinity(ctx, value)?;
                stored = Value::number(n);
                elements::number_bits(view.kind, n)
            }
        } else {
            elements::encode(ctx, view.kind, value)?
        }
    };
    if op == WAIT {
        let timeout = item(ctx, args, 3)?;
        let timeout = convert::to_number(ctx, timeout)?;
        let timeout = if timeout.is_nan() {
            None
        } else {
            Duration::try_from_secs_f64(timeout.max(0.0) / 1000.0).ok()
        };
        if !ctx.can_block() {
            return Err(ctx.throw_type_error("This agent cannot block"));
        }
        check_bounds(ctx, view, index)?;
        let ledger = ctx.heap().ledger.clone();
        let status = shared
            .unwrap()
            .wait(&ledger, offset, size, bits, timeout, || {
                ctx.check_interrupt().is_err()
            })
            .map_err(|_| ctx.throw_oom())?;
        let result = match status {
            WaitResult::Ok => "ok",
            WaitResult::NotEqual => "not-equal",
            WaitResult::TimedOut => "timed-out",
            WaitResult::Interrupted => return Err(crate::Abrupt::Interrupted),
        };
        return ctx.new_string(result).map(|s| ctx.get(s));
    }
    let replacement = if op == COMPARE {
        let replacement = item(ctx, args, 3)?;
        elements::encode(ctx, view.kind, replacement)?
    } else {
        0
    };
    check_bounds(ctx, view, index)?;
    let update = |old: u64| match op {
        ADD => Some(old.wrapping_add(bits)),
        AND => Some(old & bits),
        COMPARE if old == bits => Some(replacement),
        COMPARE | LOAD => None,
        EXCHANGE | STORE => Some(bits),
        OR => Some(old | bits),
        SUB => Some(old.wrapping_sub(bits)),
        XOR => Some(old ^ bits),
        _ => unreachable!(),
    };
    let old = if let Some(backing) = shared {
        backing.update_word(offset, size, update)
    } else {
        let old = typed::raw(ctx.heap(), view, index);
        if let Some(bits) = update(old) {
            typed::write_raw(ctx, view, index, bits);
        }
        old
    };
    if op == STORE {
        Ok(stored)
    } else {
        elements::decode(ctx, view.kind, old)
    }
}

fn check_bounds(ctx: &mut ExecCtx<'_, '_>, view: TypedArrayData, index: usize) -> JsResult<()> {
    if index >= typed::length(ctx.heap(), view).unwrap_or(0) {
        return Err(ctx.throw_range_error("Atomic index is out of bounds"));
    }
    Ok(())
}
