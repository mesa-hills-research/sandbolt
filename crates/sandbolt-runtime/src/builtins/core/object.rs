use crate::atom::Atom;
use crate::builtins::{configure_ctor, install_accessor};
use crate::ledger::TrackedVec;
use crate::object::{ObjectData, ObjectFlags};
use crate::ops::compare::same_value;
use crate::ops::property::PropertyDescriptor;
use crate::value::{ObjId, ValueKind};

use super::*;

const CTOR: u16 = 0;
const ASSIGN: u16 = 1;
const CREATE: u16 = 2;
const DEFINE: u16 = 3;
const DEFINES: u16 = 4;
const ENTRIES: u16 = 5;
const FREEZE: u16 = 6;
const FROM_ENTRIES: u16 = 7;
const GET_DESC: u16 = 8;
const GET_DESCS: u16 = 9;
const GET_NAMES: u16 = 10;
const GET_SYMBOLS: u16 = 11;
const GET_PROTO: u16 = 12;
const GROUP_BY: u16 = 13;
const HAS_OWN: u16 = 14;
const IS: u16 = 15;
const IS_EXTENSIBLE: u16 = 16;
const IS_FROZEN: u16 = 17;
const IS_SEALED: u16 = 18;
const KEYS: u16 = 19;
const PREVENT_EXTENSIONS: u16 = 20;
const SEAL: u16 = 21;
const SET_PROTO: u16 = 22;
const VALUES: u16 = 23;
const HAS_OWN_PROPERTY: u16 = 24;
const IS_PROTO_OF: u16 = 25;
const IS_ENUMERABLE: u16 = 26;
const TO_LOCALE_STRING: u16 = 27;
const TO_STRING: u16 = 28;
const VALUE_OF: u16 = 29;
const PROTO_GET: u16 = 30;
const PROTO_SET: u16 = 31;
const DEFINE_GETTER: u16 = 32;
const DEFINE_SETTER: u16 = 33;
const LOOKUP_GETTER: u16 = 34;
const LOOKUP_SETTER: u16 = 35;

