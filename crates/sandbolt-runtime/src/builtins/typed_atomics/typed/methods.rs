use std::cmp::Ordering;

use super::*;
use crate::builtins::array_iter::new_array_iterator;
use crate::ledger::TrackedVec;
use crate::object::ArrayIteratorKind;
use crate::ops::compare;

const AT: u16 = 0;
pub(super) const COPY_WITHIN: u16 = 1;
const ENTRIES: u16 = 2;
const EVERY: u16 = 3;
pub(super) const FILL: u16 = 4;
const FILTER: u16 = 5;
const FIND: u16 = 6;
const FIND_INDEX: u16 = 7;
const FIND_LAST: u16 = 8;
const FIND_LAST_INDEX: u16 = 9;
const FOR_EACH: u16 = 10;
const INCLUDES: u16 = 11;
const INDEX_OF: u16 = 12;
const JOIN: u16 = 13;
const KEYS: u16 = 14;
const LAST_INDEX_OF: u16 = 15;
const MAP: u16 = 16;
const REDUCE: u16 = 17;
const REDUCE_RIGHT: u16 = 18;
pub(super) const REVERSE: u16 = 19;
pub(super) const SET: u16 = 20;
const SLICE: u16 = 21;
const SOME: u16 = 22;
pub(super) const SORT: u16 = 23;
pub(super) const SUBARRAY: u16 = 24;
const TO_LOCALE_STRING: u16 = 25;
const TO_REVERSED: u16 = 26;
pub(super) const TO_SORTED: u16 = 27;
const VALUES: u16 = 28;
const WITH: u16 = 29;

