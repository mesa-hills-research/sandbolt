use crate::atom::Atom;
use crate::builtins::make_native_fn;
use crate::object::{ErrorKind, ObjectData, ObjectFlags};
use crate::ops::property::PropertyDescriptor;
use crate::realm::NativeFn;
use crate::value::{Abrupt, ValueKind};
use crate::{EngineLimits, Runtime};

use super::*;

fn run(test: impl for<'s> FnOnce(&mut ExecCtx<'_, 's>) -> JsResult<()>) {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&CALLBACKS);
    let realm = runtime.default_realm();
    runtime.install_builtins(realm).unwrap();
    runtime.with_scope(realm, |ctx| {
        if let Err(error) = test(ctx) {
            let message = ctx.pending_exception().and_then(|value| {
                let error = ctx.root(value).ok()?;
                let message =
                    property::get_property(ctx, error, PropKey::atom(Atom::MESSAGE)).ok()?;
                let ValueKind::String(id) = message.decode() else {
                    return None;
                };
                Some(String::from_utf16_lossy(
                    &ctx.heap().string_units(id).collect::<Vec<_>>(),
                ))
            });
            panic!("{error:?}: {message:?}");
        }
    });
    assert_eq!(runtime.vm.stack_len(), 0);
    assert_eq!(runtime.vm.frame_count(), 0);
}

fn global<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str) -> JsResult<Handle<'s>> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    named(ctx, global, name)
}

fn named<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, name: &str) -> JsResult<Handle<'s>> {
    let key = ctx.intern_str(name)?;
    get(ctx, object, key)
}

fn put_named<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    value: Handle<'s>,
) -> JsResult<()> {
    let key = ctx.intern_str(name)?;
    put(ctx, object, key, value)
}

fn method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    base: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> JsResult<Handle<'s>> {
    let fun = named(ctx, base, name)?;
    invoke(ctx, fun, base, args)
}

fn proto_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    proto: Handle<'s>,
    name: &str,
    receiver: Handle<'s>,
    args: &[Handle<'s>],
) -> JsResult<Handle<'s>> {
    let fun = named(ctx, proto, name)?;
    invoke(ctx, fun, receiver, args)
}

fn invoke<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    fun: Handle<'s>,
    this: Handle<'s>,
    args: &[Handle<'s>],
) -> JsResult<Handle<'s>> {
    let args = ctx.args(args)?;
    let result = call::call(ctx, fun, this, args)?;
    ctx.root(result)
}

fn eq(ctx: &ExecCtx<'_, '_>, value: Value, expected: Value) {
    assert!(
        crate::ops::compare::same_value_raw(ctx.heap(), value, expected),
        "{value:?} != {expected:?}"
    );
}

fn text(ctx: &ExecCtx<'_, '_>, value: Value, expected: &str) {
    let ValueKind::String(id) = value.decode() else {
        panic!("expected string, got {value:?}");
    };
    assert!(
        ctx.heap().string_units(id).eq(expected.encode_utf16()),
        "expected {expected:?}"
    );
}

fn number<'s>(ctx: &mut ExecCtx<'_, 's>, n: i32) -> JsResult<Handle<'s>> {
    ctx.root(Value::int32(n))
}

fn ok_bool(ctx: &ExecCtx<'_, '_>, value: Value, expected: bool) {
    assert_eq!(value, Value::bool(expected));
    assert!(ctx.pending_exception().is_none());
}

fn throws<T: std::fmt::Debug>(ctx: &mut ExecCtx<'_, '_>, result: JsResult<T>, kind: ErrorKind) {
    assert_eq!(result.unwrap_err(), Abrupt::Thrown);
    let object = ctx.take_exception().unwrap().0.as_object().unwrap();
    let ObjectData::Error(error) = &ctx.heap().object(object).data else {
        panic!("expected Error");
    };
    assert_eq!(error.kind, kind);
}

fn make_array<'s>(ctx: &mut ExecCtx<'_, 's>, values: &[Handle<'s>]) -> JsResult<Handle<'s>> {
    let array = ctx.new_array()?;
    for (i, &value) in values.iter().enumerate() {
        push_array(ctx, array, i as u64, value)?;
    }
    Ok(array)
}

fn index<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, i: u32) -> JsResult<Handle<'s>> {
    get(ctx, object, PropKey::index(i).unwrap())
}

fn accessor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    setter: bool,
) -> JsResult<Handle<'s>> {
    let key = ctx.intern_str(name)?;
    let d = property::get_own_property_descriptor(ctx, object, key)?.unwrap();
    let value = if setter { d.set } else { d.get }.unwrap();
    ctx.root(value)
}

fn num_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    expected: f64,
) -> JsResult<()> {
    let value = named(ctx, object, name)?;
    assert_eq!(ctx.get(value).as_number(), Some(expected));
    Ok(())
}

