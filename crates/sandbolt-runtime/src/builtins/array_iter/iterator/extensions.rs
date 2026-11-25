use super::*;
use crate::ops::compare::same_value_zero;
use crate::ops::convert::to_string;

fn nonnegative_integer(
    ctx: &mut ExecCtx<'_, '_>,
    value: Value,
    min: f64,
    max: f64,
    infinity: bool,
) -> JsResult<f64> {
    let Some(number) = value
        .as_number()
        .filter(|n| !n.is_nan() && (n.fract() == 0.0 || (infinity && n.is_infinite())))
    else {
        return Err(ctx.throw_type_error("Expected an integral number"));
    };
    if number < min || (number.is_finite() && number > max) {
        return Err(ctx.throw_range_error("Number is out of range"));
    }
    Ok(number)
}

pub(super) fn includes<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    ctx.object_id(this)?;
    let needle = arg(ctx, args, 0)?;
    let skip = arg(ctx, args, 1)?;
    let skip = ctx.get(skip);
    let mut skip = if skip == Value::UNDEFINED {
        0.0
    } else {
        let status = nonnegative_integer(ctx, skip, 0.0, MAX_LENGTH as f64, true);
        on_error(ctx, status, |ctx| IterRecord::close(ctx, this))?
    };
    let record = IterRecord::direct(ctx, this)?;
    let needle = ctx.get(needle);
    loop {
        ctx.check_interrupt()?;
        let Some(item) = IterRecord::step(ctx, record)? else {
            return Ok(Value::bool(false));
        };
        if skip > 0.0 {
            skip -= 1.0;
        } else if ctx.with_scope(|ctx| {
            let item = ctx.root(item)?;
            let needle = ctx.root(needle)?;
            Ok(same_value_zero(ctx, item, needle))
        })? {
            IterRecord::close(ctx, this)?;
            return Ok(Value::bool(true));
        }
    }
}

pub(super) fn join<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    ctx.object_id(this)?;
    let separator = arg(ctx, args, 0)?;
    let separator = if ctx.get(separator) == Value::UNDEFINED {
        ctx.new_string(",")?
    } else {
        let status = to_string(ctx, separator);
        on_error(ctx, status, |ctx| IterRecord::close(ctx, this))?
    };
    let separator = ctx.get(separator);
    let record = IterRecord::direct(ctx, this)?;
    let mut buffer = TrackedVec::new();
    let mut first = true;
    loop {
        ctx.check_interrupt()?;
        let Some(item) = IterRecord::step(ctx, record)? else {
            break;
        };
        let status = ctx.with_scope(|ctx| {
            let item = ctx.root(item)?;
            if !first {
                append_string(ctx, &mut buffer, separator)?;
            }
            if !matches!(ctx.get(item), Value::NULL | Value::UNDEFINED) {
                let text = to_string(ctx, item)?;
                append_string(ctx, &mut buffer, ctx.get(text))?;
            }
            Ok(())
        });
        on_error(ctx, status, |ctx| IterRecord::close(ctx, this))?;
        first = false;
    }
    let result = ctx.new_utf16(&buffer)?;
    Ok(ctx.get(result))
}

pub(super) fn chunk_helper<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    windows: bool,
) -> JsResult<Value> {
    ctx.object_id(this)?;
    let op = (|| {
        let size = arg(ctx, args, 0)?;
        let size =
            nonnegative_integer(ctx, ctx.get(size), 1.0, f64::from(u32::MAX), false)? as usize;
        if !windows {
            return Ok(HelperOp::Chunks { size });
        }
        let option = arg(ctx, args, 1)?;
        let allow_partial = match ctx.get(option).decode() {
            ValueKind::Undefined => false,
            ValueKind::String(id)
                if ctx
                    .heap()
                    .string_units(id)
                    .eq("allow-partial".encode_utf16()) =>
            {
                true
            }
            ValueKind::String(id) if ctx.heap().string_units(id).eq("only-full".encode_utf16()) => {
                false
            }
            _ => return Err(ctx.throw_type_error("Invalid window option")),
        };
        Ok(HelperOp::Windows {
            size,
            allow_partial,
        })
    })();
    let op = on_error(ctx, op, |ctx| IterRecord::close(ctx, this))?;
    let record = IterRecord::direct(ctx, this)?.raw(ctx);
    let result = new_iterator(ctx, ctx.realm().iterator_helper_proto, || {
        let mut state = HelperData::new(op);
        state.source = Some(record);
        IteratorKind::Helper(state)
    })?;
    Ok(ctx.get(result))
}

pub(super) fn chunk_step(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    size: usize,
    windows: bool,
    allow_partial: bool,
) -> JsResult<Option<Value>> {
    let Some(source) = helper(ctx, id).source else {
        return Ok(None);
    };
    loop {
        ctx.check_interrupt()?;
        let found = ctx.with_scope(|ctx| {
            let source = source.rooted(ctx)?;
            let Some(value) = IterRecord::step(ctx, source)? else {
                return Ok(false);
            };
            let value = ctx.root(value)?;
            let value = ctx.get(value);
            let data = helper_mut(ctx, id);
            if data.buffer.len() == size {
                data.buffer[data.buffer_start] = value;
                data.buffer_start = (data.buffer_start + 1) % size;
            } else {
                ctx.allocate(|runtime| {
                    let heap = &mut runtime.heap;
                    let ObjectData::Iterator(data) = &mut heap.objects.get_mut(id).unwrap().data
                    else {
                        unreachable!()
                    };
                    let IteratorKind::Helper(data) = &mut data.kind else {
                        unreachable!()
                    };
                    data.buffer.push(&heap.ledger, value)
                })?;
            }
            Ok(true)
        })?;
        let count = helper(ctx, id).buffer.len();
        if !found {
            helper_mut(ctx, id).source = None;
            if count == 0 || (windows && (!allow_partial || count == size)) {
                return Ok(None);
            }
            break;
        }
        if count == size {
            break;
        }
    }
    let count = helper(ctx, id).buffer.len();
    let start = helper(ctx, id).buffer_start;
    let out = ctx.new_array()?;
    let output = ctx.get(out);
    for i in 0..count {
        let value = helper(ctx, id).buffer[(start + i) % count];
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let out = ctx.root(output)?;
            create_index(ctx, out, i as u64, value)
        })?;
    }
    if !windows {
        helper_mut(ctx, id).buffer.clear();
    }
    Ok(Some(output))
}
