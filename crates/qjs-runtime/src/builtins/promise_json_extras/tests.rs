use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::object::{ErrorKind, PromiseData};
use crate::ops::call::construct;
use crate::ops::property::{
    PropertyDescriptor, define_own_property, get_own_property_descriptor, get_prototype_of,
    is_extensible, own_property_keys, set_prototype_of,
};
use crate::realm::NativeFn;
use crate::value::Abrupt;
use crate::{EngineLimits, Persistent, Runtime};

const THENABLE: NativeFnId = NativeFnId(0);
const TIMES_TWO: NativeFnId = NativeFnId(1);
const MICRO: NativeFnId = NativeFnId(2);
const CLEANUP: NativeFnId = NativeFnId(3);
const SUBCLASS: NativeFnId = NativeFnId(4);
const SPECIES_GETTER: NativeFnId = NativeFnId(5);
const ITERATOR: NativeFnId = NativeFnId(6);
const NEXT: NativeFnId = NativeFnId(7);
const CLOSE: NativeFnId = NativeFnId(8);
const THROW_RESOLVE: NativeFnId = NativeFnId(9);
const REVIVER: NativeFnId = NativeFnId(10);
const REPLACER: NativeFnId = NativeFnId(11);
const TO_JSON: NativeFnId = NativeFnId(12);
const RESOLVE_IDENTITY: NativeFnId = NativeFnId(13);
const BOTH: NativeFnId = NativeFnId(14);
const REALM_THEN: NativeFnId = NativeFnId(15);
const REENTER_JSON: NativeFnId = NativeFnId(16);
const THEN_GETTER_OOM: NativeFnId = NativeFnId(17);

const CALLBACKS: &[NativeFn] = &[
    thenable,
    times_two,
    microtask,
    cleanup,
    subclass,
    species_getter,
    iterator,
    next,
    close,
    throw_resolve,
    reviver,
    replacer,
    to_json,
    resolve_identity,
    both,
    realm_then,
    reenter_json,
    then_getter_oom,
];

thread_local! {
    static EVENTS: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static SOURCES: RefCell<Vec<(String, Option<String>)>> = const { RefCell::new(Vec::new()) };
}

fn runtime() -> Runtime {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(CALLBACKS);
    runtime.install_builtins(runtime.default_realm()).unwrap();
    EVENTS.with(|events| events.borrow_mut().clear());
    SOURCES.with(|sources| sources.borrow_mut().clear());
    runtime
}

fn invoke<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    receiver: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> JsResult<Handle<'s>> {
    let function = get_named(ctx, receiver, name)?;
    let out = call_with(ctx, function, receiver, args)?;
    ctx.root(out)
}

fn builtin<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str) -> Handle<'s> {
    let global = global(ctx).unwrap();
    get_named(ctx, global, name).unwrap()
}

fn promise_data<'a>(ctx: &'a ExecCtx<'_, '_>, promise: Value) -> &'a PromiseData {
    let ObjectData::Promise(data) = &ctx.heap().object(promise.as_object().unwrap()).data else {
        panic!("expected Promise")
    };
    data
}

fn snapshot(runtime: &mut Runtime, root: &Persistent) -> (Option<Value>, bool, bool) {
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let promise = ctx.get_persistent(root).unwrap().unwrap();
        let data = promise_data(ctx, ctx.get(promise));
        (data.result, data.rejected, data.is_handled)
    })
}

fn assert_text(ctx: &ExecCtx<'_, '_>, value: Value, expected: &str) {
    assert_eq!(text(ctx, value), expected);
}

fn text(ctx: &ExecCtx<'_, '_>, value: Value) -> String {
    let ValueKind::String(id) = value.decode() else {
        panic!("expected string, got {value:?}")
    };
    String::from_utf16(&ctx.heap().string_units(id).collect::<Vec<_>>()).unwrap()
}

fn assert_kind(ctx: &mut ExecCtx<'_, '_>, error: ErrorKind) {
    let value = ctx.take_exception().unwrap().0;
    let ObjectData::Error(data) = &ctx.heap().object(value.as_object().unwrap()).data else {
        panic!("expected Error")
    };
    assert_eq!(data.kind, error);
}

fn one<'s>(ctx: &mut ExecCtx<'_, 's>, callback: Handle<'s>, value: Handle<'s>) -> JsResult<Value> {
    let undefined = ctx.root(Value::UNDEFINED)?;
    call_with(ctx, callback, undefined, &[value])
}

fn thenable<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert!(ctx.get(this).as_object().is_some());
    EVENTS.with(|events| events.borrow_mut().push(1));
    let input = ctx.root(Value::int32(21))?;
    one(ctx, args.get(0).unwrap(), input)?;
    one(ctx, args.get(1).unwrap(), input)?;
    Err(ctx.throw(input))
}