#[test]
fn global_and_builtin_function_metadata() {
    run(|ctx| {
        let g = ctx.root(Value::object(ctx.realm().global_object))?;
        for (name, length) in [
            ("globalThis", None),
            ("Object", Some(1)),
            ("Function", Some(1)),
            ("Error", Some(1)),
            ("AggregateError", Some(2)),
            ("SuppressedError", Some(3)),
            ("EvalError", Some(1)),
            ("RangeError", Some(1)),
            ("ReferenceError", Some(1)),
            ("SyntaxError", Some(1)),
            ("TypeError", Some(1)),
            ("URIError", Some(1)),
            ("InternalError", Some(1)),
            ("Reflect", None),
            ("Proxy", Some(2)),
        ] {
            let key = ctx.intern_str(name)?;
            let d = property::get_own_property_descriptor(ctx, g, key)?.unwrap();
            assert_eq!(
                (d.writable, d.enumerable, d.configurable),
                (Some(true), Some(false), Some(true))
            );
            let value = ctx.root(d.value.unwrap())?;
            if let Some(length) = length {
                check_metadata(ctx, value, name, length)?;
            } else if name == "globalThis" {
                assert_eq!(ctx.get(value), ctx.get(g));
            } else {
                assert!(!call::is_callable(
                    ctx.heap(),
                    ctx.get(value).as_object().unwrap()
                ));
            }
        }
        let object = global(ctx, "Object")?;
        let proto = named(ctx, object, "prototype")?;
        let function = global(ctx, "Function")?;
        let function_proto = named(ctx, function, "prototype")?;
        let reflect = global(ctx, "Reflect")?;
        let proxy = global(ctx, "Proxy")?;
        for (base, methods) in [
            (
                object,
                &[
                    ("assign", 2),
                    ("create", 2),
                    ("defineProperty", 3),
                    ("defineProperties", 2),
                    ("entries", 1),
                    ("freeze", 1),
                    ("fromEntries", 1),
                    ("getOwnPropertyDescriptor", 2),
                    ("getOwnPropertyDescriptors", 1),
                    ("getOwnPropertyNames", 1),
                    ("getOwnPropertySymbols", 1),
                    ("getPrototypeOf", 1),
                    ("groupBy", 2),
                    ("hasOwn", 2),
                    ("is", 2),
                    ("isExtensible", 1),
                    ("isFrozen", 1),
                    ("isSealed", 1),
                    ("keys", 1),
                    ("preventExtensions", 1),
                    ("seal", 1),
                    ("setPrototypeOf", 2),
                    ("values", 1),
                ][..],
            ),
            (
                proto,
                &[
                    ("hasOwnProperty", 1),
                    ("isPrototypeOf", 1),
                    ("propertyIsEnumerable", 1),
                    ("toLocaleString", 0),
                    ("toString", 0),
                    ("valueOf", 0),
                    ("__defineGetter__", 2),
                    ("__defineSetter__", 2),
                    ("__lookupGetter__", 1),
                    ("__lookupSetter__", 1),
                ],
            ),
            (
                function_proto,
                &[("apply", 2), ("bind", 1), ("call", 1), ("toString", 0)],
            ),
            (
                reflect,
                &[
                    ("apply", 3),
                    ("construct", 2),
                    ("defineProperty", 3),
                    ("deleteProperty", 2),
                    ("get", 2),
                    ("getOwnPropertyDescriptor", 2),
                    ("getPrototypeOf", 1),
                    ("has", 2),
                    ("isExtensible", 1),
                    ("ownKeys", 1),
                    ("preventExtensions", 1),
                    ("set", 3),
                    ("setPrototypeOf", 2),
                ],
            ),
            (proxy, &[("revocable", 2)]),
        ] {
            for &(name, length) in methods {
                let key = ctx.intern_str(name)?;
                let d = property::get_own_property_descriptor(ctx, base, key)?.unwrap();
                assert_eq!(
                    (d.writable, d.enumerable, d.configurable),
                    (Some(true), Some(false), Some(true))
                );
                let fun = ctx.root(d.value.unwrap())?;
                check_metadata(ctx, fun, name, length)?;
                assert!(
                    !ctx.heap()
                        .object(ctx.get(fun).as_object().unwrap())
                        .flags
                        .contains(ObjectFlags::IS_CONSTRUCTOR)
                );
            }
        }
        let tag = symbol_key(ctx, WellKnown::ToStringTag);
        let d = property::get_own_property_descriptor(ctx, reflect, tag)?.unwrap();
        assert_eq!(
            (d.writable, d.enumerable, d.configurable),
            (Some(false), Some(false), Some(true))
        );
        text(ctx, d.value.unwrap(), "Reflect");
        Ok(())
    });
}

fn check_metadata<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    fun: Handle<'s>,
    name: &str,
    length: i32,
) -> JsResult<()> {
    for (key, flags) in [(Atom::LENGTH, length), (Atom::NAME, -1)] {
        let d = property::get_own_property_descriptor(ctx, fun, PropKey::atom(key))?.unwrap();
        assert_eq!(
            (d.writable, d.enumerable, d.configurable),
            (Some(false), Some(false), Some(true))
        );
        let value = d.value.unwrap();
        if flags == -1 {
            text(ctx, value, name);
        } else {
            eq(ctx, value, Value::int32(flags));
        }
    }
    Ok(())
}

