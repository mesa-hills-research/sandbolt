use super::*;

pub(super) fn at_with<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    copy: bool,
) -> JsResult<Value> {
    let position = arg(ctx, args, 0)?;
    let position = to_integer_or_infinity(ctx, position)?;
    let position = if position < 0.0 {
        r.len as f64 + position
    } else {
        position
    };
    if !(0.0..r.len as f64).contains(&position) {
        return if copy {
            Err(ctx.throw_range_error("Index out of range"))
        } else {
            Ok(Value::UNDEFINED)
        };
    }
    if !copy {
        return get_index(ctx, r.object, position as u64);
    }
    let replacement = arg(ctx, args, 1)?;
    let replacement = ctx.get(replacement);
    let result = array_of_length(ctx, r.len)?;
    let out = ctx.get(result);
    for i in 0..r.len {
        r.at(ctx, |ctx, object| {
            let output = ctx.root(out)?;
            let value = if i == position as u64 {
                replacement
            } else {
                get_index(ctx, object, i)?
            };
            create_index(ctx, output, i, value)
        })?;
    }
    Ok(out)
}

pub(super) fn concat<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let result = species_create(ctx, object, 0)?;
    let output = ctx.get(result);
    let mut next = 0;
    let items = std::iter::once(object).chain((0..args.len()).map(|i| args.get(i).unwrap()));
    for item in items {
        ctx.check_interrupt()?;
        let value = ctx.get(item);
        let (spread, len) = ctx.with_scope(|ctx| {
            let item = ctx.root(value)?;
            let spread = if value.as_object().is_some() {
                let spread = get_property(ctx, item, symbol(ctx, WellKnown::IsConcatSpreadable))?;
                if spread != Value::UNDEFINED {
                    let spread = ctx.root(spread)?;
                    to_boolean(ctx, spread)
                } else {
                    is_array(ctx, value)?
                }
            } else {
                false
            };
            Ok((spread, if spread { get_length(ctx, item)? } else { 1 }))
        })?;
        if len > MAX_LENGTH - next {
            return Err(ctx.throw_type_error("Array exceeds maximum length"));
        }
        if spread {
            for i in 0..len {
                ctx.with_scope(|ctx| {
                    ctx.check_interrupt()?;
                    let item = ctx.root(value)?;
                    let output = ctx.root(output)?;
                    if let Some(value) = present(ctx, item, i)? {
                        create_index(ctx, output, next + i, value)?;
                    }
                    Ok(())
                })?;
            }
        } else {
            ctx.with_scope(|ctx| {
                let output = ctx.root(output)?;
                create_index(ctx, output, next, value)
            })?;
        }
        next += len;
    }
    set_len(ctx, result, next)?;
    Ok(output)
}

fn transfer<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    from: u64,
    to: u64,
    count: u64,
    backwards: bool,
) -> JsResult<()> {
    for i in 0..count {
        let i = if backwards { count - i - 1 } else { i };
        r.at(ctx, |ctx, object| {
            if let Some(value) = present(ctx, object, from + i)? {
                set_index(ctx, object, to + i, value)
            } else {
                delete_index(ctx, object, to + i)
            }
        })?;
    }
    Ok(())
}

pub(super) fn copy_within<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let to = bound(ctx, args.get(0), r.len, 0)?;
    let from = bound(ctx, args.get(1), r.len, 0)?;
    let end = bound(ctx, args.get(2), r.len, r.len)?;
    let count = end.saturating_sub(from).min(r.len - to);
    transfer(ctx, r, from, to, count, from < to && to < from + count)?;
    Ok(ctx.get(r.object))
}