fn times_two<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    EVENTS.with(|events| events.borrow_mut().push(3));
    let number = ctx.get(args.get(0).unwrap()).as_number().unwrap();
    Ok(Value::number(number * 2.0))
}

fn microtask<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::object(ctx.realm().global_object));
    assert!(args.is_empty());
    EVENTS.with(|events| events.borrow_mut().push(2));
    Ok(Value::UNDEFINED)
}

fn cleanup<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    assert!(args.is_empty());
    EVENTS.with(|events| events.borrow_mut().push(4));
    let ctor = builtin(ctx, "Promise");
    let value = ctx.root(Value::int32(99))?;
    let promise = invoke(ctx, ctor, "resolve", &[value])?;
    Ok(ctx.get(promise))
}

#[test]
fn thenable_tick_precedes_microtask_and_reaction() {
    let mut runtime = runtime();
    let (source, chained, finalized) = runtime.with_scope(runtime.default_realm(), |ctx| {
        let ctor = builtin(ctx, "Promise");
        let thenable = ctx.new_object().unwrap();
        let then = ctx.new_native(THENABLE, false).unwrap();
        data(ctx, thenable, "then", then, DATA).unwrap();
        let source = invoke(ctx, ctor, "resolve", &[thenable]).unwrap();
        assert!(promise_data(ctx, ctx.get(source)).result.is_none());
        let handler = ctx.new_native(TIMES_TWO, false).unwrap();
        let chained = invoke(ctx, source, "then", &[handler]).unwrap();
        let cleanup = ctx.new_native(CLEANUP, false).unwrap();
        let finalized = invoke(ctx, chained, "finally", &[cleanup]).unwrap();
        let global = global(ctx).unwrap();
        let micro = ctx.new_native(MICRO, false).unwrap();
        invoke(ctx, global, "queueMicrotask", &[micro, handler]).unwrap();
        assert!(EVENTS.with(|events| events.borrow().is_empty()));
        (
            ctx.persist(source).unwrap(),
            ctx.persist(chained).unwrap(),
            ctx.persist(finalized).unwrap(),
        )
    });
    runtime.collect();
    assert!(runtime.run_one_job().unwrap());
    assert_eq!(
        snapshot(&mut runtime, &source),
        (Some(Value::int32(21)), false, true)
    );
    assert_eq!(snapshot(&mut runtime, &chained).0, None);
    runtime.run_jobs().unwrap();
    assert_eq!(
        EVENTS.with(|events| events.borrow().clone()),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        snapshot(&mut runtime, &chained),
        (Some(Value::number(42.0)), false, true)
    );
    assert_eq!(
        snapshot(&mut runtime, &finalized),
        (Some(Value::number(42.0)), false, false)
    );
}

#[test]
fn rejection_chains_and_self_resolution_tracks_handling() {
    let mut runtime = runtime();
    let (source, chained, recovered, finalized) =
        runtime.with_scope(runtime.default_realm(), |ctx| {
            let ctor = builtin(ctx, "Promise");
            let record = invoke(ctx, ctor, "withResolvers", &[]).unwrap();
            let source = get_named(ctx, record, "promise").unwrap();
            let resolve = get_named(ctx, record, "resolve").unwrap();
            let reject = get_named(ctx, record, "reject").unwrap();
            one(ctx, resolve, source).unwrap();
            let value = ctx.root(Value::int32(41)).unwrap();
            one(ctx, reject, value).unwrap();
            let state = promise_data(ctx, ctx.get(source));
            assert!(state.rejected && !state.is_handled);
            let ObjectData::Error(data) = &ctx
                .heap()
                .object(state.result.unwrap().as_object().unwrap())
                .data
            else {
                panic!("expected TypeError")
            };
            assert_eq!(data.kind, ErrorKind::Type);
            let source2 = invoke(ctx, ctor, "reject", &[value]).unwrap();
            let chained = invoke(ctx, source2, "then", &[]).unwrap();
            let handler = ctx.new_native(TIMES_TWO, false).unwrap();
            let recovered = invoke(ctx, chained, "catch", &[handler]).unwrap();
            let cleanup = ctx.new_native(CLEANUP, false).unwrap();
            let finalized = invoke(ctx, chained, "finally", &[cleanup]).unwrap();
            (
                ctx.persist(source).unwrap(),
                ctx.persist(chained).unwrap(),
                ctx.persist(recovered).unwrap(),
                ctx.persist(finalized).unwrap(),
            )
        });
    runtime.run_jobs().unwrap();
    assert!(snapshot(&mut runtime, &source).1);
    assert_eq!(
        snapshot(&mut runtime, &chained),
        (Some(Value::int32(41)), true, true)
    );
    assert_eq!(
        snapshot(&mut runtime, &recovered),
        (Some(Value::number(82.0)), false, false)
    );
    assert_eq!(
        snapshot(&mut runtime, &finalized),
        (Some(Value::int32(41)), true, false)
    );
}