#[test]
fn object_conversion_prototypes_and_annex_b_accessors() {
    run(|ctx| {
        let ctor = global(ctx, "Object")?;
        let proto = named(ctx, ctor, "prototype")?;
        let constructor = named(ctx, proto, "constructor")?;
        assert_eq!(ctx.get(constructor), ctx.get(ctor));
        let created = invoke(ctx, ctor, ctor, &[])?;
        let existing = invoke(ctx, ctor, ctor, &[created])?;
        assert_eq!(ctx.get(created), ctx.get(existing));
        let one = number(ctx, 1)?;
        let args = ctx.args(&[one])?;
        let boxed = call::construct(ctx, ctor, args, ctor)?;
        let boxed = ctx.root(boxed)?;
        let val = method(ctx, boxed, "valueOf", &[])?;
        eq(ctx, ctx.get(val), ctx.get(one));
        let target = ctx.new_native(NativeFnId(3), true)?;
        let custom_proto = ctx.new_object()?;
        put_named(ctx, target, "prototype", custom_proto)?;
        let subclass = call::construct(ctx, ctor, args, target)?;
        let subclass = ctx.root(subclass)?;
        let result = method(ctx, ctor, "getPrototypeOf", &[subclass])?;
        assert_eq!(ctx.get(result), ctx.get(custom_proto));
        let yes = proto_method(ctx, proto, "isPrototypeOf", custom_proto, &[subclass])?;
        ok_bool(ctx, ctx.get(yes), true);
        let result = method(ctx, ctor, "setPrototypeOf", &[subclass, proto])?;
        assert_eq!(ctx.get(result), ctx.get(subclass));
        let proto_get = accessor(ctx, proto, "__proto__", false)?;
        let proto_set = accessor(ctx, proto, "__proto__", true)?;
        let result = invoke(ctx, proto_get, subclass, &[])?;
        assert_eq!(ctx.get(result), ctx.get(proto));
        invoke(ctx, proto_set, subclass, &[custom_proto])?;
        let result = invoke(ctx, proto_get, subclass, &[])?;
        assert_eq!(ctx.get(result), ctx.get(custom_proto));
        let null = ctx.root(Value::NULL)?;
        let result = invoke(ctx, proto_get, null, &[]);
        throws(ctx, result, ErrorKind::Type);
        let key = ctx.new_string("annexKey")?;
        let getter = ctx.new_native(NativeFnId(3), false)?;
        let setter = ctx.new_native(NativeFnId(4), false)?;
        proto_method(ctx, proto, "__defineGetter__", custom_proto, &[key, getter])?;
        proto_method(ctx, proto, "__defineSetter__", custom_proto, &[key, setter])?;
        let result = proto_method(ctx, proto, "__lookupGetter__", subclass, &[key])?;
        assert_eq!(ctx.get(result), ctx.get(getter));
        let result = proto_method(ctx, proto, "__lookupSetter__", subclass, &[key])?;
        assert_eq!(ctx.get(result), ctx.get(setter));
        let result = proto_method(ctx, proto, "valueOf", created, &[])?;
        assert_eq!(ctx.get(result), ctx.get(created));
        let result = proto_method(ctx, proto, "toLocaleString", created, &[])?;
        text(ctx, ctx.get(result), "[object Object]");
        let result = proto_method(ctx, proto, "toString", null, &[])?;
        text(ctx, ctx.get(result), "[object Null]");
        let tag = ctx.new_string("Custom")?;
        put(ctx, created, symbol_key(ctx, WellKnown::ToStringTag), tag)?;
        let result = proto_method(ctx, proto, "toString", created, &[])?;
        text(ctx, ctx.get(result), "[object Custom]");
        Ok(())
    });
}