pub(super) fn callback<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    op: u16,
) -> JsResult<Value> {
    let callback = arg(ctx, args, 0)?;
    let function = ctx.get(callback);
    if !callable(ctx, function) {
        return Err(ctx.throw_type_error("Callback is not callable"));
    }
    let output = match op {
        MAP => Some(species_create(ctx, r.object, r.len)?),
        FILTER => Some(species_create(ctx, r.object, 0)?),
        _ => None,
    };
    let output = output.map(|h| ctx.get(h));
    let this = arg(ctx, args, 1)?;
    let this = ctx.get(this);
    let reduce = matches!(op, REDUCE | REDUCE_RIGHT);
    let backwards = matches!(op, FIND_LAST | FIND_LAST_INDEX | REDUCE_RIGHT);
    let read_through = matches!(op, FIND | FIND_INDEX | FIND_LAST | FIND_LAST_INDEX);
    let mut reduce_started = args.len() > 1;
    let accumulator = ctx.mut_root(this)?;
    let mut next = 0;
    for i in 0..r.len {
        let i = if backwards { r.len - 1 - i } else { i };
        let old = ctx.get(accumulator.handle());
        let found = r.at(ctx, |ctx, object| {
            let value = if read_through {
                Some(get_index(ctx, object, i)?)
            } else {
                present(ctx, object, i)?
            };
            let Some(value) = value else {
                return Ok(None);
            };
            if reduce && !reduce_started {
                return Ok(Some(value));
            }
            let item = ctx.root(value)?;
            let function = ctx.root(function)?;
            let index = ctx.root(Value::number(i as f64))?;
            let this = ctx.root(if reduce { Value::UNDEFINED } else { this })?;
            let arguments = if reduce {
                let old = ctx.root(old)?;
                ctx.args(&[old, item, index, object])?
            } else {
                ctx.args(&[item, index, object])?
            };
            let mapped = call(ctx, function, this, arguments)?;
            let mapped = ctx.root(mapped)?;
            let matched = to_boolean(ctx, mapped);
            match op {
                MAP => {
                    let out = ctx.root(output.unwrap())?;
                    create_index(ctx, out, i, ctx.get(mapped))?;
                }
                FILTER if matched => {
                    let out = ctx.root(output.unwrap())?;
                    create_index(ctx, out, next, value)?;
                    next += 1;
                }
                EVERY if !matched => return Ok(Some(Value::bool(false))),
                SOME if matched => return Ok(Some(Value::bool(true))),
                FIND | FIND_LAST if matched => return Ok(Some(value)),
                FIND_INDEX | FIND_LAST_INDEX if matched => {
                    return Ok(Some(Value::number(i as f64)));
                }
                REDUCE | REDUCE_RIGHT => return Ok(Some(ctx.get(mapped))),
                _ => {}
            }
            Ok(None)
        })?;
        if let Some(value) = found {
            if reduce {
                ctx.set(&accumulator, value);
                reduce_started = true;
            } else {
                return Ok(value);
            }
        }
    }
    match op {
        REDUCE | REDUCE_RIGHT if !reduce_started => {
            Err(ctx.throw_type_error("Reduce of empty array with no initial value"))
        }
        REDUCE | REDUCE_RIGHT => Ok(ctx.get(accumulator.handle())),
        EVERY => Ok(Value::bool(true)),
        SOME => Ok(Value::bool(false)),
        FIND_INDEX | FIND_LAST_INDEX => Ok(Value::int32(-1)),
        MAP | FILTER => Ok(output.unwrap()),
        _ => Ok(Value::UNDEFINED),
    }
}

pub(super) fn fill<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let value = arg(ctx, args, 0)?;
    let value = ctx.get(value);
    let start = bound(ctx, args.get(1), r.len, 0)?;
    let end = bound(ctx, args.get(2), r.len, r.len)?;
    let object = ctx.get(r.object);
    if start < end && !fill_dense(ctx, r.object, start, end, value)? {
        for i in start..end {
            r.at(ctx, |ctx, object| set_index(ctx, object, i, value))?;
        }
    }
    Ok(object)
}

fn fill_dense<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    start: u64,
    end: u64,
    value: Value,
) -> JsResult<bool> {
    let id = ctx.object_id(object)?;
    let heap = ctx.heap();
    let object = heap.object(id);
    let ObjectData::Array(array) = &object.data else {
        return Ok(false);
    };
    if !object.flags.contains(ObjectFlags::FAST_ARRAY)
        || object.flags.contains(ObjectFlags::IS_PROTOTYPE)
        || end > u64::from(array.length)
        || end > u64::from(crate::atom::INLINE_INDEX_LIMIT)
        || start > (array.elements.len().saturating_add(64) as u64).max(1023)
    {
        return Ok(false);
    }
    let (start, end) = (start as usize, end as usize);
    // Holes and appends must obey indexed setters on the prototype chain.
    let mut plain_proto = true;
    let mut parent = heap.shape(object.shape).proto;
    while let Some(id) = parent {
        let proto = heap.object(id);
        match &proto.data {
            ObjectData::Array(data) if data.elements.is_empty() => {}
            ObjectData::Ordinary => {}
            _ => {
                plain_proto = false;
                break;
            }
        }
        let shape = heap.shape(proto.shape);
        if shape
            .props
            .iter()
            .any(|p| heap.array_index(p.key).is_some())
        {
            plain_proto = false;
            break;
        }
        parent = shape.proto;
    }
    if !plain_proto || !object.flags.contains(ObjectFlags::EXTENSIBLE) {
        return Ok(false);
    }
    ctx.check_interrupt()?;
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::Array(array) = &mut heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        let additional = end.saturating_sub(array.elements.len());
        array.elements.try_reserve_exact(&heap.ledger, additional)
    })?;
    for base in (start..end).step_by(1024) {
        ctx.check_interrupt()?;
        let ObjectData::Array(array) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
        else {
            unreachable!()
        };
        let stop = (base + 1024).min(end);
        while array.elements.len() < stop {
            array.elements.push_reserved(Value::EMPTY);
        }
        array.elements[base..stop].fill(value);
    }
    Ok(true)
}

