use super::*;
use crate::ledger::{AllocResult, HeapLedger, TrackedBox, TrackedVec};
use crate::object::{ArrayBufferData, SharedArrayBufferData};

pub(super) fn install<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<()> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    for (offset, name, ctor, proto) in [
        (
            AB,
            "ArrayBuffer",
            ctx.realm().array_buffer_ctor,
            ctx.realm().array_buffer_proto,
        ),
        (
            SAB,
            "SharedArrayBuffer",
            ctx.realm().shared_array_buffer_ctor,
            ctx.realm().shared_array_buffer_proto,
        ),
    ] {
        let ctor = configure_ctor(ctx, ctor, id(offset), name, 1)?;
        let proto = ctx.root(Value::object(proto))?;
        put(ctx, global, name, ctx.get(ctor), method_flags())?;
        species(ctx, ctor)?;
        tag(ctx, proto, name)?;
        if offset == AB {
            let key = ctx.intern_str("isView")?;
            install_method(ctx, ctor, key, id(IS_VIEW), "isView", 1)?;
            getters(
                ctx,
                proto,
                BUFFER_START,
                &["byteLength", "maxByteLength", "resizable", "detached"],
            )?;
            methods(
                ctx,
                proto,
                BUFFER_START + 4,
                &[
                    ("resize", 1),
                    ("slice", 2),
                    ("transfer", 0),
                    ("transferToFixedLength", 0),
                ],
            )?;
        } else {
            getters(
                ctx,
                proto,
                BUFFER_START + 8,
                &["byteLength", "maxByteLength", "growable"],
            )?;
            methods(ctx, proto, BUFFER_START + 11, &[("grow", 1), ("slice", 2)])?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct Info {
    pub(super) len: usize,
    pub(super) max: Option<usize>,
    pub(super) detached: bool,
    pub(super) shared: bool,
}

pub(super) fn info(heap: &Heap, obj: ObjId) -> Option<Info> {
    match &heap.object(obj).data {
        ObjectData::ArrayBuffer(data) => Some(Info {
            len: data.bytes.len(),
            max: data.max_byte_length,
            detached: data.detached,
            shared: false,
        }),
        ObjectData::SharedArrayBuffer(data) => Some(Info {
            len: data.backing.byte_length(),
            max: data.backing.max_byte_length(),
            detached: false,
            shared: true,
        }),
        _ => None,
    }
}

pub(super) fn shared(heap: &Heap, obj: ObjId) -> Option<&Arc<SharedBufferBacking>> {
    if let ObjectData::SharedArrayBuffer(data) = &heap.object(obj).data {
        Some(&data.backing)
    } else {
        None
    }
}

pub(super) fn same_storage(heap: &Heap, a: ObjId, b: ObjId) -> bool {
    a == b
        || shared(heap, a)
            .zip(shared(heap, b))
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
}

pub(super) fn read(heap: &Heap, buffer: ObjId, offset: usize, out: &mut [u8]) {
    match &heap.object(buffer).data {
        ObjectData::ArrayBuffer(b) => out.copy_from_slice(&b.bytes[offset..][..out.len()]),
        ObjectData::SharedArrayBuffer(b) => b.backing.read(offset, out),
        _ => unreachable!(),
    }
}

pub(super) fn write(ctx: &mut ExecCtx<'_, '_>, buffer: ObjId, offset: usize, input: &[u8]) {
    match &mut ctx.runtime.heap.objects.get_mut(buffer).unwrap().data {
        ObjectData::ArrayBuffer(b) => b.bytes[offset..][..input.len()].copy_from_slice(input),
        ObjectData::SharedArrayBuffer(b) => b.backing.write(offset, input),
        _ => unreachable!(),
    }
}

pub(super) fn copy(
    ctx: &mut ExecCtx<'_, '_>,
    source: ObjId,
    from: usize,
    target: ObjId,
    to: usize,
    len: usize,
) {
    if len == 0 {
        return;
    }
    if source == target
        && let ObjectData::ArrayBuffer(b) =
            &mut ctx.runtime.heap.objects.get_mut(source).unwrap().data
    {
        b.bytes.copy_within(from..from + len, to);
        return;
    }
    let mut block = [0; 256];
    let backward = same_storage(ctx.heap(), source, target) && to > from;
    let mut copied = 0;
    while copied < len {
        let n = (len - copied).min(block.len());
        let i = if backward { len - copied - n } else { copied };
        read(ctx.heap(), source, from + i, &mut block[..n]);
        write(ctx, target, to + i, &block[..n]);
        copied += n;
    }
}

fn resize_data(bytes: &mut TrackedVec<u8>, len: usize, ledger: &HeapLedger) -> AllocResult<()> {
    if len == 0 {
        *bytes = TrackedVec::new();
    } else if len <= bytes.len() {
        bytes.truncate(len);
    } else {
        bytes.try_reserve_exact(ledger, len - bytes.len())?;
        while bytes.len() < len {
            bytes.push_reserved(0);
        }
    }
    Ok(())
}

pub(super) fn detach(ctx: &mut ExecCtx<'_, '_>, obj: ObjId) -> JsResult<()> {
    let ObjectData::ArrayBuffer(data) = &mut ctx.runtime.heap.objects.get_mut(obj).unwrap().data
    else {
        return Err(ctx.throw_type_error("Expected an ArrayBuffer"));
    };
    data.detached = true;
    data.bytes = TrackedVec::new();
    Ok(())
}

pub(super) fn new_array_buffer<'s>(ctx: &mut ExecCtx<'_, 's>, len: usize) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().array_buffer_proto;
    allocate(ctx, proto, len, None, false)
}

fn allocate<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: ObjId,
    len: usize,
    max: Option<usize>,
    shared: bool,
) -> JsResult<Handle<'s>> {
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        if shared {
            let backing = SharedArrayBufferData::new(ledger, len, max)?;
            Ok(ObjectData::SharedArrayBuffer(TrackedBox::new(
                ledger, backing,
            )?))
        } else {
            let mut bytes = TrackedVec::new();
            resize_data(&mut bytes, len, ledger)?;
            Ok(ObjectData::ArrayBuffer(TrackedBox::new(
                ledger,
                ArrayBufferData {
                    bytes,
                    max_byte_length: max,
                    detached: false,
                },
            )?))
        }
    })
}