#[test]
fn object_descriptors_copy_enumeration_and_integrity() {
    run(|ctx| {
        let object = global(ctx, "Object")?;
        let proto = named(ctx, object, "prototype")?;
        let target = ctx.new_object()?;
        let desc = ctx.new_object()?;
        let one = number(ctx, 1)?;
        let two = number(ctx, 2)?;
        let yes = ctx.root(Value::bool(true))?;
        put_named(ctx, desc, "value", one)?;
        put_named(ctx, desc, "enumerable", yes)?;
        put_named(ctx, desc, "writable", yes)?;
        put_named(ctx, desc, "configurable", yes)?;
        let k = ctx.new_string("dynamicKey")?;
        method(ctx, object, "defineProperty", &[target, k, desc])?;
        let descriptors = ctx.new_object()?;
        let symbol = ctx.new_symbol(None, false)?;
        let symkey = convert::to_property_key(ctx, symbol)?;
        put(ctx, descriptors, symkey, desc)?;
        method(ctx, object, "defineProperties", &[target, descriptors])?;
        let null = ctx.root(Value::NULL)?;
        let created = method(ctx, object, "create", &[null, descriptors])?;
        assert_eq!(property::get_prototype_of(ctx, created)?, None);
        let result = method(ctx, object, "getOwnPropertyDescriptor", &[created, symbol])?;
        num_property(ctx, result, "value", 1.0)?;
        let result = method(ctx, object, "getOwnPropertyDescriptors", &[target])?;
        let result = get(ctx, result, symkey)?;
        num_property(ctx, result, "value", 1.0)?;
        let result = method(ctx, object, "hasOwn", &[target, symbol])?;
        ok_bool(ctx, ctx.get(result), true);
        for name in ["hasOwnProperty", "propertyIsEnumerable"] {
            let result = proto_method(ctx, proto, name, target, &[k])?;
            ok_bool(ctx, ctx.get(result), true);
        }
        for name in ["getOwnPropertyNames", "keys"] {
            let result = method(ctx, object, name, &[target])?;
            num_property(ctx, result, "length", 1.0)?;
            let value = index(ctx, result, 0)?;
            text(ctx, ctx.get(value), "dynamicKey");
        }
        let result = method(ctx, object, "getOwnPropertySymbols", &[target])?;
        let value = index(ctx, result, 0)?;
        eq(ctx, ctx.get(value), ctx.get(symbol));
        let result = method(ctx, object, "values", &[target])?;
        let value = index(ctx, result, 0)?;
        eq(ctx, ctx.get(value), ctx.get(one));
        let result = method(ctx, object, "entries", &[target])?;
        let pair = index(ctx, result, 0)?;
        let value = index(ctx, pair, 0)?;
        text(ctx, ctx.get(value), "dynamicKey");
        let value = index(ctx, pair, 1)?;
        eq(ctx, ctx.get(value), ctx.get(one));
        let copy = ctx.new_object()?;
        let result = method(ctx, object, "assign", &[copy, null, target])?;
        assert_eq!(ctx.get(result), ctx.get(copy));
        let value = get(ctx, copy, symkey)?;
        eq(ctx, ctx.get(value), ctx.get(one));
        let value = method(ctx, object, "isExtensible", &[copy])?;
        ok_bool(ctx, ctx.get(value), true);
        method(ctx, object, "preventExtensions", &[copy])?;
        for name in ["isExtensible", "isSealed", "isFrozen"] {
            let value = method(ctx, object, name, &[copy])?;
            ok_bool(ctx, ctx.get(value), false);
        }
        method(ctx, object, "seal", &[copy])?;
        let value = method(ctx, object, "isSealed", &[copy])?;
        ok_bool(ctx, ctx.get(value), true);
        let value = method(ctx, object, "isFrozen", &[copy])?;
        ok_bool(ctx, ctx.get(value), false);
        assert!(property::set_property(ctx, copy, symkey, two)?);
        method(ctx, object, "freeze", &[copy])?;
        let value = method(ctx, object, "isFrozen", &[copy])?;
        ok_bool(ctx, ctx.get(value), true);
        assert!(!property::set_property(ctx, copy, symkey, one)?);
        let minus = ctx.root(Value::number(-0.0))?;
        let plus = number(ctx, 0)?;
        let value = method(ctx, object, "is", &[minus, plus])?;
        ok_bool(ctx, ctx.get(value), false);
        let nan = ctx.root(Value::number(f64::NAN))?;
        let value = method(ctx, object, "is", &[nan, nan])?;
        ok_bool(ctx, ctx.get(value), true);
        Ok(())
    });
}

const CALLBACKS: [NativeFn; 9] = [
    iterator_this,
    iterator_next,
    iterator_return,
    return_this,
    save_value,
    return_second,
    throw_first,
    read_key,
    count_call,
];

fn iterator_this<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    Ok(ctx.get(this))
}

fn iterator_next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let items = named(ctx, this, "items")?;
    let cursor = named(ctx, this, "cursor")?;
    let cursor = ctx.get(cursor).as_number().unwrap() as u32;
    let length = named(ctx, items, "length")?;
    let done = cursor as f64 >= ctx.get(length).as_number().unwrap();
    let result = ctx.new_object()?;
    let done_value = ctx.root(Value::bool(done))?;
    put_named(ctx, result, "done", done_value)?;
    if !done {
        let value = index(ctx, items, cursor)?;
        put_named(ctx, result, "value", value)?;
        let next = number(ctx, cursor as i32 + 1)?;
        put_named(ctx, this, "cursor", next)?;
    }
    Ok(ctx.get(result))
}

fn iterator_return<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    let closed = ctx.root(Value::bool(true))?;
    put_named(ctx, this, "closed", closed)?;
    Err(ctx.throw_type_error("iterator return"))
}

fn return_this<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    Ok(ctx.get(this))
}

