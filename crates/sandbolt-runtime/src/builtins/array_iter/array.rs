use super::*;
use crate::builtins::{
    configure_ctor, define_data, install_accessor, install_method, make_native_fn,
};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::ArrayData;
use crate::ops::call::{construct, get_function_realm};
use crate::ops::compare::{same_value_zero, strict_eq};
use crate::ops::convert::{to_integer_or_infinity, to_number, to_object, to_string};
use crate::ops::property::{delete_property, has_property, set_property};
use crate::shape::PropFlags;
use crate::value::{ObjId, ValueKind};

mod from_async;
mod methods;
mod sort;

pub(crate) use methods::{fast_array_pop, fast_array_push_one};

const CTOR: u16 = 0;
const IS_ARRAY: u16 = 1;
const FROM: u16 = 2;
const OF: u16 = 3;
const FROM_ASYNC: u16 = 4;
const SPECIES: u16 = 5;
const ASYNC_CONTINUE: u16 = 6;
const ASYNC_REJECTED: u16 = 7;
const AT: u16 = 16;
const CONCAT: u16 = 17;
const COPY_WITHIN: u16 = 18;
const ENTRIES: u16 = 19;
const EVERY: u16 = 20;
const FILL: u16 = 21;
const FILTER: u16 = 22;
const FIND: u16 = 23;
const FIND_INDEX: u16 = 24;
const FIND_LAST: u16 = 25;
const FIND_LAST_INDEX: u16 = 26;
const FLAT: u16 = 27;
const FLAT_MAP: u16 = 28;
const FOR_EACH: u16 = 29;
const INCLUDES: u16 = 30;
const INDEX_OF: u16 = 31;
const JOIN: u16 = 32;
const KEYS: u16 = 33;
const LAST_INDEX_OF: u16 = 34;
const MAP: u16 = 35;
const POP: u16 = 36;
const PUSH: u16 = 37;
const REDUCE: u16 = 38;
const REDUCE_RIGHT: u16 = 39;
const REVERSE: u16 = 40;
const SHIFT: u16 = 41;
const SLICE: u16 = 42;
const SOME: u16 = 43;
const SORT: u16 = 44;
const SPLICE: u16 = 45;
const TO_LOCALE_STRING: u16 = 46;
const TO_REVERSED: u16 = 47;
const TO_SORTED: u16 = 48;
const TO_SPLICED: u16 = 49;
const TO_STRING: u16 = 50;
const UNSHIFT: u16 = 51;
const VALUES: u16 = 52;
const WITH: u16 = 53;

const METHODS: &[(&str, u16, u32)] = &[
    ("at", AT, 1),
    ("concat", CONCAT, 1),
    ("copyWithin", COPY_WITHIN, 2),
    ("entries", ENTRIES, 0),
    ("every", EVERY, 1),
    ("fill", FILL, 1),
    ("filter", FILTER, 1),
    ("find", FIND, 1),
    ("findIndex", FIND_INDEX, 1),
    ("findLast", FIND_LAST, 1),
    ("findLastIndex", FIND_LAST_INDEX, 1),
    ("flat", FLAT, 0),
    ("flatMap", FLAT_MAP, 1),
    ("forEach", FOR_EACH, 1),
    ("includes", INCLUDES, 1),
    ("indexOf", INDEX_OF, 1),
    ("join", JOIN, 1),
    ("keys", KEYS, 0),
    ("lastIndexOf", LAST_INDEX_OF, 1),
    ("map", MAP, 1),
    ("pop", POP, 0),
    ("push", PUSH, 1),
    ("reduce", REDUCE, 1),
    ("reduceRight", REDUCE_RIGHT, 1),
    ("reverse", REVERSE, 0),
    ("shift", SHIFT, 0),
    ("slice", SLICE, 2),
    ("some", SOME, 1),
    ("sort", SORT, 1),
    ("splice", SPLICE, 2),
    ("toLocaleString", TO_LOCALE_STRING, 0),
    ("toReversed", TO_REVERSED, 0),
    ("toSorted", TO_SORTED, 1),
    ("toSpliced", TO_SPLICED, 2),
    ("toString", TO_STRING, 0),
    ("unshift", UNSHIFT, 1),
    ("with", WITH, 2),
];