pub(super) fn wrap_shared<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    backing: Arc<SharedBufferBacking>,
) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().shared_array_buffer_proto;
    ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        let data = SharedArrayBufferData::from_arc(ledger, backing.clone())?;
        Ok(ObjectData::SharedArrayBuffer(TrackedBox::new(
            ledger, data,
        )?))
    })
}

pub(super) fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    args: Args<'s>,
    shared: bool,
) -> JsResult<Value> {
    let arg = item(ctx, args, 0)?;
    let len = index_arg(ctx, arg)?;
    let mut max = None;
    if let Some(options) = args.get(1).filter(|&h| ctx.get(h).as_object().is_some()) {
        let limit = named(ctx, options, "maxByteLength")?;
        if limit != Value::UNDEFINED {
            let limit = ctx.root(limit)?;
            max = Some(index_arg(ctx, limit)?);
        }
    }
    if max.is_some_and(|n| len > n) {
        return Err(ctx.throw_range_error("Length exceeds maxByteLength"));
    }
    let proto = proto_from_target(ctx, target, |realm| {
        if shared {
            realm.shared_array_buffer_proto
        } else {
            realm.array_buffer_proto
        }
    })?;
    let proto = ctx.object_id(proto)?;
    let result = allocate(ctx, proto, len, max, shared)?;
    Ok(ctx.get(result))
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let obj = ctx.object_id(this)?;
    let shared = op >= 8;
    let state = info(ctx.heap(), obj)
        .filter(|i| i.shared == shared)
        .ok_or_else(|| {
            ctx.throw_type_error(if shared {
                "Expected a SharedArrayBuffer"
            } else {
                "Expected an ArrayBuffer"
            })
        })?;
    match op {
        0 | 8 => Ok(number(state.len)),
        1 | 9 => Ok(number(if state.detached {
            0
        } else {
            state.max.unwrap_or(state.len)
        })),
        2 | 10 => Ok(Value::bool(state.max.is_some())),
        3 => Ok(Value::bool(state.detached)),
        4 | 11 => {
            let Some(max) = state.max else {
                return Err(ctx.throw_type_error("Buffer has a fixed length"));
            };
            let arg = item(ctx, args, 0)?;
            let len = index_arg(ctx, arg)?;
            if info(ctx.heap(), obj).unwrap().detached {
                return Err(detached(ctx));
            }
            if shared {
                if !self::shared(ctx.heap(), obj).unwrap().grow(len) {
                    return Err(ctx.throw_range_error("Invalid SharedArrayBuffer length"));
                }
            } else {
                if len > max {
                    return Err(ctx.throw_range_error("Length exceeds maxByteLength"));
                }
                resize(ctx, obj, len)?;
            }
            Ok(Value::UNDEFINED)
        }
        5 | 12 => slice(ctx, this, obj, state, args),
        6 | 7 => {
            let arg = item(ctx, args, 0)?;
            let len = if ctx.get(arg) == Value::UNDEFINED {
                state.len
            } else {
                index_arg(ctx, arg)?
            };
            if info(ctx.heap(), obj).unwrap().detached {
                return Err(detached(ctx));
            }
            let max = if op == 6 { state.max } else { None };
            if max.is_some_and(|n| len > n) {
                return Err(ctx.throw_range_error("Length exceeds maxByteLength"));
            }
            let proto = ctx.realm().array_buffer_proto;
            let result = allocate(ctx, proto, 0, max, false)?;
            resize(ctx, obj, len)?;
            let dest = ctx.object_id(result)?;
            let mut bytes = TrackedVec::new();
            let ObjectData::ArrayBuffer(src) =
                &mut ctx.runtime.heap.objects.get_mut(obj).unwrap().data
            else {
                unreachable!()
            };
            std::mem::swap(&mut src.bytes, &mut bytes);
            src.detached = true;
            let ObjectData::ArrayBuffer(dst) =
                &mut ctx.runtime.heap.objects.get_mut(dest).unwrap().data
            else {
                unreachable!()
            };
            dst.bytes = bytes;
            Ok(ctx.get(result))
        }
        _ => unreachable!(),
    }
}