fn save_value<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    put_named(ctx, this, "saved", args.get(0).unwrap())?;
    Ok(Value::UNDEFINED)
}

fn return_second<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    Ok(ctx.get(args.get(1).unwrap()))
}

fn throw_first<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    Err(ctx.throw(args.get(0).unwrap()))
}

fn read_key<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let value = named(ctx, args.get(0).unwrap(), "key")?;
    Ok(ctx.get(value))
}

fn count_call<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let count = args.get(0).unwrap();
    let one = number(ctx, 1)?;
    put_named(ctx, count, "read", one)?;
    Ok(Value::int32(0))
}

fn iterable<'s>(ctx: &mut ExecCtx<'_, 's>, items: &[Handle<'s>]) -> JsResult<Handle<'s>> {
    let array = make_array(ctx, items)?;
    let it = ctx.new_object()?;
    let iter_method = ctx.new_native(NativeFnId(0), false)?;
    let next = ctx.new_native(NativeFnId(1), false)?;
    let ret = ctx.new_native(NativeFnId(2), false)?;
    put(ctx, it, symbol_key(ctx, WellKnown::Iterator), iter_method)?;
    put_named(ctx, it, "next", next)?;
    put_named(ctx, it, "return", ret)?;
    put_named(ctx, it, "items", array)?;
    let zero = number(ctx, 0)?;
    put_named(ctx, it, "cursor", zero)?;
    Ok(it)
}

#[test]
fn object_iterable_operations_keep_keys_values_and_original_exceptions() {
    run(|ctx| {
        let object = global(ctx, "Object")?;
        let proto = ctx.new_string("__proto__")?;
        let symbol = ctx.new_symbol(None, false)?;
        let one = number(ctx, 1)?;
        let two = number(ctx, 2)?;
        let p0 = make_array(ctx, &[proto, one])?;
        let p1 = make_array(ctx, &[symbol, two])?;
        let p2 = make_array(ctx, &[proto, two])?;
        let input = iterable(ctx, &[p0, p1, p2])?;
        let out = method(ctx, object, "fromEntries", &[input])?;
        assert_eq!(
            property::get_prototype_of(ctx, out)?,
            Some(ctx.realm().object_proto)
        );
        let val = named(ctx, out, "__proto__")?;
        eq(ctx, ctx.get(val), ctx.get(two));
        let key = convert::to_property_key(ctx, symbol)?;
        let val = get(ctx, out, key)?;
        eq(ctx, ctx.get(val), ctx.get(two));
        let v0 = ctx.new_object()?;
        let v1 = ctx.new_object()?;
        let v2 = ctx.new_object()?;
        put_named(ctx, v0, "key", proto)?;
        put_named(ctx, v1, "key", symbol)?;
        put_named(ctx, v2, "key", proto)?;
        let input = iterable(ctx, &[v0, v1, v2])?;
        let cb = ctx.new_native(NativeFnId(7), false)?;
        let out = method(ctx, object, "groupBy", &[input, cb])?;
        assert_eq!(property::get_prototype_of(ctx, out)?, None);
        let group = named(ctx, out, "__proto__")?;
        num_property(ctx, group, "length", 2.0)?;
        let val = index(ctx, group, 1)?;
        assert_eq!(ctx.get(val), ctx.get(v2));
        let group = get(ctx, out, key)?;
        let val = index(ctx, group, 0)?;
        assert_eq!(ctx.get(val), ctx.get(v1));
        let bad = iterable(ctx, &[one])?;
        let cb = ctx.new_native(NativeFnId(6), false)?;
        assert_eq!(
            method(ctx, object, "groupBy", &[bad, cb]).unwrap_err(),
            Abrupt::Thrown
        );
        let thrown = ctx.take_exception().unwrap().0;
        eq(ctx, thrown, ctx.get(one));
        let closed = named(ctx, bad, "closed")?;
        ok_bool(ctx, ctx.get(closed), true);
        let bad = iterable(ctx, &[one])?;
        let result = method(ctx, object, "fromEntries", &[bad]);
        throws(ctx, result, ErrorKind::Type);
        let closed = named(ctx, bad, "closed")?;
        ok_bool(ctx, ctx.get(closed), true);
        Ok(())
    });
}