const fn id(offset: u16) -> NativeFnId {
    NativeFnId(ARRAY_ITER_BASE + offset)
}

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = configure_ctor(ctx, ctx.realm().array_ctor, id(CTOR), "Array", 1)?;
    let proto = ctx.root(Value::object(ctx.realm().array_proto))?;
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let attrs = PropFlags::WRITABLE | PropFlags::CONFIGURABLE;
    let k = key(ctx, "Array")?;
    define_data(ctx, global, k, ctor, attrs)?;
    for (name, op, length) in [
        ("isArray", IS_ARRAY, 1),
        ("from", FROM, 1),
        ("of", OF, 0),
        ("fromAsync", FROM_ASYNC, 1),
    ] {
        let k = key(ctx, name)?;
        install_method(ctx, ctor, k, id(op), name, length)?;
    }
    install_accessor(
        ctx,
        ctor,
        symbol(ctx, WellKnown::Species),
        Some((id(SPECIES), "get [Symbol.species]")),
        None,
        true,
    )?;
    for &(name, op, length) in METHODS {
        let k = key(ctx, name)?;
        install_method(ctx, proto, k, id(op), name, length)?;
    }
    let values = make_native_fn(ctx, id(VALUES), "values", 0, false)?;
    let k = key(ctx, "values")?;
    define_data(ctx, proto, k, values, attrs)?;
    define_data(ctx, proto, symbol(ctx, WellKnown::Iterator), values, attrs)?;
    let unscopables = ctx.new_object_with_proto(None)?;
    let yes = ctx.root(Value::bool(true))?;
    for name in [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ] {
        let k = key(ctx, name)?;
        define_data(ctx, unscopables, k, yes, PropFlags::DEFAULT_DATA)?;
    }
    define_data(
        ctx,
        proto,
        symbol(ctx, WellKnown::Unscopables),
        unscopables,
        PropFlags::CONFIGURABLE,
    )
}