pub(super) const TABLE: &[(&str, u32)] = &[
    ("at", 1),
    ("copyWithin", 2),
    ("entries", 0),
    ("every", 1),
    ("fill", 1),
    ("filter", 1),
    ("find", 1),
    ("findIndex", 1),
    ("findLast", 1),
    ("findLastIndex", 1),
    ("forEach", 1),
    ("includes", 1),
    ("indexOf", 1),
    ("join", 1),
    ("keys", 0),
    ("lastIndexOf", 1),
    ("map", 1),
    ("reduce", 1),
    ("reduceRight", 1),
    ("reverse", 0),
    ("set", 1),
    ("slice", 2),
    ("some", 1),
    ("sort", 1),
    ("subarray", 2),
    ("toLocaleString", 0),
    ("toReversed", 0),
    ("toSorted", 1),
    ("values", 0),
    ("with", 2),
];

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    view: TypedArrayData,
    len: usize,
) -> JsResult<Value> {
    match op {
        AT => {
            let index = item(ctx, args, 0)?;
            let index = convert::to_integer_or_infinity(ctx, index)?;
            let index = if index < 0.0 {
                len as f64 + index
            } else {
                index
            };
            if index < 0.0 || index >= len as f64 {
                Ok(Value::UNDEFINED)
            } else {
                get(ctx, view, index as usize)
            }
        }
        COPY_WITHIN => {
            let to = position(ctx, args, 0, len, 0)?;
            let from = position(ctx, args, 1, len, 0)?;
            let end = position(ctx, args, 2, len, len)?;
            let current = length(ctx.heap(), view).ok_or_else(|| oob(ctx))?;
            let count = end
                .saturating_sub(from)
                .min(len - to)
                .min(current.saturating_sub(to.max(from)));
            let size = view.kind.size();
            buffer::copy(
                ctx,
                view.buffer,
                view.byte_offset + from * size,
                view.buffer,
                view.byte_offset + to * size,
                count * size,
            );
            Ok(ctx.get(this))
        }
        ENTRIES | KEYS | VALUES => {
            let kind = match op {
                ENTRIES => ArrayIteratorKind::Entry,
                KEYS => ArrayIteratorKind::Key,
                _ => ArrayIteratorKind::Value,
            };
            new_array_iterator(ctx, this, kind).map(|iter| ctx.get(iter))
        }
        EVERY | FILTER | FIND | FIND_INDEX | FIND_LAST | FIND_LAST_INDEX | FOR_EACH | MAP
        | SOME => callbacks(op, ctx, this, args, view, len),
        FILL => {
            let value = item(ctx, args, 0)?;
            let bits = elements::encode(ctx, view.kind, value)?;
            let start = position(ctx, args, 1, len, 0)?;
            let end = position(ctx, args, 2, len, len)?;
            let current = length(ctx.heap(), view).ok_or_else(|| oob(ctx))?;
            for k in start..end.min(current) {
                write_raw(ctx, view, k, bits);
            }
            Ok(ctx.get(this))
        }
        INCLUDES | INDEX_OF | LAST_INDEX_OF => search(op, ctx, args, view, len),
        JOIN | TO_LOCALE_STRING => join(ctx, args, view, len, op == TO_LOCALE_STRING),
        REDUCE | REDUCE_RIGHT => reduce(ctx, this, args, view, len, op == REDUCE_RIGHT),
        REVERSE => {
            for i in 0..len / 2 {
                let j = len - 1 - i;
                let first = raw(ctx.heap(), view, i);
                let last = raw(ctx.heap(), view, j);
                write_raw(ctx, view, i, last);
                write_raw(ctx, view, j, first);
            }
            Ok(ctx.get(this))
        }
        SET => set(ctx, args, view),
        SLICE | SUBARRAY => slice(ctx, this, args, view, len, op == SUBARRAY),
        SORT | TO_SORTED => sort(ctx, this, args, view, op == TO_SORTED),
        TO_REVERSED => {
            let target = intrinsic(ctx, view.kind, len)?;
            let dest = data(ctx.heap(), ctx.get(target).as_object().unwrap()).unwrap();
            for i in 0..len {
                let bits = raw(ctx.heap(), view, len - 1 - i);
                write_raw(ctx, dest, i, bits);
            }
            Ok(ctx.get(target))
        }
        WITH => {
            let index = item(ctx, args, 0)?;
            let index = convert::to_integer_or_infinity(ctx, index)?;
            let index = if index < 0.0 {
                len as f64 + index
            } else {
                index
            };
            let val = item(ctx, args, 1)?;
            let bits = elements::encode(ctx, view.kind, val)?;
            let current = length(ctx.heap(), view).ok_or_else(|| oob(ctx))?;
            if index < 0.0 || index >= current as f64 {
                return Err(ctx.throw_range_error("TypedArray index is out of bounds"));
            }
            let target = intrinsic(ctx, view.kind, len)?;
            let dest = data(ctx.heap(), ctx.get(target).as_object().unwrap()).unwrap();
            let size = view.kind.size();
            buffer::copy(
                ctx,
                view.buffer,
                view.byte_offset,
                dest.buffer,
                0,
                len.min(current) * size,
            );
            put_element(ctx, dest, index as usize, bits);
            if current < len {
                let undefined = ctx.root(Value::UNDEFINED)?;
                let bits = elements::encode(ctx, view.kind, undefined)?;
                for i in current..len {
                    write_raw(ctx, dest, i, bits);
                }
            }
            Ok(ctx.get(target))
        }
        _ => unreachable!(),
    }
}

fn position<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
    i: usize,
    len: usize,
    default: usize,
) -> JsResult<usize> {
    let arg = item(ctx, args, i)?;
    if ctx.get(arg) == Value::UNDEFINED {
        Ok(default)
    } else {
        Ok(relative(convert::to_integer_or_infinity(ctx, arg)?, len))
    }
}

