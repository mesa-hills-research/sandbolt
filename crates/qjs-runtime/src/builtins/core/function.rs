use crate::atom::Atom;
use crate::builtins::{make_native_fn, set_fn_metadata};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{BoundFunctionData, ObjectData, ObjectFlags};
use crate::ops::property::PropertyDescriptor;
use crate::value::ValueKind;

use super::*;

const EMPTY: u16 = 0x40;
const APPLY: u16 = 0x41;
const BIND: u16 = 0x42;
const CALL: u16 = 0x43;
const TO_STRING: u16 = 0x44;
const HAS_INSTANCE: u16 = 0x45;
const THROW_TYPE_ERROR: u16 = 0x46;

const METHODS: &[(u16, &str, u32)] = &[
    (APPLY, "apply", 2),
    (BIND, "bind", 1),
    (CALL, "call", 1),
    (TO_STRING, "toString", 0),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = ctx.root(Value::object(ctx.realm().function_ctor))?;
    set_fn_metadata(ctx, ctor, "Function", 1)?;
    let proto_id = ctx.realm().function_proto;
    let proto = ctx.root(Value::object(proto_id))?;
    let ObjectData::NativeFunction(data) =
        &mut ctx.runtime.heap.objects.get_mut(proto_id).unwrap().data
    else {
        unreachable!()
    };
    data.function = id(EMPTY);
    set_fn_metadata(ctx, proto, "", 0)?;
    methods(ctx, proto, METHODS)?;
    let key = symbol_key(ctx, WellKnown::HasInstance);
    let has_instance = make_native_fn(ctx, id(HAS_INSTANCE), "[Symbol.hasInstance]", 1, false)?;
    define_data(ctx, proto, key, has_instance, PropFlags::default())?;
    let thrower = make_native_fn(ctx, id(THROW_TYPE_ERROR), "", 0, false)?;
    for key in ["length", "name"] {
        let desc = PropertyDescriptor {
            configurable: Some(false),
            ..PropertyDescriptor::default()
        };
        define_or_throw(ctx, thrower, Atom::get(key), desc)?;
    }
    property::prevent_extensions(ctx, thrower)?;
    for key in ["caller", "arguments"] {
        let desc = PropertyDescriptor {
            get: Some(thrower),
            set: Some(thrower),
            enumerable: Some(false),
            configurable: Some(true),
            ..PropertyDescriptor::default()
        };
        define_or_throw(ctx, proto, Atom::get(key), desc)?;
    }
    define_global(ctx, "Function", ctor)
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match op {
        EMPTY => Ok(Value::UNDEFINED),
        THROW_TYPE_ERROR => {
            Err(ctx.throw_type_error("access to caller or arguments is restricted"))
        }
        APPLY => {
            require_callable(ctx, ctx.get(this))?;
            let receiver = arg(ctx, args, 0)?;
            let list = arg(ctx, args, 1)?;
            let args = if matches!(ctx.get(list), Value::NULL | Value::UNDEFINED) {
                Args::empty()
            } else {
                array_like_arguments(ctx, list)?
            };
            call::call(ctx, this, receiver, args)
        }
        CALL => {
            require_callable(ctx, ctx.get(this))?;
            let receiver = arg(ctx, args, 0)?;
            call::call(ctx, this, receiver, rest(args))
        }
        BIND => bind(ctx, this, args),
        TO_STRING => function_to_string(ctx, this),
        HAS_INSTANCE => {
            let value = arg(ctx, args, 0)?;
            ordinary_has_instance(ctx, this, value)
        }
        _ => Err(ctx.throw_type_error("unknown Function builtin")),
    }
}

fn rest(args: Args<'_>) -> Args<'_> {
    if args.is_empty() {
        Args::empty()
    } else {
        Args::new(args.start as usize + 1, args.len() - 1)
    }
}

pub(super) fn array_like_arguments<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    list: Handle<'s>,
) -> JsResult<Args<'s>> {
    ctx.object_id(list)?;
    let length = get(ctx, list, PropKey::atom(Atom::LENGTH))?;
    let length = convert::to_length(ctx, length)?;
    let count = usize::try_from(length).map_err(|_| ctx.stack_overflow())?;
    ctx.reserve_stack(count)?;
    let list = ctx.get(list);
    let start = ctx.runtime.vm.stack.len();
    for i in 0..length {
        let item = ctx.with_scope(|ctx| {
            let list = ctx.root(list)?;
            let key = index_key(ctx, i)?;
            property::get_property(ctx, list, key)
        })?;
        ctx.root(item)?;
    }
    Ok(Args::new(start, count))
}