fn subclass<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let ctor = builtin(ctx, "Promise");
    let target = ctx.root(Value::object(ctx.new_target().unwrap()))?;
    construct(ctx, ctor, args, target)
}

fn species_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    assert_ne!(ctx.get(this), Value::object(ctx.realm().promise_ctor));
    Ok(Value::object(ctx.realm().promise_ctor))
}

fn make_subclass<'s>(ctx: &mut ExecCtx<'_, 's>) -> Handle<'s> {
    let promise_ctor = builtin(ctx, "Promise");
    let promise_proto = ctx.root(Value::object(ctx.realm().promise_proto)).unwrap();
    let ctor = ctx.new_native(SUBCLASS, true).unwrap();
    let proto = ctx.new_object_with_proto(Some(promise_proto)).unwrap();
    set_prototype_of(ctx, ctor, Some(promise_ctor)).unwrap();
    data(ctx, ctor, "prototype", proto, PropFlags::default()).unwrap();
    data(ctx, proto, "constructor", ctor, mutable()).unwrap();
    ctor
}

#[test]
fn promise_requires_new_and_retains_subclass_prototype_and_species() {
    let mut runtime = runtime();
    let result = runtime.with_scope(runtime.default_realm(), |ctx| {
        let ctor = builtin(ctx, "Promise");
        let callback = ctx.new_native(TIMES_TWO, false).unwrap();
        assert!(call_with(ctx, ctor, ctor, &[callback]).is_err());
        assert_kind(ctx, ErrorKind::Type);
        let tool_call = make_subclass(ctx);
        let get = ctx.new_native(SPECIES_GETTER, false).unwrap();
        let species = well_known(ctx, WellKnown::Species);
        define_own_property(
            ctx,
            tool_call,
            species,
            PropertyDescriptor {
                get: Some(get),
                ..PropertyDescriptor::default()
            },
        )
        .unwrap();
        let value = ctx.root(Value::int32(7)).unwrap();
        let instance = invoke(ctx, tool_call, "resolve", &[value]).unwrap();
        let proto = get_named(ctx, tool_call, "prototype").unwrap();
        assert_eq!(
            get_prototype_of(ctx, instance).unwrap(),
            ctx.get(proto).as_object()
        );
        let ctor_of_instance = get_named(ctx, instance, "constructor").unwrap();
        assert_eq!(ctx.get(ctor_of_instance), ctx.get(tool_call));
        let same = invoke(ctx, tool_call, "resolve", &[instance]).unwrap();
        assert_eq!(ctx.get(same), ctx.get(instance));
        let result = invoke(ctx, instance, "then", &[callback]).unwrap();
        assert_eq!(
            get_prototype_of(ctx, result).unwrap(),
            Some(ctx.realm().promise_proto)
        );
        ctx.persist(result).unwrap()
    });
    runtime.run_jobs().unwrap();
    assert_eq!(snapshot(&mut runtime, &result).0, Some(Value::number(14.0)));
}

fn iterator<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    Ok(ctx.get(this))
}

fn next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let index = get_named(ctx, this, "index")?;
    let index = ctx.get(index).as_number().unwrap() as u64;
    let values = get_named(ctx, this, "items")?;
    let length = get(ctx, values, LENGTH)?;
    let done = index >= ctx.get(length).as_number().unwrap() as u64;
    let result = ctx.new_object()?;
    let done_value = ctx.root(Value::bool(done))?;
    data(ctx, result, "done", done_value, DATA)?;
    if !done {
        let step = ctx.root(Value::number(index as f64 + 1.0))?;
        data(ctx, this, "index", step, DATA)?;
        let value = get_index(ctx, values, index)?;
        data(ctx, result, "value", value, DATA)?;
    }
    Ok(ctx.get(result))
}

fn close<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    EVENTS.with(|events| events.borrow_mut().push(8));
    let reason = ctx.root(Value::int32(99))?;
    Err(ctx.throw(reason))
}

fn throw_resolve<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let reason = ctx.root(Value::int32(66))?;
    Err(ctx.throw(reason))
}

fn resolve_identity<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    Ok(ctx.get(args.get(0).unwrap()))
}

fn both<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let value = ctx.root(Value::int32(1))?;
    let reason = ctx.root(Value::int32(2))?;
    one(ctx, args.get(0).unwrap(), value)?;
    one(ctx, args.get(1).unwrap(), reason)
}