fn search<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
    view: TypedArrayData,
    len: usize,
) -> JsResult<Value> {
    let miss = if op == INCLUDES {
        Value::bool(false)
    } else {
        Value::int32(-1)
    };
    if len == 0 {
        return Ok(miss);
    }
    let needle = item(ctx, args, 0)?;
    let from = if let Some(from) = args.get(1) {
        convert::to_integer_or_infinity(ctx, from)?
    } else if op == LAST_INDEX_OF {
        (len - 1) as f64
    } else {
        0.0
    };
    let needle = ctx.get(needle);
    let current = length(ctx.heap(), view).unwrap_or(0);
    let max = if op == INCLUDES {
        len
    } else {
        len.min(current)
    };
    let start = if from < 0.0 { len as f64 + from } else { from };
    let (lo, hi) = if op == LAST_INDEX_OF {
        if start < 0.0 {
            return Ok(miss);
        }
        (0, max.min((start as usize).saturating_add(1)))
    } else {
        ((start.max(0.0) as usize).min(max), max)
    };
    for offset in 0..(hi - lo) {
        let i = if op == LAST_INDEX_OF {
            hi - 1 - offset
        } else {
            lo + offset
        };
        let matched = ctx.with_scope(|ctx| {
            let val = get(ctx, view, i)?;
            let val = ctx.root(val)?;
            let needle = ctx.root(needle)?;
            Ok(if op == INCLUDES {
                compare::same_value_zero(ctx, val, needle)
            } else {
                compare::strict_eq(ctx, val, needle)
            })
        })?;
        if matched {
            return Ok(if op == INCLUDES {
                Value::bool(true)
            } else {
                number(i)
            });
        }
    }
    Ok(miss)
}

fn callbacks<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    view: TypedArrayData,
    len: usize,
) -> JsResult<Value> {
    let func = item(ctx, args, 0)?;
    require_callable(ctx, func)?;
    let binding = item(ctx, args, 1)?;
    let mut map = None;
    let mut kept = None;
    if op == MAP {
        let length = ctx.root(number(len))?;
        map = Some(species_create(ctx, this, view, &[length], Some(len))?);
    } else if op == FILTER {
        kept = Some(ctx.new_array()?);
    }
    let (func, binding, this_raw) = (ctx.get(func), ctx.get(binding), ctx.get(this));
    let kept_raw = kept.map(|h| ctx.get(h));
    let mut count = 0;
    for k in 0..len {
        let i = if matches!(op, FIND_LAST | FIND_LAST_INDEX) {
            len - k - 1
        } else {
            k
        };
        let found = ctx.with_scope(|ctx| {
            let func = ctx.root(func)?;
            let binding = ctx.root(binding)?;
            let this = ctx.root(this_raw)?;
            let value = get(ctx, view, i)?;
            let value = ctx.root(value)?;
            let index = ctx.root(number(i))?;
            let args = ctx.args(&[value, index, this])?;
            let mapped = call::call(ctx, func, binding, args)?;
            if let Some((_, dest)) = map {
                let mapped = ctx.root(mapped)?;
                let bits = elements::encode(ctx, dest.kind, mapped)?;
                put_element(ctx, dest, i, bits);
                return Ok(None);
            }
            let yes = convert::to_boolean_value(ctx.heap(), mapped);
            if let Some(kept) = kept_raw.filter(|_| yes) {
                let kept = ctx.root(kept)?;
                let key = key_at(ctx, count)?;
                define_data(ctx, kept, key, value, PropFlags::DEFAULT_DATA)?;
                count += 1;
            }
            Ok(match op {
                EVERY if !yes => Some(Value::bool(false)),
                SOME if yes => Some(Value::bool(true)),
                FIND | FIND_LAST if yes => Some(ctx.get(value)),
                FIND_INDEX | FIND_LAST_INDEX if yes => Some(number(i)),
                _ => None,
            })
        })?;
        if let Some(found) = found {
            return Ok(found);
        }
    }
    if let Some((output, _)) = map {
        return Ok(ctx.get(output));
    }
    if let Some(kept) = kept {
        let size = ctx.root(number(count))?;
        let (output, _) = species_create(ctx, this, view, &[size], Some(count))?;
        copy_array_like(ctx, kept, output, count, None)?;
        return Ok(ctx.get(output));
    }
    Ok(match op {
        EVERY => Value::bool(true),
        SOME => Value::bool(false),
        FIND_INDEX | FIND_LAST_INDEX => Value::int32(-1),
        _ => Value::UNDEFINED,
    })
}