#[test]
fn function_apply_call_bind_and_has_instance() {
    run(|ctx| {
        let function = global(ctx, "Function")?;
        let proto = named(ctx, function, "prototype")?;
        let ctor = named(ctx, proto, "constructor")?;
        assert_eq!(ctx.get(ctor), ctx.get(function));
        let undefined = ctx.root(Value::UNDEFINED)?;
        let value = invoke(ctx, proto, undefined, &[])?;
        assert_eq!(ctx.get(value), Value::UNDEFINED);
        let callable = make_native_fn(ctx, NativeFnId(5), "chooseSecond", 3, true)?;
        let one = number(ctx, 1)?;
        let two = number(ctx, 2)?;
        let input = make_array(ctx, &[one, two])?;
        let receiver = ctx.new_object()?;
        let result = method(ctx, callable, "apply", &[receiver, input])?;
        eq(ctx, ctx.get(result), ctx.get(two));
        let result = method(ctx, callable, "call", &[receiver, one, two])?;
        eq(ctx, ctx.get(result), ctx.get(two));
        let prototype = ctx.new_object()?;
        put_named(ctx, callable, "prototype", prototype)?;
        let custom_proto = ctx.new_object_with_proto(Some(proto))?;
        assert!(property::set_prototype_of(
            ctx,
            callable,
            Some(custom_proto)
        )?);
        let bound = method(ctx, callable, "bind", &[receiver, one])?;
        check_metadata(ctx, bound, "bound chooseSecond", 2)?;
        assert_eq!(
            property::get_prototype_of(ctx, bound)?,
            ctx.get(custom_proto).as_object()
        );
        let result = invoke(ctx, bound, undefined, &[two])?;
        eq(ctx, ctx.get(result), ctx.get(two));
        let args = ctx.args(&[two])?;
        let constructed = call::construct(ctx, bound, args, bound)?;
        let constructed = ctx.root(constructed)?;
        assert_eq!(
            property::get_prototype_of(ctx, constructed)?,
            ctx.get(prototype).as_object()
        );
        let key = symbol_key(ctx, WellKnown::HasInstance);
        let d = property::get_own_property_descriptor(ctx, proto, key)?.unwrap();
        assert_eq!(
            (d.writable, d.enumerable, d.configurable),
            (Some(false), Some(false), Some(false))
        );
        let test = ctx.root(d.value.unwrap())?;
        check_metadata(ctx, test, "[Symbol.hasInstance]", 1)?;
        let result = invoke(ctx, test, bound, &[constructed])?;
        ok_bool(ctx, ctx.get(result), true);
        let result = invoke(ctx, test, bound, &[two])?;
        ok_bool(ctx, ctx.get(result), false);
        let object = global(ctx, "Object")?;
        let result = method(ctx, object, "toString", &[])?;
        text(ctx, ctx.get(result), "function Object() { [native code] }");
        let thrower = accessor(ctx, proto, "caller", false)?;
        for (key, setter) in [("caller", true), ("arguments", false), ("arguments", true)] {
            let accessor = accessor(ctx, proto, key, setter)?;
            assert_eq!(ctx.get(accessor), ctx.get(thrower));
        }
        assert!(!property::is_extensible(ctx, thrower)?);
        let result = named(ctx, proto, "caller");
        throws(ctx, result, ErrorKind::Type);
        let result = method(ctx, callable, "apply", &[receiver, one]);
        throws(ctx, result, ErrorKind::Type);
        Ok(())
    });
}