fn iterable<'s>(ctx: &mut ExecCtx<'_, 's>, items: &[Handle<'s>]) -> Handle<'s> {
    let object = ctx.new_object().unwrap();
    let method = ctx.new_native(ITERATOR, false).unwrap();
    let key = well_known(ctx, WellKnown::Iterator);
    super::super::define_data(ctx, object, key, method, DATA).unwrap();
    let next = ctx.new_native(NEXT, false).unwrap();
    let close = ctx.new_native(CLOSE, false).unwrap();
    data(ctx, object, "next", next, DATA).unwrap();
    data(ctx, object, "return", close, DATA).unwrap();
    let zero = ctx.root(Value::int32(0)).unwrap();
    data(ctx, object, "index", zero, DATA).unwrap();
    let array = ctx.new_array().unwrap();
    for (index, &item) in items.iter().enumerate() {
        let key = index_key(ctx, index as u64).unwrap();
        super::super::define_data(ctx, array, key, item, DATA).unwrap();
    }
    data(ctx, object, "items", array, DATA).unwrap();
    object
}

#[test]
fn promise_combinators_wait_and_preserve_iteration_order() {
    let mut runtime = runtime();
    let roots = runtime.with_scope(runtime.default_realm(), |ctx| {
        let ctor = builtin(ctx, "Promise");
        let first = ctx.root(Value::int32(1)).unwrap();
        let two = ctx.root(Value::int32(2)).unwrap();
        let reason = ctx.root(Value::int32(9)).unwrap();
        let record = invoke(ctx, ctor, "withResolvers", &[]).unwrap();
        let deferred = get_named(ctx, record, "promise").unwrap();
        let resolve = get_named(ctx, record, "resolve").unwrap();
        let rejection = invoke(ctx, ctor, "reject", &[reason]).unwrap();
        let roots = [
            ("all", vec![deferred, two]),
            ("allSettled", vec![rejection, two]),
            ("race", vec![two, rejection]),
            ("any", vec![rejection, two]),
            ("any", vec![rejection, rejection]),
            ("all", vec![]),
            ("race", vec![]),
            ("any", vec![]),
        ]
        .into_iter()
        .map(|(method, items)| {
            let iterable = iterable(ctx, &items);
            let result = invoke(ctx, ctor, method, &[iterable]).unwrap();
            ctx.persist(result).unwrap()
        })
        .collect::<Vec<_>>();
        one(ctx, resolve, first).unwrap();
        roots
    });
    runtime.collect();
    runtime.run_jobs().unwrap();
    for (root, expected) in [
        (&roots[0], "[1,2]"),
        (
            &roots[1],
            r#"[{"status":"rejected","reason":9},{"status":"fulfilled","value":2}]"#,
        ),
        (&roots[5], "[]"),
    ] {
        runtime.with_scope(runtime.default_realm(), |ctx| {
            let promise = ctx.get_persistent(root).unwrap().unwrap();
            let result = promise_data(ctx, ctx.get(promise)).result.unwrap();
            let result = ctx.root(result).unwrap();
            let json = builtin(ctx, "JSON");
            let output = invoke(ctx, json, "stringify", &[result]).unwrap();
            assert_text(ctx, ctx.get(output), expected);
        });
    }
    assert_eq!(
        snapshot(&mut runtime, &roots[2]),
        (Some(Value::int32(2)), false, false)
    );
    assert_eq!(
        snapshot(&mut runtime, &roots[3]),
        (Some(Value::int32(2)), false, false)
    );
    assert_eq!(snapshot(&mut runtime, &roots[6]).0, None);
    for (root, expected) in [(&roots[4], "[9,9]"), (&roots[7], "[]")] {
        runtime.with_scope(runtime.default_realm(), |ctx| {
            let promise = ctx.get_persistent(root).unwrap().unwrap();
            let state = promise_data(ctx, ctx.get(promise));
            assert!(state.rejected);
            let error = state.result.unwrap();
            let ObjectData::Error(data) = &ctx.heap().object(error.as_object().unwrap()).data
            else {
                panic!("expected AggregateError")
            };
            assert_eq!(data.kind, ErrorKind::Aggregate);
            let error = ctx.root(error).unwrap();
            let errors = get_named(ctx, error, "errors").unwrap();
            let json = builtin(ctx, "JSON");
            let text = invoke(ctx, json, "stringify", &[errors]).unwrap();
            assert_text(ctx, ctx.get(text), expected);
        });
    }
}

#[test]
fn promise_combinator_closes_iterator_and_ignores_double_callback() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let ctor = make_subclass(ctx);
        let method = ctx.new_native(THROW_RESOLVE, false).unwrap();
        data(ctx, ctor, "resolve", method, mutable()).unwrap();
        let value = ctx.root(Value::int32(1)).unwrap();
        let input = iterable(ctx, &[value]);
        let rejected = invoke(ctx, ctor, "all", &[input]).unwrap();
        let state = promise_data(ctx, ctx.get(rejected));
        assert_eq!(state.result, Some(Value::int32(66)));
        assert!(state.rejected);
        assert_eq!(EVENTS.with(|events| events.borrow().clone()), vec![8]);

        let method = ctx.new_native(RESOLVE_IDENTITY, false).unwrap();
        data(ctx, ctor, "resolve", method, mutable()).unwrap();
        let thenable = ctx.new_object().unwrap();
        let then = ctx.new_native(BOTH, false).unwrap();
        data(ctx, thenable, "then", then, DATA).unwrap();
        let input = iterable(ctx, &[thenable]);
        let result = invoke(ctx, ctor, "allSettled", &[input]).unwrap();
        let state = promise_data(ctx, ctx.get(result));
        assert!(!state.rejected);
        let array = ctx.root(state.result.unwrap()).unwrap();
        let json = builtin(ctx, "JSON");
        let output = invoke(ctx, json, "stringify", &[array]).unwrap();
        assert_text(
            ctx,
            ctx.get(output),
            r#"[{"status":"fulfilled","value":1}]"#,
        );
    });
}