const STATIC: &[(u16, &str, u32)] = &[
    (ASSIGN, "assign", 2),
    (CREATE, "create", 2),
    (DEFINE, "defineProperty", 3),
    (DEFINES, "defineProperties", 2),
    (ENTRIES, "entries", 1),
    (FREEZE, "freeze", 1),
    (FROM_ENTRIES, "fromEntries", 1),
    (GET_DESC, "getOwnPropertyDescriptor", 2),
    (GET_DESCS, "getOwnPropertyDescriptors", 1),
    (GET_NAMES, "getOwnPropertyNames", 1),
    (GET_SYMBOLS, "getOwnPropertySymbols", 1),
    (GET_PROTO, "getPrototypeOf", 1),
    (GROUP_BY, "groupBy", 2),
    (HAS_OWN, "hasOwn", 2),
    (IS, "is", 2),
    (IS_EXTENSIBLE, "isExtensible", 1),
    (IS_FROZEN, "isFrozen", 1),
    (IS_SEALED, "isSealed", 1),
    (KEYS, "keys", 1),
    (PREVENT_EXTENSIONS, "preventExtensions", 1),
    (SEAL, "seal", 1),
    (SET_PROTO, "setPrototypeOf", 2),
    (VALUES, "values", 1),
];
const PROTOTYPE: &[(u16, &str, u32)] = &[
    (HAS_OWN_PROPERTY, "hasOwnProperty", 1),
    (IS_PROTO_OF, "isPrototypeOf", 1),
    (IS_ENUMERABLE, "propertyIsEnumerable", 1),
    (TO_LOCALE_STRING, "toLocaleString", 0),
    (TO_STRING, "toString", 0),
    (VALUE_OF, "valueOf", 0),
    (DEFINE_GETTER, "__defineGetter__", 2),
    (DEFINE_SETTER, "__defineSetter__", 2),
    (LOOKUP_GETTER, "__lookupGetter__", 1),
    (LOOKUP_SETTER, "__lookupSetter__", 1),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor = configure_ctor(ctx, ctx.realm().object_ctor, id(CTOR), "Object", 1)?;
    let proto = ctx.root(Value::object(ctx.realm().object_proto))?;
    methods(ctx, ctor, STATIC)?;
    methods(ctx, proto, PROTOTYPE)?;
    install_accessor(
        ctx,
        proto,
        Atom::get("__proto__"),
        Some((id(PROTO_GET), "get __proto__")),
        Some((id(PROTO_SET), "set __proto__")),
        true,
    )?;
    define_global(ctx, "Object", ctor)
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let a = arg(ctx, args, 0)?;
    match op {
        CTOR => {
            let obj = if let Some(target) = ctx.new_target()
                && target != ctx.realm().object_ctor
            {
                ctx.ordinary_create_from_constructor(
                    target,
                    |realm| realm.object_proto,
                    ObjectFlags::EXTENSIBLE,
                    |_| Ok(ObjectData::Ordinary),
                )?
            } else if matches!(ctx.get(a), Value::UNDEFINED | Value::NULL) {
                ctx.new_object()?
            } else {
                convert::to_object(ctx, a)?
            };
            Ok(ctx.get(obj))
        }
        ASSIGN => assign(ctx, a, args),
        CREATE => {
            let proto = prototype_arg(ctx, a)?;
            let object = ctx.new_object_with_proto(proto)?;
            let descriptors = arg(ctx, args, 1)?;
            if ctx.get(descriptors) != Value::UNDEFINED {
                define_properties(ctx, object, descriptors)?;
            }
            Ok(ctx.get(object))
        }
        DEFINE => {
            ctx.object_id(a)?;
            let b = arg(ctx, args, 1)?;
            let key = convert::to_property_key(ctx, b)?;
            let c = arg(ctx, args, 2)?;
            let desc = to_descriptor(ctx, c)?;
            define_or_throw(ctx, a, key, desc)?;
            Ok(ctx.get(a))
        }
        DEFINES => {
            ctx.object_id(a)?;
            let b = arg(ctx, args, 1)?;
            define_properties(ctx, a, b)?;
            Ok(ctx.get(a))
        }
        ENTRIES | GET_DESCS | GET_NAMES | GET_SYMBOLS | KEYS | VALUES => {
            collect_properties(ctx, a, op)
        }
        FREEZE | SEAL | PREVENT_EXTENSIONS => set_integrity(ctx, a, op),
        IS_FROZEN | IS_SEALED => test_integrity(ctx, a, op),
        FROM_ENTRIES => from_entries(ctx, a),
        GET_DESC | HAS_OWN => {
            let obj = convert::to_object(ctx, a)?;
            let b = arg(ctx, args, 1)?;
            let key = convert::to_property_key(ctx, b)?;
            let desc = property::get_own_property_descriptor(ctx, obj, key)?;
            if op == HAS_OWN {
                Ok(Value::bool(desc.is_some()))
            } else {
                from_descriptor(ctx, desc)
            }
        }
        GET_PROTO | PROTO_GET => {
            let obj = convert::to_object(ctx, if op == PROTO_GET { this } else { a })?;
            Ok(prototype_value(property::get_prototype_of(ctx, obj)?))
        }
        GROUP_BY => {
            let b = arg(ctx, args, 1)?;
            group_by(ctx, a, b)
        }
        IS => {
            let b = arg(ctx, args, 1)?;
            Ok(Value::bool(same_value(ctx, a, b)))
        }
        IS_EXTENSIBLE => Ok(Value::bool(
            ctx.get(a).as_object().is_some() && property::is_extensible(ctx, a)?,
        )),
        SET_PROTO => {
            require_coercible(ctx, ctx.get(a))?;
            let b = arg(ctx, args, 1)?;
            let proto = prototype_arg(ctx, b)?;
            if ctx.get(a).as_object().is_some() {
                set_prototype_or_throw(ctx, a, proto)?;
            }
            Ok(ctx.get(a))
        }
        HAS_OWN_PROPERTY | IS_ENUMERABLE => {
            let key = convert::to_property_key(ctx, a)?;
            let obj = convert::to_object(ctx, this)?;
            let desc = property::get_own_property_descriptor(ctx, obj, key)?;
            Ok(Value::bool(desc.is_some_and(|desc| {
                op == HAS_OWN_PROPERTY || desc.enumerable == Some(true)
            })))
        }
        IS_PROTO_OF => {
            if ctx.get(a).as_object().is_none() {
                return Ok(Value::bool(false));
            }
            let obj = convert::to_object(ctx, this)?;
            let id = ctx.object_id(obj)?;
            Ok(Value::bool(prototype_chain_contains(ctx, a, id)?))
        }
        TO_LOCALE_STRING => {
            let obj = convert::to_object(ctx, this)?;
            let method = property::get_property_with_receiver(
                ctx,
                obj,
                PropKey::atom(Atom::TO_STRING),
                this,
            )?;
            let method = ctx.root(method)?;
            call::call(ctx, method, this, Args::empty())
        }
        TO_STRING => object_to_string(ctx, this),
        VALUE_OF => {
            let obj = convert::to_object(ctx, this)?;
            Ok(ctx.get(obj))
        }
        PROTO_SET => {
            require_coercible(ctx, ctx.get(this))?;
            if ctx.get(this).as_object().is_some()
                && (ctx.get(a).as_object().is_some() || ctx.get(a) == Value::NULL)
            {
                let proto = prototype_arg(ctx, a)?;
                set_prototype_or_throw(ctx, this, proto)?;
            }
            Ok(Value::UNDEFINED)
        }
        DEFINE_GETTER | DEFINE_SETTER => {
            let obj = convert::to_object(ctx, this)?;
            let b = arg(ctx, args, 1)?;
            require_callable(ctx, ctx.get(b))?;
            let key = convert::to_property_key(ctx, a)?;
            let desc = PropertyDescriptor {
                get: (op == DEFINE_GETTER).then_some(b),
                set: (op == DEFINE_SETTER).then_some(b),
                enumerable: Some(true),
                configurable: Some(true),
                ..PropertyDescriptor::default()
            };
            define_or_throw(ctx, obj, key, desc)?;
            Ok(Value::UNDEFINED)
        }
        LOOKUP_GETTER | LOOKUP_SETTER => {
            let obj = convert::to_object(ctx, this)?;
            let key = convert::to_property_key(ctx, a)?;
            lookup_accessor(ctx, obj, key, op == LOOKUP_GETTER)
        }
        _ => Err(ctx.throw_type_error("unknown Object builtin")),
    }
}