#[test]
fn error_family_brands_messages_causes_and_stack_accessors() {
    run(|ctx| {
        let error = global(ctx, "Error")?;
        let proto = named(ctx, error, "prototype")?;
        let text_arg = ctx.new_string("broken")?;
        let one = number(ctx, 1)?;
        let two = number(ctx, 2)?;
        let parent_opts = ctx.new_object()?;
        put_named(ctx, parent_opts, "cause", two)?;
        let opts = ctx.new_object_with_proto(Some(parent_opts))?;
        for name in [
            "Error",
            "EvalError",
            "RangeError",
            "ReferenceError",
            "SyntaxError",
            "TypeError",
            "URIError",
            "InternalError",
            "AggregateError",
            "SuppressedError",
        ] {
            let ctor = global(ctx, name)?;
            let specific = named(ctx, ctor, "prototype")?;
            if name != "Error" {
                assert_eq!(
                    property::get_prototype_of(ctx, specific)?,
                    ctx.get(proto).as_object()
                );
                assert_eq!(
                    property::get_prototype_of(ctx, ctor)?,
                    ctx.get(error).as_object()
                );
            }
            let inherited = named(ctx, specific, "message")?;
            text(ctx, ctx.get(inherited), "");
            let instance = match name {
                "AggregateError" => {
                    let input = iterable(ctx, &[one, two])?;
                    let instance = invoke(ctx, ctor, error, &[input, text_arg, opts])?;
                    let values = named(ctx, instance, "errors")?;
                    let value = index(ctx, values, 1)?;
                    eq(ctx, ctx.get(value), ctx.get(two));
                    assert_eq!(
                        property::get_own_property_descriptor(ctx, instance, Atom::get("errors"))?
                            .unwrap()
                            .enumerable,
                        Some(false)
                    );
                    instance
                }
                "SuppressedError" => {
                    let instance = invoke(ctx, ctor, error, &[one, two, text_arg, opts])?;
                    for (name, expected) in [("error", one), ("suppressed", two)] {
                        let key = ctx.intern_str(name)?;
                        let d = property::get_own_property_descriptor(ctx, instance, key)?.unwrap();
                        assert_eq!(
                            (d.writable, d.enumerable, d.configurable),
                            (Some(true), Some(false), Some(true))
                        );
                        eq(ctx, d.value.unwrap(), ctx.get(expected));
                    }
                    instance
                }
                _ => invoke(ctx, ctor, error, &[text_arg, opts])?,
            };
            assert_eq!(
                property::get_prototype_of(ctx, instance)?,
                ctx.get(specific).as_object()
            );
            let cause =
                property::get_own_property_descriptor(ctx, instance, Atom::get("cause"))?.unwrap();
            assert_eq!(
                (cause.writable, cause.enumerable, cause.configurable),
                (Some(true), Some(false), Some(true))
            );
            eq(ctx, cause.value.unwrap(), ctx.get(two));
            let output = method(ctx, instance, "toString", &[])?;
            text(ctx, ctx.get(output), &format!("{name}: broken"));
            let branded = method(ctx, error, "isError", &[instance])?;
            ok_bool(ctx, ctx.get(branded), true);
            let branded = method(ctx, error, "isError", &[specific])?;
            ok_bool(ctx, ctx.get(branded), false);
        }
        let new_target = ctx.new_native(NativeFnId(3), true)?;
        let custom_proto = ctx.new_object()?;
        put_named(ctx, new_target, "prototype", custom_proto)?;
        let output = call::construct(ctx, error, Args::empty(), new_target)?;
        let output = ctx.root(output)?;
        assert_eq!(
            property::get_prototype_of(ctx, output)?,
            ctx.get(custom_proto).as_object()
        );
        assert!(
            property::get_own_property_descriptor(ctx, output, PropKey::atom(Atom::MESSAGE))?
                .is_none()
        );
        let stack_get = accessor(ctx, proto, "stack", false)?;
        let stack_set = accessor(ctx, proto, "stack", true)?;
        let out = invoke(ctx, stack_get, custom_proto, &[])?;
        assert_eq!(ctx.get(out), Value::UNDEFINED);
        let result = invoke(ctx, stack_set, output, &[one]);
        throws(ctx, result, ErrorKind::Type);
        let result = invoke(ctx, stack_set, proto, &[text_arg]);
        throws(ctx, result, ErrorKind::Type);
        invoke(ctx, stack_set, output, &[text_arg])?;
        let d = property::get_own_property_descriptor(ctx, output, Atom::get("stack"))?.unwrap();
        assert_eq!(
            (d.writable, d.enumerable, d.configurable),
            (Some(true), Some(true), Some(true))
        );
        text(ctx, d.value.unwrap(), "broken");
        Ok(())
    });
}

#[test]
fn reflect_methods_keep_receivers_and_distinguish_false_from_throw() {
    run(|ctx| {
        let reflect = global(ctx, "Reflect")?;
        let target = ctx.new_object()?;
        let receiver = ctx.new_object()?;
        let key = ctx.new_string("field")?;
        let desc = ctx.new_object()?;
        let get_fn = ctx.new_native(NativeFnId(3), false)?;
        let set_fn = ctx.new_native(NativeFnId(4), false)?;
        let one = number(ctx, 1)?;
        put_named(ctx, desc, "get", get_fn)?;
        put_named(ctx, desc, "set", set_fn)?;
        let result = method(ctx, reflect, "defineProperty", &[target, key, desc])?;
        ok_bool(ctx, ctx.get(result), true);
        let result = method(ctx, reflect, "getOwnPropertyDescriptor", &[target, key])?;
        let get = named(ctx, result, "get")?;
        assert_eq!(ctx.get(get), ctx.get(get_fn));
        let result = method(ctx, reflect, "get", &[target, key, receiver])?;
        assert_eq!(ctx.get(result), ctx.get(receiver));
        let result = method(ctx, reflect, "set", &[target, key, one, receiver])?;
        ok_bool(ctx, ctx.get(result), true);
        num_property(ctx, receiver, "saved", 1.0)?;
        let result = method(ctx, reflect, "deleteProperty", &[target, key])?;
        ok_bool(ctx, ctx.get(result), false);
        let result = method(ctx, reflect, "has", &[target, key])?;
        ok_bool(ctx, ctx.get(result), true);
        let symbol = ctx.new_symbol(None, false)?;
        let symkey = convert::to_property_key(ctx, symbol)?;
        put(ctx, target, symkey, one)?;
        let result = method(ctx, reflect, "ownKeys", &[target])?;
        let k = index(ctx, result, 0)?;
        text(ctx, ctx.get(k), "field");
        let k = index(ctx, result, 1)?;
        eq(ctx, ctx.get(k), ctx.get(symbol));
        let result = method(ctx, reflect, "setPrototypeOf", &[target, receiver])?;
        ok_bool(ctx, ctx.get(result), true);
        let result = method(ctx, reflect, "getPrototypeOf", &[target])?;
        assert_eq!(ctx.get(result), ctx.get(receiver));
        let result = method(ctx, reflect, "isExtensible", &[target])?;
        ok_bool(ctx, ctx.get(result), true);
        let result = method(ctx, reflect, "preventExtensions", &[target])?;
        ok_bool(ctx, ctx.get(result), true);
        let result = method(ctx, reflect, "isExtensible", &[target])?;
        ok_bool(ctx, ctx.get(result), false);
        let new_key = ctx.new_string("missing")?;
        let result = method(ctx, reflect, "defineProperty", &[target, new_key, desc])?;
        ok_bool(ctx, ctx.get(result), false);
        let result = method(ctx, reflect, "setPrototypeOf", &[target, desc])?;
        ok_bool(ctx, ctx.get(result), false);
        let list = make_array(ctx, &[one])?;
        let result = method(ctx, reflect, "apply", &[get_fn, receiver, list])?;
        assert_eq!(ctx.get(result), ctx.get(receiver));
        let object = global(ctx, "Object")?;
        let result = method(ctx, reflect, "construct", &[object, list])?;
        let value = method(ctx, result, "valueOf", &[])?;
        eq(ctx, ctx.get(value), ctx.get(one));
        let monitored = ctx.new_object()?;
        let getter = ctx.new_native(NativeFnId(8), false)?;
        let args = ctx.args(&[monitored])?;
        let length_get = ctx.bind_function(getter, monitored, args)?;
        let list = ctx.new_object()?;
        define_or_throw(
            ctx,
            list,
            PropKey::atom(Atom::LENGTH),
            PropertyDescriptor {
                get: Some(length_get),
                ..PropertyDescriptor::default()
            },
        )?;
        let result = method(ctx, reflect, "apply", &[one, receiver, list]);
        throws(ctx, result, ErrorKind::Type);
        let undefined = ctx.root(Value::UNDEFINED)?;
        let result = method(ctx, reflect, "construct", &[object, list, undefined]);
        throws(ctx, result, ErrorKind::Type);
        let read = named(ctx, monitored, "read")?;
        assert_eq!(ctx.get(read), Value::UNDEFINED);
        let result = method(ctx, reflect, "apply", &[get_fn, receiver, list])?;
        assert_eq!(ctx.get(result), ctx.get(receiver));
        num_property(ctx, monitored, "read", 1.0)?;
        Ok(())
    });
}