#[test]
fn promise_try_forwards_arguments_and_rejects_throw() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let ctor = builtin(ctx, "Promise");
        let callback = ctx.new_native(TIMES_TWO, false).unwrap();
        let value = ctx.root(Value::int32(12)).unwrap();
        let result = invoke(ctx, ctor, "try", &[callback, value]).unwrap();
        assert_eq!(
            promise_data(ctx, ctx.get(result)).result,
            Some(Value::number(24.0))
        );
        assert_eq!(EVENTS.with(|events| events.borrow().clone()), vec![3]);
        let callback = ctx.new_native(THROW_RESOLVE, false).unwrap();
        let result = invoke(ctx, ctor, "try", &[callback]).unwrap();
        let state = promise_data(ctx, ctx.get(result));
        assert!(state.rejected);
        assert_eq!(state.result, Some(Value::int32(66)));
        let result = invoke(ctx, ctor, "try", &[]).unwrap();
        assert!(promise_data(ctx, ctx.get(result)).rejected);
    });
}

fn reviver<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(args.len(), 3);
    let name = text(ctx, ctx.get(args.get(0).unwrap()));
    let context = args.get(2).unwrap();
    let source = get_named(ctx, context, "source")?;
    let source = (ctx.get(source) != Value::UNDEFINED).then(|| text(ctx, ctx.get(source)));
    if name == "a" {
        let replacement = ctx.new_object()?;
        let two = ctx.root(Value::int32(2))?;
        data(ctx, replacement, "d", two, DATA)?;
        data(ctx, this, "c", replacement, DATA)?;
    }
    if name == "gone" {
        return Ok(Value::UNDEFINED);
    }
    SOURCES.with(|sources| sources.borrow_mut().push((name, source)));
    Ok(ctx.get(args.get(1).unwrap()))
}

#[test]
fn parse_strict_utf16_numbers_and_reviver_sources() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let json = builtin(ctx, "JSON");
        for bad in ["01", "[1,]", "{\"a\":1,}", "1.", "1e+", "+1", "NaN", "\u{feff}null", "\"\n\"", "\"\\x00\"", "true false"] {
            let input = ctx.new_string(bad).unwrap();
            assert!(invoke(ctx, json, "parse", &[input]).is_err(), "accepted {bad:?}");
            assert_kind(ctx, ErrorKind::Syntax);
        }
        let input = ctx.new_string(r#" {"a": 1, "a": 1e-02, "b": [-0,1e999,"\ud800"], "c": {"d":2}, "gone":0, "__proto__":true} "#).unwrap();
        let reviver = ctx.new_native(REVIVER, false).unwrap();
        let object = invoke(ctx, json, "parse", &[input, reviver]).unwrap();
        let expected = [("a", Some("1e-02")), ("0", Some("-0")), ("1", Some("1e999")), ("2", Some(r#""\ud800""#)), ("b", None), ("d", None), ("c", None), ("__proto__", Some("true")), ("", None)];
        SOURCES.with(|sources| {
            assert_eq!(sources.borrow().iter().map(|(name, source)| (name.as_str(), source.as_deref())).collect::<Vec<_>>(), expected);
        });
        let array = get_named(ctx, object, "b").unwrap();
        let zero = get_index(ctx, array, 0).unwrap();
        assert_eq!(ctx.get(zero).as_number().unwrap().to_bits(), (-0.0f64).to_bits());
        let infinity = get_index(ctx, array, 1).unwrap();
        assert_eq!(ctx.get(infinity).as_number(), Some(f64::INFINITY));
        let lone = get_index(ctx, array, 2).unwrap();
        let ValueKind::String(lone) = ctx.get(lone).decode() else { panic!("expected string") };
        assert!(ctx.heap().string_units(lone).eq([0xd800]));
        let prop = get_named(ctx, object, "__proto__").unwrap();
        assert_eq!(ctx.get(prop), Value::bool(true));
        let gone = get_named(ctx, object, "gone").unwrap();
        assert_eq!(ctx.get(gone), Value::UNDEFINED);
        assert_eq!(get_prototype_of(ctx, object).unwrap(), Some(ctx.realm().object_proto));
    });
}

fn replacer<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(args.len(), 2);
    assert!(ctx.get(this).as_object().is_some());
    let key = text(ctx, ctx.get(args.get(0).unwrap()));
    if key == "omit" {
        return Ok(Value::UNDEFINED);
    }
    if key == "hook" {
        assert_eq!(ctx.get(args.get(1).unwrap()), Value::int32(8));
    }
    Ok(ctx.get(args.get(1).unwrap()))
}