fn reduce<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    view: TypedArrayData,
    len: usize,
    reverse: bool,
) -> JsResult<Value> {
    let callback = item(ctx, args, 0)?;
    require_callable(ctx, callback)?;
    let index = |i| if reverse { len - 1 - i } else { i };
    let (start, first) = if let Some(initial) = args.get(1) {
        (0, ctx.get(initial))
    } else {
        if len == 0 {
            return Err(ctx.throw_type_error("Reduce of an empty TypedArray"));
        }
        (1, get(ctx, view, index(0))?)
    };
    let acc = ctx.mut_root(first)?;
    let (callback, this) = (ctx.get(callback), ctx.get(this));
    for i in start..len {
        let current = ctx.get(acc.handle());
        let next = ctx.with_scope(|ctx| {
            let acc = ctx.root(current)?;
            let callback = ctx.root(callback)?;
            let this = ctx.root(this)?;
            let binding = ctx.root(Value::UNDEFINED)?;
            let value = get(ctx, view, index(i))?;
            let value = ctx.root(value)?;
            let index = ctx.root(number(index(i)))?;
            let args = ctx.args(&[acc, value, index, this])?;
            call::call(ctx, callback, binding, args)
        })?;
        ctx.set(&acc, next);
    }
    Ok(ctx.get(acc.handle()))
}

fn join<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    args: Args<'s>,
    view: TypedArrayData,
    len: usize,
    locale: bool,
) -> JsResult<Value> {
    let separator = item(ctx, args, 0)?;
    let default = || [u16::from(b',')];
    let separator = if locale || ctx.get(separator) == Value::UNDEFINED {
        None
    } else {
        Some(stringify(ctx, separator)?)
    };
    let mut units = TrackedVec::new();
    for i in 0..len {
        if i != 0 {
            if let Some(id) = separator {
                append_units(ctx, &mut units, id)?;
            } else {
                ctx.allocate(|rt| units.extend_from_slice(&rt.heap.ledger, &default()))?;
            }
        }
        ctx.with_scope(|ctx| {
            let value = get(ctx, view, i)?;
            if value == Value::UNDEFINED {
                return Ok(());
            }
            let value = ctx.root(value)?;
            let value = if locale {
                let obj = convert::to_object(ctx, value)?;
                let func = named(ctx, obj, "toLocaleString")?;
                let func = ctx.root(func)?;
                let text = call::call(ctx, func, value, Args::empty())?;
                ctx.root(text)?
            } else {
                value
            };
            let string = stringify(ctx, value)?;
            append_units(ctx, &mut units, string)
        })?;
    }
    ctx.new_utf16(&units).map(|str| ctx.get(str))
}

fn append_units(
    ctx: &mut ExecCtx<'_, '_>,
    to: &mut TrackedVec<u16>,
    str: crate::value::StrId,
) -> JsResult<()> {
    let len = ctx.heap().string(str).len() as usize;
    ctx.allocate(|rt| to.try_reserve(&rt.heap.ledger, len))?;
    for c in ctx.heap().string_units(str) {
        to.push_reserved(c);
    }
    Ok(())
}