fn resize(ctx: &mut ExecCtx<'_, '_>, obj: ObjId, len: usize) -> JsResult<()> {
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::ArrayBuffer(data) = &mut heap.objects.get_mut(obj).unwrap().data else {
            unreachable!()
        };
        resize_data(&mut data.bytes, len, &heap.ledger)
    })
}

fn slice<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    source: ObjId,
    state: Info,
    args: Args<'s>,
) -> JsResult<Value> {
    if state.detached {
        return Err(detached(ctx));
    }
    let start = item(ctx, args, 0)?;
    let start = relative(convert::to_integer_or_infinity(ctx, start)?, state.len);
    let end = item(ctx, args, 1)?;
    let end = if ctx.get(end) == Value::UNDEFINED {
        state.len
    } else {
        relative(convert::to_integer_or_infinity(ctx, end)?, state.len)
    };
    let count = end.saturating_sub(start);
    let dest = if let Some(ctor) = species_constructor(ctx, this)? {
        let len = ctx.root(number(count))?;
        let args = ctx.args(&[len])?;
        let result = call::construct(ctx, ctor, args, ctor)?;
        ctx.root(result)?
    } else {
        let proto = if state.shared {
            ctx.realm().shared_array_buffer_proto
        } else {
            ctx.realm().array_buffer_proto
        };
        allocate(ctx, proto, count, None, state.shared)?
    };
    let target = ctx.object_id(dest)?;
    let target_info = info(ctx.heap(), target)
        .filter(|i| i.shared == state.shared && !i.detached && i.len >= count);
    if target_info.is_none() || same_storage(ctx.heap(), source, target) {
        return Err(ctx.throw_type_error("Invalid buffer returned by species constructor"));
    }
    let src = info(ctx.heap(), source).unwrap();
    if src.detached {
        return Err(detached(ctx));
    }
    copy(
        ctx,
        source,
        start,
        target,
        0,
        count.min(src.len.saturating_sub(start)),
    );
    Ok(ctx.get(dest))
}

pub(super) fn detached(ctx: &mut ExecCtx<'_, '_>) -> crate::Abrupt {
    ctx.throw_type_error("ArrayBuffer is detached")
}