fn to_json<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_text(ctx, ctx.get(args.get(0).unwrap()), "hook");
    Ok(Value::int32(8))
}

#[test]
fn stringify_replacer_gap_to_json_raw_json_and_surrogates() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let json = builtin(ctx, "JSON");
        let raw_text = ctx.new_string("1e+002").unwrap();
        let raw = invoke(ctx, json, "rawJSON", &[raw_text]).unwrap();
        assert_eq!(get_prototype_of(ctx, raw).unwrap(), None);
        assert!(!is_extensible(ctx, raw).unwrap());
        let key = ctx.intern_str("rawJSON").unwrap();
        let desc = get_own_property_descriptor(ctx, raw, key).unwrap().unwrap();
        assert_eq!((desc.writable, desc.configurable, desc.enumerable), (Some(false), Some(false), Some(true)));
        let is_raw = invoke(ctx, json, "isRawJSON", &[raw]).unwrap();
        assert_eq!(ctx.get(is_raw), Value::bool(true));
        for bad in ["{}", "[]", " 0", "0 ", "0\n"] {
            let input = ctx.new_string(bad).unwrap();
            assert!(invoke(ctx, json, "rawJSON", &[input]).is_err());
            assert_kind(ctx, ErrorKind::Syntax);
        }
        let object = ctx.new_object().unwrap();
        data(ctx, object, "raw", raw, DATA).unwrap();
        let lone = ctx.new_utf16(&[0xdc00, 0xd83d, 0xde00]).unwrap();
        data(ctx, object, "lone", lone, DATA).unwrap();
        let hook = ctx.new_object().unwrap();
        let to_json = ctx.new_native(TO_JSON, false).unwrap();
        data(ctx, hook, "toJSON", to_json, DATA).unwrap();
        data(ctx, object, "hook", hook, DATA).unwrap();
        data(ctx, object, "omit", hook, DATA).unwrap();
        let undefined = ctx.root(Value::UNDEFINED).unwrap();
        data(ctx, object, "omit", undefined, DATA).unwrap();
        let replacer = ctx.new_native(REPLACER, false).unwrap();
        let space = ctx.new_string("1234567890extra").unwrap();
        let output = invoke(ctx, json, "stringify", &[object, replacer, space]).unwrap();
        assert_text(ctx, ctx.get(output), "{\n1234567890\"raw\": 1e+002,\n1234567890\"lone\": \"\\udc00😀\",\n1234567890\"hook\": 8\n}");

        let list = ctx.new_array().unwrap();
        for (index, property) in ["lone", "raw", "lone"].iter().enumerate() {
            let name = ctx.new_string(property).unwrap();
            let boxed = to_object(ctx, name).unwrap();
            let key = index_key(ctx, index as u64).unwrap();
            super::super::define_data(ctx, list, key, boxed, DATA).unwrap();
        }
        let space = ctx.root(Value::int32(2)).unwrap();
        let space = to_object(ctx, space).unwrap();
        let output = invoke(ctx, json, "stringify", &[object, list, space]).unwrap();
        assert_text(ctx, ctx.get(output), "{\n  \"lone\": \"\\udc00😀\",\n  \"raw\": 1e+002\n}");
        data(ctx, object, "cycle", object, DATA).unwrap();
        assert!(invoke(ctx, json, "stringify", &[object]).is_err());
        assert_kind(ctx, ErrorKind::Type);
        let bigint = ctx.root(Value::short_bigint(2).unwrap()).unwrap();
        assert!(invoke(ctx, json, "stringify", &[bigint]).is_err());
        assert_kind(ctx, ErrorKind::Type);
    });
}

