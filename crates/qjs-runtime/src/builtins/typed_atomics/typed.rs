use super::*;
use crate::ledger::TrackedBox;
use crate::value::ValueKind;

mod methods;

const TAG_GET: u16 = 4;
const FROM: u16 = 5;
const OF: u16 = 6;
const METHODS: u16 = 7;

pub(super) fn install<'s>(ctx: &mut ExecCtx<'_, 's>) -> JsResult<()> {
    let abstract_ctor = configure_ctor(
        ctx,
        ctx.realm().typed_array_ctor,
        id(ABSTRACT_TA),
        "TypedArray",
        0,
    )?;
    let proto = ctx.root(Value::object(ctx.realm().typed_array_proto))?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    species(ctx, abstract_ctor)?;
    super::methods(
        ctx,
        abstract_ctor,
        TYPED_START + FROM,
        &[("from", 1), ("of", 0)],
    )?;
    getters(
        ctx,
        proto,
        TYPED_START,
        &["buffer", "byteLength", "byteOffset", "length"],
    )?;
    let tag = symbol(ctx, WellKnown::ToStringTag);
    install_accessor(
        ctx,
        proto,
        tag,
        Some((id(TYPED_START + TAG_GET), "get [Symbol.toStringTag]")),
        None,
        true,
    )?;
    super::methods(ctx, proto, TYPED_START + METHODS, methods::TABLE)?;
    let values = named(ctx, proto, "values")?;
    let values = ctx.root(values)?;
    let iterator = symbol(ctx, WellKnown::Iterator);
    define_data(ctx, proto, iterator, values, method_flags())?;
    let array_proto = ctx.root(Value::object(ctx.realm().array_proto))?;
    let to_string = property::get_property(ctx, array_proto, PropKey::atom(Atom::TO_STRING))?;
    let to_string = ctx.root(to_string)?;
    define_data(
        ctx,
        proto,
        PropKey::atom(Atom::TO_STRING),
        to_string,
        method_flags(),
    )?;
    for kind in TypedArrayKind::ALL {
        let abstract_ctor = ctx.get(abstract_ctor);
        let proto = ctx.get(proto);
        let global = ctx.get(global);
        ctx.with_scope(|ctx| {
            let abstract_ctor = ctx.root(abstract_ctor)?;
            let proto = ctx.root(proto)?;
            let global = ctx.root(global)?;
            let ctor = make_native_fn(ctx, id(TA_START + kind as u16), kind.name(), 3, true)?;
            property::set_prototype_of(ctx, ctor, Some(abstract_ctor))?;
            let child = ctx.new_object_with_proto(Some(proto))?;
            define_data(
                ctx,
                ctor,
                PropKey::atom(Atom::PROTOTYPE),
                child,
                PropFlags::default(),
            )?;
            put(ctx, child, "constructor", ctx.get(ctor), method_flags())?;
            let size = number(kind.size());
            put(ctx, child, "BYTES_PER_ELEMENT", size, PropFlags::default())?;
            put(ctx, ctor, "BYTES_PER_ELEMENT", size, PropFlags::default())?;
            put(ctx, global, kind.name(), ctx.get(ctor), method_flags())?;
            let pair = (ctx.object_id(ctor)?, ctx.object_id(child)?);
            let realm = ctx.realm_id();
            ctx.runtime
                .heap
                .realms
                .get_mut(realm)
                .unwrap()
                .typed_array_concrete[kind as usize] = Some(pair);
            if kind == TypedArrayKind::Uint8 {
                encoding::install(ctx, ctor, child)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub(super) fn data(heap: &Heap, obj: ObjId) -> Option<TypedArrayData> {
    if let ObjectData::TypedArray(data) = &heap.object(obj).data {
        Some(**data)
    } else {
        None
    }
}

pub(super) fn length(heap: &Heap, view: TypedArrayData) -> Option<usize> {
    let info = buffer::info(heap, view.buffer)?;
    if info.detached {
        return None;
    }
    let rest = info.len.checked_sub(view.byte_offset)? / view.kind.size();
    match view.length {
        Some(len) if len > rest => None,
        Some(len) => Some(len),
        None => Some(rest),
    }
}

pub(super) fn validate(ctx: &mut ExecCtx<'_, '_>, obj: ObjId) -> JsResult<(TypedArrayData, usize)> {
    let view =
        data(ctx.heap(), obj).ok_or_else(|| ctx.throw_type_error("Expected a TypedArray"))?;
    let n = length(ctx.heap(), view).ok_or_else(|| oob(ctx))?;
    Ok((view, n))
}

fn oob(ctx: &mut ExecCtx<'_, '_>) -> crate::Abrupt {
    ctx.throw_type_error("TypedArray is out of bounds")
}

pub(super) fn get(
    ctx: &mut ExecCtx<'_, '_>,
    view: TypedArrayData,
    index: usize,
) -> JsResult<Value> {
    if index >= length(ctx.heap(), view).unwrap_or(0) {
        return Ok(Value::UNDEFINED);
    }
    let bits = raw(ctx.heap(), view, index);
    elements::decode(ctx, view.kind, bits)
}

pub(super) fn raw(heap: &Heap, view: TypedArrayData, index: usize) -> u64 {
    let size = view.kind.size();
    let mut bytes = [0; 8];
    buffer::read(
        heap,
        view.buffer,
        view.byte_offset + index * size,
        &mut bytes[..size],
    );
    u64::from_le_bytes(bytes)
}

pub(super) fn put_element(
    ctx: &mut ExecCtx<'_, '_>,
    view: TypedArrayData,
    index: usize,
    bits: u64,
) {
    if index < length(ctx.heap(), view).unwrap_or(0) {
        write_raw(ctx, view, index, bits);
    }
}

pub(super) fn write_raw(ctx: &mut ExecCtx<'_, '_>, view: TypedArrayData, index: usize, bits: u64) {
    let size = view.kind.size();
    buffer::write(
        ctx,
        view.buffer,
        view.byte_offset + index * size,
        &bits.to_le_bytes()[..size],
    );
}

fn allocate_view<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: ObjId,
    view: TypedArrayData,
) -> JsResult<Handle<'s>> {
    ctx.make_object(
        Some(proto),
        ObjectFlags::EXTENSIBLE | ObjectFlags::IS_EXOTIC,
        |ledger| Ok(ObjectData::TypedArray(TrackedBox::new(ledger, view)?)),
    )
}

fn allocate_length<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: ObjId,
    kind: TypedArrayKind,
    length: usize,
) -> JsResult<Handle<'s>> {
    let bytes = length
        .checked_mul(kind.size())
        .ok_or_else(|| ctx.throw_range_error("TypedArray length is too large"))?;
    let buffer = buffer::new_array_buffer(ctx, bytes)?;
    let buffer = ctx.object_id(buffer)?;
    allocate_view(
        ctx,
        proto,
        TypedArrayData {
            buffer,
            byte_offset: 0,
            length: Some(length),
            kind,
        },
    )
}

