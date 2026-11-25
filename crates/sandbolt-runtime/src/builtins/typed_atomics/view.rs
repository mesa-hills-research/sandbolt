use super::*;
use crate::ledger::TrackedBox;
use crate::object::DataViewData;

pub(super) fn install<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<()> {
    let ctor = configure_ctor(ctx, ctx.realm().data_view_ctor, id(DV), "DataView", 1)?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    put(ctx, global, "DataView", ctx.get(ctor), method_flags())?;
    let proto = ctx.root(Value::object(ctx.realm().data_view_proto))?;
    tag(ctx, proto, "DataView")?;
    getters(
        ctx,
        proto,
        VIEW_START,
        &["buffer", "byteLength", "byteOffset"],
    )?;
    for op in 0..2 {
        for (index, kind) in kinds().enumerate() {
            let name = format!(
                "{}{}",
                if op == 0 { "get" } else { "set" },
                kind.name().trim_end_matches("Array")
            );
            let key = ctx.intern_str(&name)?;
            install_method(
                ctx,
                proto,
                key,
                id(VIEW_START + 3 + op * 11 + index as u16),
                &name,
                u32::from(op + 1),
            )?;
        }
    }
    Ok(())
}

fn kinds() -> impl Iterator<Item = TypedArrayKind> {
    TypedArrayKind::ALL
        .into_iter()
        .filter(|&k| k != TypedArrayKind::Uint8Clamped)
}

pub(super) fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let buffer = item(ctx, args, 0)?;
    let buffer = ctx.object_id(buffer)?;
    if buffer::info(ctx.heap(), buffer).is_none() {
        return Err(ctx.throw_type_error("Expected an ArrayBuffer"));
    }
    let offset = item(ctx, args, 1)?;
    let byte_offset = index_arg(ctx, offset)?;
    let state = buffer::info(ctx.heap(), buffer).unwrap();
    if state.detached {
        return Err(buffer::detached(ctx));
    }
    if byte_offset > state.len {
        return Err(ctx.throw_range_error("DataView offset exceeds buffer length"));
    }
    let length_arg = item(ctx, args, 2)?;
    let byte_length = if ctx.get(length_arg) == Value::UNDEFINED {
        state.max.is_none().then_some(state.len - byte_offset)
    } else {
        let len = index_arg(ctx, length_arg)?;
        if len > state.len - byte_offset {
            return Err(ctx.throw_range_error("DataView exceeds buffer length"));
        }
        Some(len)
    };
    let proto = proto_from_target(ctx, target, |realm| realm.data_view_proto)?;
    let proto = ctx.object_id(proto)?;
    let state = buffer::info(ctx.heap(), buffer).unwrap();
    if state.detached {
        return Err(buffer::detached(ctx));
    }
    if byte_offset > state.len || byte_length.is_some_and(|n| n > state.len - byte_offset) {
        return Err(ctx.throw_range_error("DataView exceeds buffer length"));
    }
    let data = DataViewData {
        buffer,
        byte_offset,
        byte_length,
    };
    let result = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::DataView(TrackedBox::new(ledger, data)?))
    })?;
    Ok(ctx.get(result))
}

fn length(heap: &Heap, view: DataViewData) -> Option<usize> {
    let state = buffer::info(heap, view.buffer)?;
    if state.detached {
        return None;
    }
    let rest = state.len.checked_sub(view.byte_offset)?;
    match view.byte_length {
        Some(n) if n > rest => None,
        Some(n) => Some(n),
        None => Some(rest),
    }
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let obj = ctx.object_id(this)?;
    let ObjectData::DataView(view) = &ctx.heap().object(obj).data else {
        return Err(ctx.throw_type_error("Expected a DataView"));
    };
    let view = **view;
    if op == 0 {
        return Ok(Value::object(view.buffer));
    }
    if op < 3 {
        let len = length(ctx.heap(), view)
            .ok_or_else(|| ctx.throw_type_error("DataView is out of bounds"))?;
        return Ok(number(if op == 1 { len } else { view.byte_offset }));
    }
    let op = op - 3;
    let set = op >= 11;
    if set {
        buffer::require_mutable(ctx, view.buffer)?;
    }
    let kind = kinds().nth(usize::from(op % 11)).unwrap();
    let index = item(ctx, args, 0)?;
    let index = index_arg(ctx, index)?;
    let bits = if set {
        let value = item(ctx, args, 1)?;
        Some(elements::encode(ctx, kind, value)?)
    } else {
        None
    };
    let endian = item(ctx, args, if set { 2 } else { 1 })?;
    let little = convert::to_boolean(ctx, endian);
    let len = length(ctx.heap(), view)
        .ok_or_else(|| ctx.throw_type_error("DataView is out of bounds"))?;
    let size = kind.size();
    if len
        .checked_sub(index)
        .is_none_or(|remaining| remaining < size)
    {
        return Err(ctx.throw_range_error("DataView index is out of bounds"));
    }
    let offset = view.byte_offset + index;
    if let Some(bits) = bits {
        if little {
            buffer::write(ctx, view.buffer, offset, &bits.to_le_bytes()[..size]);
        } else {
            buffer::write(ctx, view.buffer, offset, &bits.to_be_bytes()[8 - size..]);
        }
        Ok(Value::UNDEFINED)
    } else {
        let mut bytes = [0; 8];
        let bits = if little {
            buffer::read(ctx.heap(), view.buffer, offset, &mut bytes[..size]);
            u64::from_le_bytes(bytes)
        } else {
            buffer::read(ctx.heap(), view.buffer, offset, &mut bytes[8 - size..]);
            u64::from_be_bytes(bytes)
        };
        elements::decode(ctx, kind, bits)
    }
}