pub(super) fn dispatch<'s>(
    n: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match n {
        CTOR => return make_array(ctx, args),
        IS_ARRAY => {
            let value = arg(ctx, args, 0)?;
            return Ok(Value::bool(is_array(ctx, ctx.get(value))?));
        }
        FROM => return from(ctx, this, args),
        OF => return of(ctx, this, args),
        FROM_ASYNC => return from_async::start(ctx, this, args),
        ASYNC_CONTINUE | ASYNC_REJECTED => {
            return from_async::resume(ctx, this, args, n == ASYNC_REJECTED);
        }
        SPECIES => return Ok(ctx.get(this)),
        SORT | TO_SORTED => return sort::sort(ctx, this, args, n == TO_SORTED),
        ENTRIES | KEYS | VALUES => {
            let object = to_object(ctx, this)?;
            let kind = match n {
                ENTRIES => ArrayIteratorKind::Entry,
                KEYS => ArrayIteratorKind::Key,
                _ => ArrayIteratorKind::Value,
            };
            let result = new_array_iterator(ctx, object, kind)?;
            return Ok(ctx.get(result));
        }
        _ => {}
    }
    if matches!(n, POP | PUSH)
        && let Some(id) = ctx.get(this).as_object()
    {
        let result = if n == POP {
            fast_array_pop(ctx, id)
        } else if args.len() == 1 {
            fast_array_push_one(ctx, id, ctx.get(args.get(0).unwrap()))?
        } else if ctx.runtime.vm.can_root_without_growing(1) {
            methods::fast_push(ctx, id, args)?
        } else {
            None
        };
        if let Some(result) = result {
            return Ok(result);
        }
    }
    let object = to_object(ctx, this)?;
    if n == TO_STRING {
        let join = named(ctx, object, "join")?;
        if callable(ctx, join) {
            let join = ctx.root(join)?;
            return call(ctx, join, object, Args::empty());
        }
        return object_to_string(ctx, object);
    }
    if n == CONCAT {
        return methods::concat(ctx, object, args);
    }
    let len = get_length(ctx, object)?;
    let receiver = Receiver { object, len };
    match n {
        AT | WITH => methods::at_with(ctx, receiver, args, n == WITH),
        COPY_WITHIN => methods::copy_within(ctx, receiver, args),
        EVERY | FILTER | FIND | FIND_INDEX | FIND_LAST | FIND_LAST_INDEX | FOR_EACH | MAP
        | SOME | REDUCE | REDUCE_RIGHT => methods::callback(ctx, receiver, args, n),
        FILL => methods::fill(ctx, receiver, args),
        FLAT | FLAT_MAP => methods::flat(ctx, receiver, args, n == FLAT_MAP),
        INCLUDES | INDEX_OF | LAST_INDEX_OF => methods::search(ctx, receiver, args, n),
        JOIN | TO_LOCALE_STRING => methods::join(ctx, receiver, args, n == TO_LOCALE_STRING),
        POP | SHIFT => methods::remove_end(ctx, receiver, n == SHIFT),
        PUSH | UNSHIFT => methods::insert_end(ctx, receiver, args, n == UNSHIFT),
        REVERSE | TO_REVERSED => methods::reverse(ctx, receiver, n == TO_REVERSED),
        SLICE => methods::slice(ctx, receiver, args),
        SPLICE | TO_SPLICED => methods::splice(ctx, receiver, args, n == TO_SPLICED),
        _ => Err(ctx.throw_type_error("Unknown array builtin")),
    }
}

fn object_to_string<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<Value> {
    let id = ctx.object_id(object)?;
    let builtin = if is_array(ctx, ctx.get(object))? {
        "Array"
    } else if is_callable(ctx.heap(), id) {
        "Function"
    } else {
        match &ctx.heap().object(id).data {
            ObjectData::StringObject(_) => "String",
            ObjectData::NumberObject(_) => "Number",
            ObjectData::BooleanObject(_) => "Boolean",
            ObjectData::Arguments(_) | ObjectData::MappedArguments(_) => "Arguments",
            ObjectData::Date(_) => "Date",
            ObjectData::RegExp(_) => "RegExp",
            ObjectData::Error(_) => "Error",
            _ => "Object",
        }
    };
    let tag = get_property(ctx, object, symbol(ctx, WellKnown::ToStringTag))?;
    let tag = if matches!(tag.decode(), ValueKind::String(_)) {
        ctx.root(tag)?
    } else {
        ctx.new_string(builtin)?
    };
    let head = ctx.new_string("[object ")?;
    let tail = ctx.new_string("]")?;
    let head = ctx.concat_strings(head, tag)?;
    let text = ctx.concat_strings(head, tail)?;
    Ok(ctx.get(text))
}