pub(super) fn intrinsic<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    kind: TypedArrayKind,
    length: usize,
) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().typed_array_concrete[kind as usize].unwrap().1;
    allocate_length(ctx, proto, kind, length)
}

pub(super) fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    args: Args<'s>,
    kind: TypedArrayKind,
) -> JsResult<Value> {
    let first = item(ctx, args, 0)?;
    let object = ctx.get(first).as_object();
    let primitive_len = if object.is_none() {
        index_arg(ctx, first)?
    } else {
        0
    };
    let proto = proto_from_target(ctx, target, |realm| {
        realm.typed_array_concrete[kind as usize].unwrap().1
    })?;
    let proto = ctx.object_id(proto)?;
    let result = if let Some(obj) = object {
        if let Some(src) = data(ctx.heap(), obj) {
            let len = length(ctx.heap(), src).ok_or_else(|| oob(ctx))?;
            if src.kind.is_bigint() != kind.is_bigint() {
                return Err(ctx.throw_type_error("Cannot mix Number and BigInt arrays"));
            }
            let result = allocate_length(ctx, proto, kind, len)?;
            let dest = data(ctx.heap(), ctx.get(result).as_object().unwrap()).unwrap();
            if src.kind == kind {
                buffer::copy(
                    ctx,
                    src.buffer,
                    src.byte_offset,
                    dest.buffer,
                    0,
                    len * kind.size(),
                );
            } else {
                convert_range(ctx, src, 0, dest, 0, len)?;
            }
            result
        } else if buffer::info(ctx.heap(), obj).is_some() {
            construct_view(ctx, proto, kind, obj, args)?
        } else {
            let (source, len) = array_like(ctx, first)?;
            let result = allocate_length(ctx, proto, kind, len)?;
            copy_array_like(ctx, source, result, len, None)?;
            result
        }
    } else {
        allocate_length(ctx, proto, kind, primitive_len)?
    };
    Ok(ctx.get(result))
}