pub(super) fn prototype_value(proto: Option<ObjId>) -> Value {
    proto.map_or(Value::NULL, Value::object)
}

fn set_prototype_or_throw<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    proto: Option<Handle<'s>>,
) -> JsResult<()> {
    if !property::set_prototype_of(ctx, object, proto)? {
        return Err(ctx.throw_type_error("cannot set prototype"));
    }
    Ok(())
}

pub(super) fn prototype_chain_contains<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    proto: ObjId,
) -> JsResult<bool> {
    let cursor = ctx.mut_root(ctx.get(object))?;
    loop {
        ctx.check_interrupt()?;
        let Some(parent) = property::get_prototype_of(ctx, cursor.handle())? else {
            return Ok(false);
        };
        if parent == proto {
            return Ok(true);
        }
        ctx.set(&cursor, Value::object(parent));
    }
}

fn lookup_accessor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    getter: bool,
) -> JsResult<Value> {
    let cursor = ctx.mut_root(ctx.get(object))?;
    loop {
        ctx.check_interrupt()?;
        if let Some(desc) = property::get_own_property_descriptor(ctx, cursor.handle(), key)? {
            return Ok(if getter { desc.get } else { desc.set }.unwrap_or(Value::UNDEFINED));
        }
        let Some(parent) = property::get_prototype_of(ctx, cursor.handle())? else {
            return Ok(Value::UNDEFINED);
        };
        ctx.set(&cursor, Value::object(parent));
    }
}