fn set<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, dest: TypedArrayData) -> JsResult<Value> {
    let offset = item(ctx, args, 1)?;
    let offset = convert::to_integer_or_infinity(ctx, offset)?;
    if offset < 0.0 {
        return Err(ctx.throw_range_error("Negative TypedArray offset"));
    }
    let offset = offset as usize;
    let dest_len = length(ctx.heap(), dest).ok_or_else(|| oob(ctx))?;
    let src = item(ctx, args, 0)?;
    if let Some(source) = ctx.get(src).as_object().and_then(|o| data(ctx.heap(), o)) {
        let src_len = length(ctx.heap(), source).ok_or_else(|| oob(ctx))?;
        if src_len > dest_len.checked_sub(offset).ok_or_else(|| too_long(ctx))? {
            return Err(too_long(ctx));
        }
        if source.kind.is_bigint() != dest.kind.is_bigint() {
            return Err(ctx.throw_type_error("Cannot mix Number and BigInt arrays"));
        }
        if source.kind == dest.kind {
            let size = source.kind.size();
            buffer::copy(
                ctx,
                source.buffer,
                source.byte_offset,
                dest.buffer,
                dest.byte_offset + size * offset,
                src_len * size,
            );
        } else if buffer::same_storage(ctx.heap(), source.buffer, dest.buffer) {
            let copy = allocate_length(
                ctx,
                ctx.realm().typed_array_concrete[source.kind as usize]
                    .unwrap()
                    .1,
                source.kind,
                src_len,
            )?;
            let copied = data(ctx.heap(), ctx.get(copy).as_object().unwrap()).unwrap();
            buffer::copy(
                ctx,
                source.buffer,
                source.byte_offset,
                copied.buffer,
                0,
                src_len * source.kind.size(),
            );
            convert_range(ctx, copied, 0, dest, offset, src_len)?;
        } else {
            convert_range(ctx, source, 0, dest, offset, src_len)?;
        }
    } else {
        let source = convert::to_object(ctx, src)?;
        let len = named(ctx, source, "length")?;
        let len = ctx.root(len)?;
        let len = convert::to_length(ctx, len)? as usize;
        if len > dest_len.checked_sub(offset).ok_or_else(|| too_long(ctx))? {
            return Err(too_long(ctx));
        }
        let source = ctx.get(source);
        for i in 0..len {
            ctx.with_scope(|ctx| {
                let source = ctx.root(source)?;
                let key = key_at(ctx, i)?;
                let value = property::get_property(ctx, source, key)?;
                let value = ctx.root(value)?;
                let bits = elements::encode(ctx, dest.kind, value)?;
                put_element(ctx, dest, offset + i, bits);
                Ok(())
            })?;
        }
    }
    Ok(Value::UNDEFINED)
}

fn too_long(ctx: &mut ExecCtx<'_, '_>) -> crate::Abrupt {
    ctx.throw_range_error("Source exceeds the TypedArray length")
}

fn slice<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    view: TypedArrayData,
    len: usize,
    subarray: bool,
) -> JsResult<Value> {
    let first = position(ctx, args, 0, len, 0)?;
    let end_arg = item(ctx, args, 1)?;
    let no_end = ctx.get(end_arg) == Value::UNDEFINED;
    let end = if no_end {
        len
    } else {
        relative(convert::to_integer_or_infinity(ctx, end_arg)?, len)
    };
    let count = end.saturating_sub(first);
    let size = view.kind.size();
    let len_arg = ctx.root(number(count))?;
    if subarray {
        let buffer = ctx.root(Value::object(view.buffer))?;
        let offset = ctx.root(number(view.byte_offset + first * size))?;
        let args = [buffer, offset, len_arg];
        let args = &args[..if no_end && view.length.is_none() {
            2
        } else {
            3
        }];
        let (result, _) = species_create(ctx, this, view, args, None)?;
        return Ok(ctx.get(result));
    }
    let (result, dest) = species_create(ctx, this, view, &[len_arg], Some(count))?;
    if count != 0 {
        let current = length(ctx.heap(), view).ok_or_else(|| oob(ctx))?;
        let count = count.min(current.saturating_sub(first));
        if dest.kind == view.kind {
            let from = view.byte_offset + first * size;
            if buffer::same_storage(ctx.heap(), view.buffer, dest.buffer)
                && dest.byte_offset > from
                && dest.byte_offset < from + count * size
            {
                for i in 0..count {
                    let bits = raw(ctx.heap(), view, first + i);
                    write_raw(ctx, dest, i, bits);
                }
            } else {
                buffer::copy(
                    ctx,
                    view.buffer,
                    from,
                    dest.buffer,
                    dest.byte_offset,
                    count * size,
                );
            }
        } else {
            convert_range(ctx, view, first, dest, 0, count)?;
        }
    }
    Ok(ctx.get(result))
}