pub(super) fn is_array(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<bool> {
    let Some(mut id) = value.as_object() else {
        return Ok(false);
    };
    loop {
        match &ctx.heap().object(id).data {
            ObjectData::Array(_) => return Ok(true),
            ObjectData::Proxy(proxy) => {
                id = proxy
                    .target
                    .ok_or_else(|| ctx.throw_type_error("Proxy is revoked"))?;
                ctx.check_interrupt()?;
            }
            _ => return Ok(false),
        }
    }
}

pub(super) fn prototype_from_constructor(
    ctx: &mut ExecCtx<'_, '_>,
    constructor: Value,
    intrinsic: impl Fn(&crate::realm::Realm) -> ObjId,
) -> JsResult<ObjId> {
    let ctor = ctx.root(constructor)?;
    let proto = get_property(ctx, ctor, PropKey::atom(Atom::PROTOTYPE))?;
    if let Some(proto) = proto.as_object() {
        ctx.root(Value::object(proto))?;
        return Ok(proto);
    }
    let realm = get_function_realm(ctx, constructor.as_object().unwrap())?;
    Ok(intrinsic(ctx.heap().realm(realm)))
}

fn array_create<'s>(ctx: &mut ExecCtx<'_, 's>, len: u64, proto: ObjId) -> JsResult<Handle<'s>> {
    let length = u32::try_from(len).map_err(|_| ctx.throw_range_error("Invalid array length"))?;
    let realm = ctx.realm_id();
    ctx.make_object(
        Some(proto),
        ObjectFlags::EXTENSIBLE | ObjectFlags::FAST_ARRAY | ObjectFlags::IS_EXOTIC,
        |ledger| {
            Ok(ObjectData::Array(TrackedBox::new(
                ledger,
                ArrayData {
                    elements: TrackedVec::new(),
                    length,
                    length_writable: true,
                    realm,
                },
            )?))
        },
    )
}

pub(super) fn array_of_length<'s>(ctx: &mut ExecCtx<'_, 's>, len: u64) -> JsResult<Handle<'s>> {
    array_create(ctx, len, ctx.realm().array_proto)
}

fn make_array<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let proto = match ctx.new_target() {
        Some(target) if target != ctx.realm().array_ctor => {
            prototype_from_constructor(ctx, Value::object(target), |realm| realm.array_proto)?
        }
        _ => ctx.realm().array_proto,
    };
    if args.len() == 1
        && let Some(length) = ctx.get(args.get(0).unwrap()).as_number()
    {
        if length != f64::from(length as u32) {
            return Err(ctx.throw_range_error("Invalid array length"));
        }
        let result = array_create(ctx, length as u64, proto)?;
        return Ok(ctx.get(result));
    }
    let result = array_create(ctx, args.len() as u64, proto)?;
    let result_id = ctx.object_id(result)?;
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let ObjectData::Array(data) = &mut heap.objects.get_mut(result_id).unwrap().data else {
            unreachable!()
        };
        data.elements.try_reserve_exact(&heap.ledger, args.len())?;
        for i in 0..args.len() {
            data.elements
                .push_reserved(runtime.vm.stack[args.get(i).unwrap().index()]);
        }
        Ok(())
    })?;
    Ok(ctx.get(result))
}

fn species_create<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    len: u64,
) -> JsResult<Handle<'s>> {
    if !is_array(ctx, ctx.get(object))? {
        return array_of_length(ctx, len);
    }
    let mut value = named(ctx, object, "constructor")?;
    let c = ctx.root(value)?;
    if constructor(ctx, value) {
        let realm = get_function_realm(ctx, value.as_object().unwrap())?;
        if realm != ctx.realm_id() && value == Value::object(ctx.heap().realm(realm).array_ctor) {
            value = Value::UNDEFINED;
        }
    }
    if value.as_object().is_some() {
        value = get_property(ctx, c, symbol(ctx, WellKnown::Species))?;
        if value == Value::NULL {
            value = Value::UNDEFINED;
        }
    }
    if value == Value::UNDEFINED {
        return array_of_length(ctx, len);
    }
    if !constructor(ctx, value) {
        return Err(ctx.throw_type_error("Array species is not a constructor"));
    }
    let c = ctx.root(value)?;
    construct_result(ctx, c, Some(len))
}

fn construct_result<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    c: Handle<'s>,
    len: Option<u64>,
) -> JsResult<Handle<'s>> {
    if !constructor(ctx, ctx.get(c)) {
        return array_of_length(ctx, len.unwrap_or(0));
    }
    let args = if let Some(len) = len {
        let len = ctx.root(Value::number(len as f64))?;
        ctx.args(&[len])?
    } else {
        Args::empty()
    };
    let value = construct(ctx, c, args, c)?;
    ctx.root(value)
}