#[test]
fn json_traversals_check_stack_heap_and_interrupt_limits() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let json = builtin(ctx, "JSON");
        let sample = ctx.new_string("\"warm\"").unwrap();
        invoke(ctx, json, "parse", &[sample]).unwrap();
        invoke(ctx, json, "stringify", &[sample]).unwrap();
        let large = ctx.new_string(&"x".repeat(20_000)).unwrap();
        let stringify = get_named(ctx, json, "stringify").unwrap();
        let args = ctx.args(&[large]).unwrap();
        let ledger = ctx.heap().ledger.clone();
        ledger.fail_after(Some(1));
        assert_eq!(call(ctx, stringify, json, args), Err(Abrupt::Thrown));
        assert!(ctx.is_oom());
        ledger.fail_after(None);
        ctx.take_exception();
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        ctx.runtime.set_interrupt_handler(Some(Box::new(move || {
            count.set(count.get() + 1);
            count.get() == 5
        })));
        assert_eq!(call(ctx, stringify, json, args), Err(Abrupt::Interrupted));
        assert_eq!(calls.get(), 5);
        assert!(ctx.pending_exception().is_none());
        ctx.runtime.set_interrupt_handler(None);
        let valid = ctx
            .new_string(&format!("{}0{}", "[".repeat(40), "]".repeat(40)))
            .unwrap();
        let former_limit = ctx.runtime.vm.limits.stack_bytes;
        ctx.runtime.vm.limits.stack_bytes = 16 * 1024;
        assert!(invoke(ctx, json, "parse", &[valid]).is_err());
        assert_kind(ctx, ErrorKind::Range);
        ctx.runtime.vm.limits.stack_bytes = former_limit;
        invoke(ctx, json, "parse", &[valid]).unwrap();
    });
}

#[test]
fn domexception_slots_legacy_constants_and_base64() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let ctor = builtin(ctx, "DOMException");
        let message = ctx.new_string("test").unwrap();
        let name = ctx.new_string("HierarchyRequestError").unwrap();
        let args = ctx.args(&[message, name]).unwrap();
        assert!(call(ctx, ctor, ctor, args).is_err());
        assert_kind(ctx, ErrorKind::Type);
        let instance = construct(ctx, ctor, args, ctor).unwrap();
        let instance = ctx.root(instance).unwrap();
        let proto = get_prototype_of(ctx, instance).unwrap().unwrap();
        let proto = ctx.root(Value::object(proto)).unwrap();
        assert_eq!(
            get_prototype_of(ctx, proto).unwrap(),
            Some(ctx.realm().error_proto)
        );
        for name in ["name", "message", "code"] {
            let key = ctx.intern_str(name).unwrap();
            let desc = get_own_property_descriptor(ctx, proto, key)
                .unwrap()
                .unwrap();
            assert_eq!(
                (desc.enumerable, desc.configurable, desc.set),
                (Some(true), Some(true), Some(Value::UNDEFINED))
            );
            assert!(callable(ctx, desc.get.unwrap()));
        }
        let code = get_named(ctx, instance, "code").unwrap();
        assert_eq!(ctx.get(code), Value::int32(3));
        let code = get_named(ctx, ctor, "HIERARCHY_REQUEST_ERR").unwrap();
        assert_eq!(ctx.get(code), Value::int32(3));
        let code = get_named(ctx, proto, "DATA_CLONE_ERR").unwrap();
        assert_eq!(ctx.get(code), Value::int32(25));
        let own = own_property_keys(ctx, instance).unwrap();
        assert_eq!(own.len(), 1);
        let key = crate::ops::convert::key_value(ctx, own[0]).unwrap();
        assert_text(ctx, ctx.get(key), "stack");

        let global = global(ctx).unwrap();
        let binary = ctx.new_string("\0éÿ").unwrap();
        let encoded = invoke(ctx, global, "btoa", &[binary]).unwrap();
        assert_text(ctx, ctx.get(encoded), "AOn/");
        let decoded = invoke(ctx, global, "atob", &[encoded]).unwrap();
        assert_text(ctx, ctx.get(decoded), "\0éÿ");
        let unpadded = ctx.new_string("6 Q\t\n\r\u{c}").unwrap();
        let decoded = invoke(ctx, global, "atob", &[unpadded]).unwrap();
        assert_text(ctx, ctx.get(decoded), "é");
        for (method, payload) in [("atob", "AA="), ("atob", "AA\u{b}=="), ("atob", "A===")]
            .into_iter()
            .chain([("btoa", "Ā")])
        {
            let payload = ctx.new_string(payload).unwrap();
            assert!(invoke(ctx, global, method, &[payload]).is_err());
            let error = ctx.take_exception().unwrap().0;
            let error = ctx.root(error).unwrap();
            let name = get_named(ctx, error, "name").unwrap();
            assert_text(ctx, ctx.get(name), "InvalidCharacterError");
            let code = get_named(ctx, error, "code").unwrap();
            assert_eq!(ctx.get(code), Value::int32(5));
        }
        assert!(invoke(ctx, global, "queueMicrotask", &[]).is_err());
        assert_kind(ctx, ErrorKind::Type);
    });
}

fn realm_then<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let resolve = args.get(0).unwrap();
    assert_eq!(
        get_prototype_of(ctx, resolve)?,
        Some(ctx.realm().function_proto)
    );
    let result = ctx.root(Value::int32(17))?;
    one(ctx, resolve, result)
}