/// Apply the intrinsic `Object.prototype.toString` without looking up the method.
pub fn object_to_string<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    match ctx.get(this) {
        Value::UNDEFINED => return ctx.new_string("[object Undefined]").map(|s| ctx.get(s)),
        Value::NULL => return ctx.new_string("[object Null]").map(|s| ctx.get(s)),
        _ => {}
    }
    let obj = convert::to_object(ctx, this)?;
    let id = ctx.object_id(obj)?;
    let mut array_target = id;
    let is_array = loop {
        match &ctx.heap().object(array_target).data {
            ObjectData::Array(_) => break true,
            ObjectData::Proxy(p) => {
                array_target = p
                    .target
                    .ok_or_else(|| ctx.throw_type_error("proxy is revoked"))?
            }
            _ => break false,
        }
    };
    let builtin = if is_array {
        "Array"
    } else if call::is_callable(ctx.heap(), id) {
        "Function"
    } else {
        match &ctx.heap().object(id).data {
            ObjectData::StringObject(_) => "String",
            ObjectData::NumberObject(_) => "Number",
            ObjectData::BooleanObject(_) => "Boolean",
            ObjectData::Error(_) => "Error",
            ObjectData::RegExp(_) => "RegExp",
            ObjectData::Date(_) => "Date",
            ObjectData::Arguments(_) | ObjectData::MappedArguments(_) => "Arguments",
            _ => "Object",
        }
    };
    let tag = get(ctx, obj, symbol_key(ctx, WellKnown::ToStringTag))?;
    let tag = if matches!(ctx.get(tag).decode(), ValueKind::String(_)) {
        tag
    } else {
        ctx.new_string(builtin)?
    };
    concat(ctx, "[object ", tag, "]")
}