#[inline]
fn index_value(i: u64) -> Value {
    if i <= i32::MAX as u64 {
        Value::int32(i as i32)
    } else {
        Value::number(i as f64)
    }
}

/// Returns `(src_shape, proto_and_shape)` when `src_id` is an ordinary object
/// with an immutable shape carrying no array-index keys, and its prototype
/// chain (at most one ordinary object, e.g. `Object.prototype`) also has an
/// immutable shape with no array-index keys. Any read of an index below
/// `INLINE_INDEX_LIMIT` on such an object is `undefined` as long as those
/// shapes match.
fn index_free_source_shapes(
    heap: &crate::heap::Heap,
    src_id: ObjId,
) -> Option<(
    crate::value::ShapeId,
    Option<(ObjId, crate::value::ShapeId)>,
)> {
    let src_obj = heap.object(src_id);
    if !matches!(src_obj.data, ObjectData::Ordinary)
        || src_obj.flags.contains(ObjectFlags::IS_EXOTIC)
    {
        return None;
    }
    let src_shape_id = src_obj.shape;
    let src_shape = heap.shape(src_shape_id);
    if src_shape.is_mutable() || src_shape.has_index_key() {
        return None;
    }
    let Some(proto_id) = src_shape.proto else {
        return Some((src_shape_id, None));
    };
    let proto_obj = heap.object(proto_id);
    if !matches!(proto_obj.data, ObjectData::Ordinary)
        || proto_obj.flags.contains(ObjectFlags::IS_EXOTIC)
    {
        return None;
    }
    let proto_shape_id = proto_obj.shape;
    let proto_shape = heap.shape(proto_shape_id);
    if proto_shape.is_mutable() || proto_shape.has_index_key() || proto_shape.proto.is_some() {
        return None;
    }
    Some((src_shape_id, Some((proto_id, proto_shape_id))))
}

#[inline]
fn fast_push_output_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    output_id: ObjId,
    i: u64,
    value: Value,
    val_root: &crate::root::MutRoot<'s>,
) -> JsResult<bool> {
    if i >= u64::from(crate::atom::INLINE_INDEX_LIMIT) {
        return Ok(false);
    }
    let idx = i as u32;
    let can_fast = {
        let obj = ctx.heap().object(output_id);
        obj.flags
            .contains(ObjectFlags::FAST_ARRAY | ObjectFlags::EXTENSIBLE)
            && match &obj.data {
                ObjectData::Array(array) => {
                    array.elements.len() == idx as usize
                        && (idx < array.length || array.length_writable)
                }
                _ => false,
            }
    };
    if !can_fast {
        return Ok(false);
    }
    let pushed = {
        let ObjectData::Array(array) =
            &mut ctx.runtime.heap.objects.get_mut(output_id).unwrap().data
        else {
            unreachable!()
        };
        if array.elements.len() < array.elements.capacity() {
            array.elements.push_reserved(value);
            array.length = array.length.max(idx + 1);
            true
        } else {
            false
        }
    };
    if !pushed {
        ctx.set(val_root, value);
        ctx.allocate(|runtime| {
            let ObjectData::Array(array) =
                &mut runtime.heap.objects.get_mut(output_id).unwrap().data
            else {
                unreachable!()
            };
            array.elements.push(&runtime.heap.ledger, value)?;
            array.length = array.length.max(idx + 1);
            Ok(())
        })?;
        ctx.set(val_root, Value::UNDEFINED);
    }
    Ok(true)
}