#[test]
fn thenable_job_creates_resolvers_in_the_then_methods_realm() {
    let mut runtime = runtime();
    let other = runtime.create_realm().unwrap();
    runtime.install_builtins(other).unwrap();
    let thenable = runtime.with_scope(other, |ctx| {
        let thenable = ctx.new_object().unwrap();
        let then = ctx.new_native(REALM_THEN, false).unwrap();
        data(ctx, thenable, "then", then, DATA).unwrap();
        ctx.persist(thenable).unwrap()
    });
    let promise = runtime.with_scope(runtime.default_realm(), |ctx| {
        let thenable = ctx.get_persistent(&thenable).unwrap().unwrap();
        let ctor = builtin(ctx, "Promise");
        let promise = invoke(ctx, ctor, "resolve", &[thenable]).unwrap();
        ctx.persist(promise).unwrap()
    });
    runtime.release_persistent(thenable);
    runtime.run_jobs().unwrap();
    assert_eq!(snapshot(&mut runtime, &promise).0, Some(Value::int32(17)));
}

fn reenter_json<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let json = builtin(ctx, "JSON");
    let redirect = get_named(ctx, this, "redirect")?;
    let string = invoke(ctx, json, "stringify", &[redirect])?;
    Ok(ctx.get(string))
}

fn then_getter_oom<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    EVENTS.with(|events| events.borrow_mut().push(5));
    Err(ctx.throw_oom())
}

#[test]
fn then_getter_runs_synchronously_and_its_oom_rejects_the_promise() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let thenable = ctx.new_object().unwrap();
        let get = ctx.new_native(THEN_GETTER_OOM, false).unwrap();
        let key = ctx.intern_str("then").unwrap();
        define_own_property(
            ctx,
            thenable,
            key,
            PropertyDescriptor {
                get: Some(get),
                ..PropertyDescriptor::default()
            },
        )
        .unwrap();
        let ctor = builtin(ctx, "Promise");
        let result = invoke(ctx, ctor, "resolve", &[thenable]);
        assert_eq!(EVENTS.with(|events| events.borrow().clone()), vec![5]);
        let result = result.unwrap();
        assert!(!ctx.is_oom());
        let data = promise_data(ctx, ctx.get(result));
        assert!(data.rejected);
        assert_eq!(
            data.result,
            Some(Value::object(ctx.realm().oom_error.object))
        );
    });
    assert_eq!(runtime.queued_jobs(), 0);
}

#[test]
fn nested_to_json_calls_share_the_stack_budget() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let json = builtin(ctx, "JSON");
        let mut value = ctx.root(Value::NULL).unwrap();
        for _ in 0..3 {
            let redirect = ctx.new_object().unwrap();
            data(ctx, redirect, "redirect", value, DATA).unwrap();
            let method = ctx.new_native(REENTER_JSON, false).unwrap();
            data(ctx, redirect, "toJSON", method, DATA).unwrap();
            value = redirect;
            for _ in 0..7 {
                let array = ctx.new_array().unwrap();
                let key = index_key(ctx, 0).unwrap();
                super::super::define_data(ctx, array, key, value, DATA).unwrap();
                value = array;
            }
        }
        let original = ctx.runtime.vm.limits.stack_bytes;
        ctx.runtime.vm.limits.stack_bytes = 40 * 1024;
        assert!(invoke(ctx, json, "stringify", &[value]).is_err());
        assert_kind(ctx, ErrorKind::Range);
        ctx.runtime.vm.limits.stack_bytes = original;
        invoke(ctx, json, "stringify", &[value]).unwrap();
    });
}

#[test]
fn performance_and_globals_have_expected_surface() {
    let mut runtime = runtime();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let global = global(ctx).unwrap();
        for absent in [
            "console",
            "print",
            "std",
            "os",
            "setTimeout",
            "fetch",
            "WebAssembly",
        ] {
            let key = ctx.intern_str(absent).unwrap();
            assert!(
                get_own_property_descriptor(ctx, global, key)
                    .unwrap()
                    .is_none()
            );
        }
        for name in [
            "Promise",
            "JSON",
            "DOMException",
            "performance",
            "atob",
            "btoa",
            "queueMicrotask",
        ] {
            let key = ctx.intern_str(name).unwrap();
            assert!(
                get_own_property_descriptor(ctx, global, key)
                    .unwrap()
                    .is_some()
            );
        }
        let performance = builtin(ctx, "performance");
        let begin = invoke(ctx, performance, "now", &[]).unwrap();
        let later = invoke(ctx, performance, "now", &[]).unwrap();
        let begin = ctx.get(begin).as_number().unwrap();
        let later = ctx.get(later).as_number().unwrap();
        assert!(begin >= 0.0 && later >= begin);
        let origin = get_named(ctx, performance, "timeOrigin").unwrap();
        let origin = ctx.get(origin).as_number().unwrap();
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            * 1e3;
        assert!(origin > 1.0e12 && origin <= wall);
        assert_eq!(origin, ctx.realm().time_origin_epoch_ms);
    });
}