#[test]
fn proxy_validates_arguments_and_revocation_is_idempotent() {
    run(|ctx| {
        let ctor = global(ctx, "Proxy")?;
        assert!(
            property::get_own_property_descriptor(ctx, ctor, PropKey::atom(Atom::PROTOTYPE))?
                .is_none()
        );
        let target = ctx.new_object()?;
        let handler = ctx.new_object()?;
        let val = number(ctx, 5)?;
        put_named(ctx, target, "test", val)?;
        let result = invoke(ctx, ctor, target, &[target, handler]);
        throws(ctx, result, ErrorKind::Type);
        let args = ctx.args(&[val, handler])?;
        let result = call::construct(ctx, ctor, args, ctor);
        throws(ctx, result, ErrorKind::Type);
        let args = ctx.args(&[target, handler])?;
        let result = call::construct(ctx, ctor, args, ctor)?;
        let proxy = ctx.root(result)?;
        num_property(ctx, proxy, "test", 5.0)?;
        assert!(!call::is_callable(
            ctx.heap(),
            ctx.get(proxy).as_object().unwrap()
        ));
        let getter = ctx.new_native(NativeFnId(3), true)?;
        let args = ctx.args(&[getter, handler])?;
        let result = call::construct(ctx, ctor, args, ctor)?;
        let proxy = ctx.root(result)?;
        assert!(call::is_callable(
            ctx.heap(),
            ctx.get(proxy).as_object().unwrap()
        ));
        assert!(
            ctx.heap()
                .object(ctx.get(proxy).as_object().unwrap())
                .flags
                .contains(ObjectFlags::IS_CONSTRUCTOR)
        );
        let receiver = ctx.new_object()?;
        let result = invoke(ctx, proxy, receiver, &[])?;
        assert_eq!(ctx.get(result), ctx.get(receiver));
        let pair = method(ctx, ctor, "revocable", &[target, handler])?;
        let proxy = named(ctx, pair, "proxy")?;
        let revoke = named(ctx, pair, "revoke")?;
        check_metadata(ctx, revoke, "", 0)?;
        num_property(ctx, proxy, "test", 5.0)?;
        let result = invoke(ctx, revoke, receiver, &[val])?;
        assert_eq!(ctx.get(result), Value::UNDEFINED);
        let result = invoke(ctx, revoke, receiver, &[])?;
        assert_eq!(ctx.get(result), Value::UNDEFINED);
        let result = named(ctx, proxy, "test");
        throws(ctx, result, ErrorKind::Type);
        let proto = ctx.root(Value::object(ctx.realm().object_proto))?;
        let result = proto_method(ctx, proto, "toString", proxy, &[]);
        throws(ctx, result, ErrorKind::Type);
        Ok(())
    });
}