fn bind<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let target_id = ctx.object_id(target)?;
    require_callable(ctx, ctx.get(target))?;
    let receiver = arg(ctx, args, 0)?;
    let receiver = ctx.get(receiver);
    let bound_args = rest(args);
    let proto = property::get_prototype_of(ctx, target)?;
    if let Some(proto) = proto {
        ctx.root(Value::object(proto))?;
    }
    let mut flags = ObjectFlags::EXTENSIBLE;
    flags.set(
        ObjectFlags::IS_CONSTRUCTOR,
        ctx.heap()
            .object(target_id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR),
    );
    let bound_id = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let start = bound_args.start as usize;
        let args =
            TrackedVec::from_slice(&heap.ledger, &rt.vm.stack[start..start + bound_args.len()])?;
        let payload = TrackedBox::new(
            &heap.ledger,
            BoundFunctionData {
                target: target_id,
                this_value: receiver,
                args,
            },
        )?;
        heap.insert_object(proto, flags, ObjectData::BoundFunction(payload))
    })?;
    let bound = ctx.root(Value::object(bound_id))?;
    let length_key = PropKey::atom(Atom::LENGTH);
    let mut length = 0.0;
    if property::get_own_property_descriptor(ctx, target, length_key)?.is_some() {
        let value = get(ctx, target, length_key)?;
        if ctx.get(value).is_number() {
            length =
                (convert::to_integer_or_infinity(ctx, value)? - bound_args.len() as f64).max(0.0);
        }
    }
    let name_key = PropKey::atom(Atom::NAME);
    let name = get(ctx, target, name_key)?;
    let name = if matches!(ctx.get(name).decode(), ValueKind::String(_)) {
        let prefix = ctx.new_string("bound ")?;
        ctx.concat_strings(prefix, name)?
    } else {
        ctx.new_string("bound ")?
    };
    let length = ctx.root(Value::number(length))?;
    define_data(ctx, bound, length_key, length, PropFlags::CONFIGURABLE)?;
    define_data(ctx, bound, name_key, name, PropFlags::CONFIGURABLE)?;
    Ok(ctx.get(bound))
}

fn function_to_string<'s>(ctx: &mut ExecCtx<'_, 's>, function: Handle<'s>) -> JsResult<Value> {
    require_callable(ctx, ctx.get(function))?;
    if let Some(source) = ctx.function_source(function)? {
        return Ok(ctx.get(source));
    }
    let name = get(ctx, function, PropKey::atom(Atom::NAME))?;
    let name = if matches!(ctx.get(name).decode(), ValueKind::String(_)) {
        name
    } else {
        ctx.new_string("")?
    };
    concat(ctx, "function ", name, "() { [native code] }")
}

fn ordinary_has_instance<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    function: Handle<'s>,
    object: Handle<'s>,
) -> JsResult<Value> {
    let func_value = ctx.get(function);
    let Some(id) = func_value
        .as_object()
        .filter(|&id| call::is_callable(ctx.heap(), id))
    else {
        return Ok(Value::bool(false));
    };
    if let ObjectData::BoundFunction(bound) = &ctx.heap().object(id).data {
        let target = ctx.root(Value::object(bound.target))?;
        let method = get(ctx, target, symbol_key(ctx, WellKnown::HasInstance))?;
        if !matches!(ctx.get(method), Value::NULL | Value::UNDEFINED) {
            let args = ctx.args(&[object])?;
            let result = call::call(ctx, method, target, args)?;
            return Ok(Value::bool(convert::to_boolean_value(ctx.heap(), result)));
        }
        return ordinary_has_instance(ctx, target, object);
    }
    if ctx.get(object).as_object().is_none() {
        return Ok(Value::bool(false));
    }
    let proto = get(ctx, function, PropKey::atom(Atom::PROTOTYPE))?;
    let proto = ctx.object_id(proto)?;
    Ok(Value::bool(super::object::prototype_chain_contains(
        ctx, object, proto,
    )?))
}

pub(super) fn require_constructor(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<()> {
    if value.as_object().is_some_and(|id| {
        ctx.heap()
            .object(id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
    }) {
        Ok(())
    } else {
        Err(ctx.throw_type_error("expected a constructor"))
    }
}