fn from<'s>(ctx: &mut ExecCtx<'_, 's>, c: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let source = arg(ctx, args, 0)?;
    let mapper_handle = arg(ctx, args, 1)?;
    let this_handle = arg(ctx, args, 2)?;
    let mapper = ctx.get(mapper_handle);
    if mapper != Value::UNDEFINED && !callable(ctx, mapper) {
        return Err(ctx.throw_type_error("Array.from mapper is not callable"));
    }
    let source_object = to_object(ctx, source)?;
    let method = get_method_v(ctx, source_object, source, symbol(ctx, WellKnown::Iterator))?;
    let this_arg = ctx.get(this_handle);
    if let Some(method) = method {
        let result = construct_result(ctx, c, None)?;
        let output = ctx.get(result);
        let record = IterRecord::from_method(ctx, source, method)?;
        for i in 0..=MAX_LENGTH {
            if i == MAX_LENGTH {
                let err = Err(ctx.throw_type_error("Array exceeds maximum length"));
                return on_error(ctx, err, |ctx| IterRecord::close(ctx, record.object));
            }
            if (i & 1023) == 0 {
                ctx.check_interrupt()?;
            }
            let Some(value) = IterRecord::step(ctx, record)? else {
                set_len(ctx, result, i)?;
                return Ok(output);
            };
            let status = ctx.with_scope(|ctx| {
                let value = ctx.root(value)?;
                from_item(ctx, output, mapper, this_arg, i, value)
            });
            on_error(ctx, status, |ctx| IterRecord::close(ctx, record.object))?;
        }
        unreachable!();
    }
    let len = get_length(ctx, source_object)?;
    let result = construct_result(ctx, c, Some(len))?;
    let output = ctx.get(result);
    let output_id = output.as_object();
    let src_id = ctx.get(source_object).as_object();
    let has_mapper = mapper != Value::UNDEFINED;
    let arg0 = ctx.mut_root(Value::UNDEFINED)?;
    let arg1 = ctx.mut_root(Value::UNDEFINED)?;
    debug_assert_eq!(arg1.handle().index(), arg0.handle().index() + 1);
    let call_args = Args::new(arg0.handle().index(), 2);
    let out_val = ctx.mut_root(Value::UNDEFINED)?;
    let mut gc_epoch = ctx.heap().collection_count();
    let mut index_free = src_id.and_then(|id| index_free_source_shapes(ctx.heap(), id));
    for i in 0..len {
        if (i & 1023) == 0 {
            ctx.check_interrupt()?;
        }
        let item = if i < u64::from(crate::atom::INLINE_INDEX_LIMIT)
            && let Some(id) = src_id
            && let Some((src_shape, proto_check)) = index_free
        {
            let heap = ctx.heap();
            let valid = if heap.collection_count() == gc_epoch {
                heap.object(id).shape == src_shape
                    && proto_check.is_none_or(|(proto_id, proto_shape)| {
                        heap.object(proto_id).shape == proto_shape
                    })
            } else {
                gc_epoch = heap.collection_count();
                index_free = index_free_source_shapes(heap, id);
                index_free.is_some()
            };
            if valid {
                Value::UNDEFINED
            } else {
                index_free = None;
                let source = ctx.get(source_object);
                ctx.with_scope(|ctx| {
                    let source = ctx.root(source)?;
                    get_index(ctx, source, i)
                })?
            }
        } else {
            let source = ctx.get(source_object);
            ctx.with_scope(|ctx| {
                let source = ctx.root(source)?;
                get_index(ctx, source, i)
            })?
        };
        let mapped = if has_mapper {
            ctx.set(&arg0, item);
            ctx.set(&arg1, index_value(i));
            let v = call(ctx, mapper_handle, this_handle, call_args)?;
            ctx.set(&arg0, Value::UNDEFINED);
            v
        } else {
            item
        };
        if let Some(out_id) = output_id
            && fast_push_output_index(ctx, out_id, i, mapped, &out_val)?
        {
            continue;
        }
        ctx.set(&out_val, mapped);
        let status = ctx.with_scope(|ctx| {
            let result = ctx.root(output)?;
            create_index(ctx, result, i, mapped)
        });
        ctx.set(&out_val, Value::UNDEFINED);
        status?;
    }
    set_len(ctx, result, len)?;
    Ok(output)
}