pub(super) fn search<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    op: u16,
) -> JsResult<Value> {
    let missing = if op == INCLUDES {
        Value::bool(false)
    } else {
        Value::int32(-1)
    };
    if r.len == 0 {
        return Ok(missing);
    }
    let needle = arg(ctx, args, 0)?;
    let needle = ctx.get(needle);
    let offset = match args.get(1) {
        Some(from) => to_integer_or_infinity(ctx, from)?,
        None if op == LAST_INDEX_OF => r.len as f64 - 1.0,
        None => 0.0,
    };
    let backwards = op == LAST_INDEX_OF;
    let (start, count) = if backwards {
        let start = if offset < 0.0 {
            r.len as f64 + offset
        } else {
            offset.min((r.len - 1) as f64)
        };
        if start < 0.0 {
            return Ok(missing);
        }
        (start as u64, start as u64 + 1)
    } else {
        let start = relative(offset, r.len);
        (start, r.len - start)
    };
    for k in 0..count {
        let i = if backwards { start - k } else { start + k };
        if r.at(ctx, |ctx, object| {
            let value = if op == INCLUDES {
                Some(get_index(ctx, object, i)?)
            } else {
                present(ctx, object, i)?
            };
            let Some(value) = value else { return Ok(false) };
            let value = ctx.root(value)?;
            let needle = ctx.root(needle)?;
            Ok(if op == INCLUDES {
                same_value_zero(ctx, value, needle)
            } else {
                strict_eq(ctx, value, needle)
            })
        })? {
            return Ok(if op == INCLUDES {
                Value::bool(true)
            } else {
                Value::number(i as f64)
            });
        }
    }
    Ok(missing)
}

pub(super) fn join<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    locale: bool,
) -> JsResult<Value> {
    let separator = if locale
        || args
            .get(0)
            .is_none_or(|arg| ctx.get(arg) == Value::UNDEFINED)
    {
        ctx.new_string(",")?
    } else {
        to_string(ctx, args.get(0).unwrap())?
    };
    let sep = ctx.get(separator);
    let object = ctx.get(r.object);
    let frames = &ctx.runtime.vm.frames;
    if frames.iter().rev().skip(1).any(|frame| {
        ctx.runtime.vm.stack[frame.base as usize + 1] == object
            && matches!(&ctx.heap().object(frame.callee).data, ObjectData::NativeFunction(f) if f.function == id(JOIN) || f.function == id(TO_LOCALE_STRING))
    }) {
        let empty = ctx.new_string("")?;
        return Ok(ctx.get(empty));
    }
    let mut text = TrackedVec::new();
    for i in 0..r.len {
        r.at(ctx, |ctx, object| {
            if i != 0 {
                append_string(ctx, &mut text, sep)?;
            }
            let value = get_index(ctx, object, i)?;
            if matches!(value, Value::NULL | Value::UNDEFINED) {
                return Ok(());
            }
            let value = ctx.root(value)?;
            let item = if locale {
                let object = to_object(ctx, value)?;
                let k = key(ctx, "toLocaleString")?;
                let method =
                    crate::ops::property::get_property_with_receiver(ctx, object, k, value)?;
                let method = ctx.root(method)?;
                let result = call(ctx, method, value, Args::empty())?;
                ctx.root(result)?
            } else {
                value
            };
            let item = to_string(ctx, item)?;
            append_string(ctx, &mut text, ctx.get(item))
        })?;
    }
    let text = ctx.new_utf16(&text)?;
    Ok(ctx.get(text))
}