fn construct_view<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: ObjId,
    kind: TypedArrayKind,
    buffer: ObjId,
    args: Args<'s>,
) -> JsResult<Handle<'s>> {
    let offset = item(ctx, args, 1)?;
    let byte_offset = index_arg(ctx, offset)?;
    let size = kind.size();
    if byte_offset % size != 0 {
        return Err(ctx.throw_range_error("Unaligned TypedArray offset"));
    }
    let arg = item(ctx, args, 2)?;
    let requested = if ctx.get(arg) == Value::UNDEFINED {
        None
    } else {
        Some(index_arg(ctx, arg)?)
    };
    let info = buffer::info(ctx.heap(), buffer).unwrap();
    if info.detached {
        return Err(buffer::detached(ctx));
    }
    let rest = info
        .len
        .checked_sub(byte_offset)
        .ok_or_else(|| ctx.throw_range_error("TypedArray offset exceeds buffer length"))?;
    let length = match requested {
        Some(n) if n > rest / size => {
            return Err(ctx.throw_range_error("TypedArray exceeds buffer length"));
        }
        Some(n) => Some(n),
        None if info.max.is_some() => None,
        None if rest % size == 0 => Some(rest / size),
        None => return Err(ctx.throw_range_error("Unaligned TypedArray length")),
    };
    allocate_view(
        ctx,
        proto,
        TypedArrayData {
            buffer,
            byte_offset,
            length,
            kind,
        },
    )
}