fn assign<'s>(ctx: &mut ExecCtx<'_, 's>, target: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let target = convert::to_object(ctx, target)?;
    let target_value = ctx.get(target);
    for i in 1..args.len() {
        let source = ctx.get(args.get(i).unwrap());
        if matches!(source, Value::UNDEFINED | Value::NULL) {
            continue;
        }
        ctx.with_scope(|ctx| {
            let source = ctx.root(source)?;
            let source = convert::to_object(ctx, source)?;
            let keys = property::own_property_keys(ctx, source)?;
            root_keys(ctx, &keys)?;
            let source = ctx.get(source);
            for &key in keys.iter() {
                ctx.with_scope(|ctx| {
                    let source = ctx.root(source)?;
                    let target = ctx.root(target_value)?;
                    if is_enumerable(ctx, source, key)? {
                        let value = get(ctx, source, key)?;
                        set_or_throw(ctx, target, key, value)?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    Ok(target_value)
}

fn is_enumerable<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    source: Handle<'s>,
    key: PropKey,
) -> JsResult<bool> {
    Ok(property::get_own_property_descriptor(ctx, source, key)?
        .is_some_and(|desc| desc.enumerable == Some(true)))
}

fn read_if_present<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
) -> JsResult<Option<Handle<'s>>> {
    let key = Atom::get(name);
    if property::has_property(ctx, object, key)? {
        get(ctx, object, key).map(Some)
    } else {
        Ok(None)
    }
}

pub(super) fn to_descriptor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
) -> JsResult<PropertyDescriptor<Handle<'s>>> {
    ctx.object_id(object)?;
    let enumerable =
        read_if_present(ctx, object, "enumerable")?.map(|v| convert::to_boolean(ctx, v));
    let configurable =
        read_if_present(ctx, object, "configurable")?.map(|v| convert::to_boolean(ctx, v));
    let value = read_if_present(ctx, object, "value")?;
    let writable = read_if_present(ctx, object, "writable")?.map(|v| convert::to_boolean(ctx, v));
    let get = read_if_present(ctx, object, "get")?;
    if let Some(get) = get
        && ctx.get(get) != Value::UNDEFINED
    {
        require_callable(ctx, ctx.get(get))?;
    }
    let set = read_if_present(ctx, object, "set")?;
    if let Some(set) = set
        && ctx.get(set) != Value::UNDEFINED
    {
        require_callable(ctx, ctx.get(set))?;
    }
    let desc = PropertyDescriptor {
        enumerable,
        configurable,
        value,
        writable,
        get,
        set,
    };
    if desc.is_accessor() && desc.is_data() {
        return Err(ctx.throw_type_error("property descriptor mixes accessors and data"));
    }
    Ok(desc)
}

pub(super) fn from_descriptor(
    ctx: &mut ExecCtx<'_, '_>,
    desc: Option<PropertyDescriptor>,
) -> JsResult<Value> {
    let Some(desc) = desc else {
        return Ok(Value::UNDEFINED);
    };
    let fields = [
        ("value", desc.value),
        ("writable", desc.writable.map(Value::bool)),
        ("get", desc.get),
        ("set", desc.set),
        ("enumerable", desc.enumerable.map(Value::bool)),
        ("configurable", desc.configurable.map(Value::bool)),
    ];
    let mut roots = [None; 6];
    for ((_, value), root) in fields.into_iter().zip(&mut roots) {
        *root = value.map(|v| ctx.root(v)).transpose()?;
    }
    let object = ctx.new_object()?;
    for ((name, _), root) in fields.into_iter().zip(roots) {
        if let Some(value) = root {
            put(ctx, object, Atom::get(name), value)?;
        }
    }
    Ok(ctx.get(object))
}

fn define_properties<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    properties: Handle<'s>,
) -> JsResult<()> {
    let props = convert::to_object(ctx, properties)?;
    let keys = property::own_property_keys(ctx, props)?;
    root_keys(ctx, &keys)?;
    let mut pending = TrackedVec::new();
    for &key in keys.iter() {
        if is_enumerable(ctx, props, key)? {
            let desc = get(ctx, props, key)?;
            let desc = to_descriptor(ctx, desc)?;
            ctx.allocate(|rt| pending.push(&rt.heap.ledger, (key, desc)))?;
        }
    }
    for &(key, desc) in pending.iter() {
        define_or_throw(ctx, object, key, desc)?;
    }
    Ok(())
}

fn collect_properties<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    input: Handle<'s>,
    op: u16,
) -> JsResult<Value> {
    let source = convert::to_object(ctx, input)?;
    let keys = property::own_property_keys(ctx, source)?;
    root_keys(ctx, &keys)?;
    let out = if op == GET_DESCS {
        ctx.new_object()?
    } else {
        ctx.new_array()?
    };
    let (source, out) = (ctx.get(source), ctx.get(out));
    let mut index = 0;
    for &key in keys.iter() {
        let symbol = matches!(key.decode(), KeyKind::Symbol(_));
        if op != GET_DESCS && symbol != (op == GET_SYMBOLS) {
            continue;
        }
        ctx.with_scope(|ctx| {
            let out = ctx.root(out)?;
            let source = ctx.root(source)?;
            let value = match op {
                GET_DESCS => {
                    if let Some(desc) = property::get_own_property_descriptor(ctx, source, key)? {
                        let desc = from_descriptor(ctx, Some(desc))?;
                        let desc = ctx.root(desc)?;
                        put(ctx, out, key, desc)?;
                    }
                    return Ok(());
                }
                GET_NAMES | GET_SYMBOLS => convert::key_value(ctx, key)?,
                _ => {
                    if !is_enumerable(ctx, source, key)? {
                        return Ok(());
                    }
                    match op {
                        KEYS => convert::key_value(ctx, key)?,
                        VALUES => get(ctx, source, key)?,
                        ENTRIES => {
                            let value = get(ctx, source, key)?;
                            let key = convert::key_value(ctx, key)?;
                            let pair = ctx.new_array()?;
                            put(ctx, pair, ZERO, key)?;
                            put(ctx, pair, ONE, value)?;
                            pair
                        }
                        _ => unreachable!(),
                    }
                }
            };
            push_array(ctx, out, index, value)?;
            index += 1;
            Ok(())
        })?;
    }
    Ok(out)
}