pub(super) fn remove_end<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    front: bool,
) -> JsResult<Value> {
    if r.len == 0 {
        set_len(ctx, r.object, 0)?;
        return Ok(Value::UNDEFINED);
    }
    let result = get_index(ctx, r.object, if front { 0 } else { r.len - 1 })?;
    let result = ctx.root(result)?;
    if front {
        transfer(ctx, r, 1, 0, r.len - 1, false)?;
    }
    delete_index(ctx, r.object, r.len - 1)?;
    set_len(ctx, r.object, r.len - 1)?;
    Ok(ctx.get(result))
}

pub(super) fn insert_end<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    front: bool,
) -> JsResult<Value> {
    let count = args.len() as u64;
    if count > MAX_LENGTH - r.len {
        return Err(ctx.throw_type_error("Array exceeds maximum length"));
    }
    if front && count > 0 {
        transfer(ctx, r, 0, count, r.len, true)?;
    }
    let base = if front { 0 } else { r.len };
    for i in 0..args.len() {
        let value = ctx.get(args.get(i).unwrap());
        r.at(ctx, |ctx, object| {
            set_index(ctx, object, base + i as u64, value)
        })?;
    }
    set_len(ctx, r.object, r.len + count)?;
    Ok(Value::number((r.len + count) as f64))
}

pub(super) fn reverse<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    copy: bool,
) -> JsResult<Value> {
    if copy {
        let output = array_of_length(ctx, r.len)?;
        let out = ctx.get(output);
        for i in 0..r.len {
            r.at(ctx, |ctx, object| {
                let output = ctx.root(out)?;
                let value = get_index(ctx, object, r.len - 1 - i)?;
                create_index(ctx, output, i, value)
            })?;
        }
        return Ok(out);
    }
    for lower in 0..r.len / 2 {
        r.at(ctx, |ctx, object| {
            let upper = r.len - lower - 1;
            let a = present(ctx, object, lower)?
                .map(|v| ctx.root(v))
                .transpose()?;
            let b = present(ctx, object, upper)?
                .map(|v| ctx.root(v))
                .transpose()?;
            match (a, b) {
                (Some(a), Some(b)) => {
                    set_index(ctx, object, lower, ctx.get(b))?;
                    set_index(ctx, object, upper, ctx.get(a))?;
                }
                (None, Some(b)) => {
                    set_index(ctx, object, lower, ctx.get(b))?;
                    delete_index(ctx, object, upper)?;
                }
                (Some(a), None) => {
                    delete_index(ctx, object, lower)?;
                    set_index(ctx, object, upper, ctx.get(a))?;
                }
                (None, None) => {}
            }
            Ok(())
        })?;
    }
    Ok(ctx.get(r.object))
}