pub(super) fn get_method_v<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    receiver: Handle<'s>,
    key: PropKey,
) -> JsResult<Option<Handle<'s>>> {
    let method = crate::ops::property::get_property_with_receiver(ctx, object, key, receiver)?;
    if matches!(method, Value::NULL | Value::UNDEFINED) {
        Ok(None)
    } else if callable(ctx, method) {
        Ok(Some(ctx.root(method)?))
    } else {
        Err(ctx.throw_type_error("Property is not a method"))
    }
}

fn from_item<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    output: Value,
    mapper: Value,
    this: Value,
    i: u64,
    value: Handle<'s>,
) -> JsResult<()> {
    let output = ctx.root(output)?;
    let value = if mapper != Value::UNDEFINED {
        let mapper = ctx.root(mapper)?;
        let this = ctx.root(this)?;
        let index = ctx.root(index_value(i))?;
        let args = ctx.args(&[value, index])?;
        call(ctx, mapper, this, args)?
    } else {
        ctx.get(value)
    };
    create_index(ctx, output, i, value)
}

fn of<'s>(ctx: &mut ExecCtx<'_, 's>, c: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let len = args.len() as u64;
    let out = construct_result(ctx, c, Some(len))?;
    let result = ctx.get(out);
    for i in 0..args.len() {
        let value = ctx.get(args.get(i).unwrap());
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let out = ctx.root(result)?;
            create_index(ctx, out, i as u64, value)
        })?;
    }
    set_len(ctx, out, len)?;
    Ok(result)
}

fn set_len<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, len: u64) -> JsResult<()> {
    let len = ctx.root(Value::number(len as f64))?;
    set(ctx, object, LENGTH, len)
}

fn set<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
) -> JsResult<()> {
    if !set_property(ctx, object, key, value)? {
        return Err(ctx.throw_type_error("Cannot set property"));
    }
    Ok(())
}

fn set_index<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    i: u64,
    value: Value,
) -> JsResult<()> {
    let value = ctx.root(value)?;
    let key = index(ctx, i)?;
    set(ctx, object, key, value)
}

fn delete_index<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, i: u64) -> JsResult<()> {
    let key = index(ctx, i)?;
    if !delete_property(ctx, object, key)? {
        return Err(ctx.throw_type_error("Cannot delete property"));
    }
    Ok(())
}

fn present<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, i: u64) -> JsResult<Option<Value>> {
    let key = index(ctx, i)?;
    if has_property(ctx, object, key)? {
        get_property(ctx, object, key).map(Some)
    } else {
        Ok(None)
    }
}

fn relative(index: f64, len: u64) -> u64 {
    if index < 0.0 {
        (len as f64 + index).max(0.0) as u64
    } else {
        index.min(len as f64) as u64
    }
}

fn bound<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    arg: Option<Handle<'s>>,
    len: u64,
    default: u64,
) -> JsResult<u64> {
    match arg {
        Some(arg) if ctx.get(arg) != Value::UNDEFINED => {
            Ok(relative(to_integer_or_infinity(ctx, arg)?, len))
        }
        _ => Ok(default),
    }
}

#[derive(Clone, Copy)]
struct Receiver<'s> {
    object: Handle<'s>,
    len: u64,
}

impl<'s> Receiver<'s> {
    fn at<T>(
        self,
        ctx: &mut ExecCtx<'_, 's>,
        f: impl for<'a> FnOnce(&mut ExecCtx<'_, 'a>, Handle<'a>) -> JsResult<T>,
    ) -> JsResult<T> {
        let object = ctx.get(self.object);
        ctx.with_scope(|ctx| {
            ctx.check_interrupt()?;
            let object = ctx.root(object)?;
            f(ctx, object)
        })
    }
}