fn convert_range(
    ctx: &mut ExecCtx<'_, '_>,
    source: TypedArrayData,
    first: usize,
    dest: TypedArrayData,
    to: usize,
    n: usize,
) -> JsResult<()> {
    for k in 0..n {
        ctx.with_scope(|ctx| {
            if source.kind.is_bigint() {
                let bits = raw(ctx.heap(), source, first + k);
                write_raw(ctx, dest, to + k, bits);
            } else {
                let value = get(ctx, source, first + k)?;
                let bits = elements::number_bits(dest.kind, value.as_number().unwrap());
                write_raw(ctx, dest, to + k, bits);
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn array_like<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<(Handle<'s>, usize)> {
    let source = convert::to_object(ctx, value)?;
    let iterator_key = symbol(ctx, WellKnown::Iterator);
    let method = property::get_property_with_receiver(ctx, source, iterator_key, value)?;
    if method != Value::NULL && method != Value::UNDEFINED {
        let method = ctx.root(method)?;
        require_callable(ctx, method)?;
        let iterator = call::call(ctx, method, value, Args::empty())?;
        let iterator = ctx.root(iterator)?;
        ctx.object_id(iterator)?;
        let next = named(ctx, iterator, "next")?;
        let next = ctx.root(next)?;
        let list = ctx.new_array()?;
        let mut len = 0;
        loop {
            ctx.check_interrupt()?;
            let (iterator, next, list_value) = (ctx.get(iterator), ctx.get(next), ctx.get(list));
            let done = ctx.with_scope(|ctx| {
                let iterator = ctx.root(iterator)?;
                let next = ctx.root(next)?;
                let list = ctx.root(list_value)?;
                let step = call::call(ctx, next, iterator, Args::empty())?;
                let step = ctx.root(step)?;
                ctx.object_id(step)?;
                let done = named(ctx, step, "done")?;
                if convert::to_boolean_value(ctx.heap(), done) {
                    return Ok(true);
                }
                let value = named(ctx, step, "value")?;
                let value = ctx.root(value)?;
                let key = key_at(ctx, len)?;
                define_data(ctx, list, key, value, PropFlags::DEFAULT_DATA)?;
                Ok(false)
            })?;
            if done {
                return Ok((list, len));
            }
            len += 1;
        }
    }
    let len = named(ctx, source, "length")?;
    let len = ctx.root(len)?;
    Ok((source, convert::to_length(ctx, len)? as usize))
}

fn copy_array_like<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    source: Handle<'s>,
    result: Handle<'s>,
    len: usize,
    map: Option<(Handle<'s>, Handle<'s>)>,
) -> JsResult<()> {
    let obj = ctx.object_id(result)?;
    let (view, _) = validate(ctx, obj)?;
    let source = ctx.get(source);
    let map = map.map(|(f, this)| (ctx.get(f), ctx.get(this)));
    for i in 0..len {
        ctx.with_scope(|ctx| {
            let source = ctx.root(source)?;
            let key = key_at(ctx, i)?;
            let val = property::get_property(ctx, source, key)?;
            let mut val = ctx.root(val)?;
            if let Some((f, this)) = map {
                let f = ctx.root(f)?;
                let this = ctx.root(this)?;
                let index = ctx.root(number(i))?;
                let args = ctx.args(&[val, index])?;
                let value = call::call(ctx, f, this, args)?;
                val = ctx.root(value)?;
            }
            let bits = elements::encode(ctx, view.kind, val)?;
            put_element(ctx, view, i, bits);
            Ok(())
        })?;
    }
    Ok(())
}

fn create<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    ctor: Handle<'s>,
    args: &[Handle<'s>],
    min_len: Option<usize>,
) -> JsResult<Handle<'s>> {
    let args = ctx.args(args)?;
    let result = call::construct(ctx, ctor, args, ctor)?;
    let handle = ctx.root(result)?;
    let id = ctx.object_id(handle)?;
    let (view, length) = validate(ctx, id)?;
    if let Some(min_len) = min_len {
        buffer::require_mutable(ctx, view.buffer)?;
        if length < min_len {
            return Err(ctx.throw_type_error("TypedArray result is too short"));
        }
    }
    Ok(handle)
}

fn species_create<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    source: Handle<'s>,
    from: TypedArrayData,
    args: &[Handle<'s>],
    len: Option<usize>,
) -> JsResult<(Handle<'s>, TypedArrayData)> {
    let ctor = if let Some(ctor) = species_constructor(ctx, source)? {
        ctor
    } else {
        let ctor = ctx.realm().typed_array_concrete[from.kind as usize]
            .unwrap()
            .0;
        ctx.root(Value::object(ctor))?
    };
    let result = create(ctx, ctor, args, len)?;
    let view = data(ctx.heap(), ctx.get(result).as_object().unwrap()).unwrap();
    if view.kind.is_bigint() != from.kind.is_bigint() {
        return Err(ctx.throw_type_error("Cannot mix Number and BigInt arrays"));
    }
    Ok((result, view))
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    if op == FROM || op == OF {
        require_constructor(ctx, this)?;
        if op == OF {
            let n = ctx.root(number(args.len()))?;
            let result = create(ctx, this, &[n], Some(args.len()))?;
            let dest = data(ctx.heap(), ctx.get(result).as_object().unwrap()).unwrap();
            for i in 0..args.len() {
                let bits = elements::encode(ctx, dest.kind, args.get(i).unwrap())?;
                put_element(ctx, dest, i, bits);
            }
            return Ok(ctx.get(result));
        }
        let map = item(ctx, args, 1)?;
        let map = if ctx.get(map) != Value::UNDEFINED {
            require_callable(ctx, map)?;
            Some((map, item(ctx, args, 2)?))
        } else {
            None
        };
        let source = item(ctx, args, 0)?;
        let (source, len) = array_like(ctx, source)?;
        let n = ctx.root(number(len))?;
        let result = create(ctx, this, &[n], Some(len))?;
        copy_array_like(ctx, source, result, len, map)?;
        return Ok(ctx.get(result));
    }
    if op == TAG_GET {
        return match ctx
            .get(this)
            .as_object()
            .and_then(|id| data(ctx.heap(), id))
        {
            Some(view) => ctx.new_string(view.kind.name()).map(|s| ctx.get(s)),
            None => Ok(Value::UNDEFINED),
        };
    }
    let obj = ctx.object_id(this)?;
    let view =
        data(ctx.heap(), obj).ok_or_else(|| ctx.throw_type_error("Expected a TypedArray"))?;
    let len = length(ctx.heap(), view);
    if op < 4 {
        return Ok(match op {
            0 => Value::object(view.buffer),
            1 => number(len.unwrap_or(0) * view.kind.size()),
            2 => number(len.map_or(0, |_| view.byte_offset)),
            _ => number(len.unwrap_or(0)),
        });
    }
    let operation = op - METHODS;
    if matches!(
        operation,
        methods::COPY_WITHIN | methods::FILL | methods::REVERSE | methods::SET | methods::SORT
    ) {
        buffer::require_mutable(ctx, view.buffer)?;
    }
    if len.is_none()
        && !matches!(
            operation,
            methods::SET | methods::SORT | methods::TO_SORTED | methods::SUBARRAY
        )
    {
        return Err(oob(ctx));
    }
    methods::dispatch(operation, ctx, this, args, view, len.unwrap_or(0))
}

fn stringify<'s>(ctx: &mut ExecCtx<'_, 's>, h: Handle<'s>) -> JsResult<crate::value::StrId> {
    let s = convert::to_string(ctx, h)?;
    if let ValueKind::String(s) = ctx.get(s).decode() {
        Ok(s)
    } else {
        unreachable!()
    }
}