fn copy_range<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    out: Value,
    from: u64,
    to: u64,
    count: u64,
    dense: bool,
) -> JsResult<()> {
    for i in 0..count {
        r.at(ctx, |ctx, object| {
            let output = ctx.root(out)?;
            let value = if dense {
                Some(get_index(ctx, object, from + i)?)
            } else {
                present(ctx, object, from + i)?
            };
            if let Some(value) = value {
                create_index(ctx, output, to + i, value)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub(super) fn slice<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let from = bound(ctx, args.get(0), r.len, 0)?;
    let end = bound(ctx, args.get(1), r.len, r.len)?;
    let count = end.saturating_sub(from);
    let output = species_create(ctx, r.object, count)?;
    let out = ctx.get(output);
    copy_range(ctx, r, out, from, 0, count, false)?;
    set_len(ctx, output, count)?;
    Ok(out)
}

pub(super) fn splice<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    copy: bool,
) -> JsResult<Value> {
    let start = bound(ctx, args.get(0), r.len, 0)?;
    let remove = match args.len() {
        0 => 0,
        1 => r.len - start,
        _ => to_integer_or_infinity(ctx, args.get(1).unwrap())?.clamp(0.0, (r.len - start) as f64)
            as u64,
    };
    let insert = args.len().saturating_sub(2) as u64;
    let new_len = r.len - remove + insert;
    if new_len > MAX_LENGTH {
        return Err(ctx.throw_type_error("Array exceeds maximum length"));
    }
    let output = if copy {
        array_of_length(ctx, new_len)?
    } else {
        species_create(ctx, r.object, remove)?
    };
    let out = ctx.get(output);
    if copy {
        copy_range(ctx, r, out, 0, 0, start, true)?;
    } else {
        copy_range(ctx, r, out, start, 0, remove, false)?;
        set_len(ctx, output, remove)?;
        let tail = r.len - start - remove;
        if insert < remove {
            transfer(ctx, r, start + remove, start + insert, tail, false)?;
            for i in (new_len..r.len).rev() {
                r.at(ctx, |ctx, object| delete_index(ctx, object, i))?;
            }
        } else if insert > remove {
            transfer(ctx, r, start + remove, start + insert, tail, true)?;
        }
    }
    for i in 0..insert {
        let value = ctx.get(args.get(i as usize + 2).unwrap());
        r.at(ctx, |ctx, object| {
            if copy {
                let output = ctx.root(out)?;
                create_index(ctx, output, start + i, value)
            } else {
                set_index(ctx, object, start + i, value)
            }
        })?;
    }
    if copy {
        copy_range(
            ctx,
            r,
            out,
            start + remove,
            start + insert,
            r.len - start - remove,
            true,
        )?;
    } else {
        set_len(ctx, r.object, new_len)?;
    }
    Ok(out)
}

pub(super) fn flat<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    r: Receiver<'s>,
    args: Args<'s>,
    map: bool,
) -> JsResult<Value> {
    let a0 = arg(ctx, args, 0)?;
    let depth = if map {
        if !callable(ctx, ctx.get(a0)) {
            return Err(ctx.throw_type_error("Callback is not callable"));
        }
        1.0
    } else if ctx.get(a0) == Value::UNDEFINED {
        1.0
    } else {
        to_integer_or_infinity(ctx, a0)?.max(0.0)
    };
    let mapper = if map { Some(ctx.get(a0)) } else { None };
    let this = arg(ctx, args, 1)?;
    let this = ctx.get(this);
    let output = species_create(ctx, r.object, 0)?;
    let out = ctx.get(output);
    let sources = ctx.new_array()?;
    let sources_id = ctx.object_id(sources)?;
    let sources = ctx.get(sources);
    let mut frames = TrackedVec::new();
    let mut next = 0;
    let source = ctx.get(r.object);
    ctx.allocate(|runtime| frames.push(&runtime.heap.ledger, (0, r.len, depth)))?;
    let source_root = ctx.root(sources)?;
    create_index(ctx, source_root, 0, source)?;
    while let Some(&(from, len, depth)) = frames.last() {
        let level = frames.len() - 1;
        let ObjectData::Array(inputs) = &ctx.heap().object(sources_id).data else {
            unreachable!()
        };
        let source = inputs.elements[level];
        if from == len {
            frames.pop();
            let ObjectData::Array(inputs) =
                &mut ctx.runtime.heap.objects.get_mut(sources_id).unwrap().data
            else {
                unreachable!()
            };
            inputs.elements.pop();
            continue;
        }
        frames[level].0 += 1;
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let object = ctx.root(source)?;
            let Some(value) = present(ctx, object, from)? else {
                return Ok(());
            };
            let mut value = ctx.root(value)?;
            if let Some(mapper) = mapper.filter(|_| level == 0) {
                let mapper = ctx.root(mapper)?;
                let this = ctx.root(this)?;
                let index = ctx.root(Value::number(from as f64))?;
                let args = ctx.args(&[value, index, object])?;
                let mapped = call(ctx, mapper, this, args)?;
                value = ctx.root(mapped)?;
            }
            if depth > 0.0 && is_array(ctx, ctx.get(value))? {
                let len = get_length(ctx, value)?;
                if !ctx.runtime.vm.check_budget(frames.len(), 0, 0) {
                    return Err(ctx.stack_overflow());
                }
                ctx.allocate(|runtime| frames.push(&runtime.heap.ledger, (0, len, depth - 1.0)))?;
                let sources = ctx.root(sources)?;
                create_index(ctx, sources, frames.len() as u64 - 1, ctx.get(value))?;
            } else {
                if next == MAX_LENGTH {
                    return Err(ctx.throw_type_error("Array exceeds maximum length"));
                }
                let output = ctx.root(out)?;
                create_index(ctx, output, next, ctx.get(value))?;
                next += 1;
            }
            Ok(())
        })?;
    }
    Ok(out)
}