fn sort<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    view: TypedArrayData,
    copy: bool,
) -> JsResult<Value> {
    let func = item(ctx, args, 0)?;
    let func = if ctx.get(func) == Value::UNDEFINED {
        None
    } else {
        require_callable(ctx, func)?;
        Some(ctx.get(func))
    };
    let n = length(ctx.heap(), view).ok_or_else(|| oob(ctx))?;
    let mut values = TrackedVec::new();
    ctx.allocate(|rt| values.try_reserve_exact(&rt.heap.ledger, n))?;
    for i in 0..n {
        values.push_reserved(raw(ctx.heap(), view, i));
    }
    if n <= crate::builtins::INSERTION_SORT_LIMIT {
        crate::builtins::stable_insertion_sort(&mut values, |left, right| {
            cmp(ctx, view.kind, func, left, right)
        })?;
    } else {
        let mut scratch = TrackedVec::new();
        ctx.allocate(|rt| scratch.try_reserve_exact(&rt.heap.ledger, n))?;
        for _ in 0..n {
            scratch.push_reserved(0);
        }
        let mut width = 1usize;
        while width < n {
            let mut start = 0;
            while start < n {
                let mid = (start + width).min(n);
                let end = (mid + width).min(n);
                let (mut i, mut j) = (start, mid);
                for out in &mut scratch[start..end] {
                    let from_left = j == end
                        || (i < mid
                            && cmp(ctx, view.kind, func, values[i], values[j])?
                                != Ordering::Greater);
                    if from_left {
                        *out = values[i];
                        i += 1;
                    } else {
                        *out = values[j];
                        j += 1;
                    }
                }
                start = end;
            }
            std::mem::swap(&mut values, &mut scratch);
            width *= 2;
        }
    }
    let dest = if copy {
        intrinsic(ctx, view.kind, n)?
    } else {
        this
    };
    let dest_view = data(ctx.heap(), ctx.get(dest).as_object().unwrap()).unwrap();
    let live = length(ctx.heap(), dest_view).unwrap_or(0);
    for (i, &bits) in values[..n.min(live)].iter().enumerate() {
        write_raw(ctx, dest_view, i, bits);
    }
    Ok(ctx.get(dest))
}

fn cmp(
    ctx: &mut ExecCtx<'_, '_>,
    kind: TypedArrayKind,
    func: Option<Value>,
    a: u64,
    b: u64,
) -> JsResult<Ordering> {
    if func.is_none() && kind.is_bigint() {
        return Ok(if kind == TypedArrayKind::BigInt64 {
            (a as i64).cmp(&(b as i64))
        } else {
            a.cmp(&b)
        });
    }
    ctx.with_scope(|ctx| {
        let x = elements::decode(ctx, kind, a)?;
        let x = ctx.root(x)?;
        let y = elements::decode(ctx, kind, b)?;
        let y = ctx.root(y)?;
        if let Some(func) = func {
            let func = ctx.root(func)?;
            let binding = ctx.root(Value::UNDEFINED)?;
            let args = ctx.args(&[x, y])?;
            let order = call::call(ctx, func, binding, args)?;
            let order = ctx.root(order)?;
            let sign = convert::to_number(ctx, order)?;
            return Ok(sign.partial_cmp(&0.0).unwrap_or(Ordering::Equal));
        }
        let x = ctx.get(x).as_number().unwrap();
        let y = ctx.get(y).as_number().unwrap();
        Ok(if x.is_nan() {
            if y.is_nan() {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        } else if y.is_nan() {
            Ordering::Less
        } else {
            x.total_cmp(&y)
        })
    })
}