fn set_integrity<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, op: u16) -> JsResult<Value> {
    let value = ctx.get(object);
    if value.as_object().is_none() {
        return Ok(value);
    }
    if !property::prevent_extensions(ctx, object)? {
        return Err(ctx.throw_type_error("cannot prevent extensions"));
    }
    if op == PREVENT_EXTENSIONS {
        return Ok(value);
    }
    let keys = property::own_property_keys(ctx, object)?;
    root_keys(ctx, &keys)?;
    for &key in keys.iter() {
        ctx.with_scope(|ctx| {
            let object = ctx.root(value)?;
            let mut desc = PropertyDescriptor {
                configurable: Some(false),
                ..PropertyDescriptor::default()
            };
            if op == FREEZE {
                let Some(current) = property::get_own_property_descriptor(ctx, object, key)? else {
                    return Ok(());
                };
                if current.is_data() {
                    desc.writable = Some(false);
                }
            }
            define_or_throw(ctx, object, key, desc)
        })?;
    }
    Ok(value)
}

fn test_integrity<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, op: u16) -> JsResult<Value> {
    let value = ctx.get(object);
    if value.as_object().is_none() {
        return Ok(Value::bool(true));
    }
    if property::is_extensible(ctx, object)? {
        return Ok(Value::bool(false));
    }
    let keys = property::own_property_keys(ctx, object)?;
    root_keys(ctx, &keys)?;
    for &key in keys.iter() {
        let desc = ctx.with_scope(|ctx| {
            let object = ctx.root(value)?;
            property::get_own_property_descriptor(ctx, object, key)
        })?;
        if desc.is_some_and(|desc| {
            desc.configurable == Some(true) || (op == IS_FROZEN && desc.writable == Some(true))
        }) {
            return Ok(Value::bool(false));
        }
    }
    Ok(Value::bool(true))
}

fn from_entries<'s>(ctx: &mut ExecCtx<'_, 's>, iterable: Handle<'s>) -> JsResult<Value> {
    require_coercible(ctx, ctx.get(iterable))?;
    let out = ctx.new_object()?;
    let out = ctx.get(out);
    for_each_iterator(ctx, iterable, |ctx, item, _| {
        ctx.object_id(item)?;
        let key = get(ctx, item, ZERO)?;
        let value = get(ctx, item, ONE)?;
        let key = convert::to_property_key(ctx, key)?;
        let out = ctx.root(out)?;
        put(ctx, out, key, value)
    })?;
    Ok(out)
}

fn group_by<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    iterable: Handle<'s>,
    callback: Handle<'s>,
) -> JsResult<Value> {
    require_coercible(ctx, ctx.get(iterable))?;
    let callback = ctx.get(callback);
    require_callable(ctx, callback)?;
    let out = ctx.new_object_with_proto(None)?;
    let out = ctx.get(out);
    for_each_iterator(ctx, iterable, |ctx, item, index| {
        let callback = ctx.root(callback)?;
        let undef = ctx.root(Value::UNDEFINED)?;
        let index = ctx.root(Value::number(index as f64))?;
        let args = ctx.args(&[item, index])?;
        let key = call::call(ctx, callback, undef, args)?;
        let key = ctx.root(key)?;
        let key = convert::to_property_key(ctx, key)?;
        let out = ctx.root(out)?;
        let group = get(ctx, out, key)?;
        if let Some(id) = ctx.get(group).as_object() {
            let ObjectData::Array(array) = &ctx.heap().object(id).data else {
                unreachable!()
            };
            push_array(ctx, group, u64::from(array.length), item)
        } else {
            let group = ctx.new_array()?;
            push_array(ctx, group, 0, item)?;
            put(ctx, out, key, group)
        }
    })?;
    Ok(out)
}
