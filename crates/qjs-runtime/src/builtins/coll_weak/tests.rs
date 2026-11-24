use std::cell::RefCell;

use super::*;
use crate::atom::Atom;
use crate::heap::BigIntData;
use crate::ledger::TrackedVec;
use crate::object::{ErrorKind, ObjectData};
use crate::ops::property::{
    PropertyDescriptor, define_own_property, get_own_property_descriptor, get_prototype_of,
};
use crate::realm::NativeFn;
use crate::value::{Abrupt, ValueKind};
use crate::{EngineLimits, Runtime};

thread_local! {
    static EVENTS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    static VISITED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

fn event(name: &'static str) {
    EVENTS.with_borrow_mut(|events| events.push(name));
}

fn events() -> Vec<&'static str> {
    EVENTS.with_borrow_mut(std::mem::take)
}

fn runtime() -> Runtime {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&CALLBACKS);
    runtime.install_builtins(runtime.default_realm()).unwrap();
    runtime
}

fn native<'s>(ctx: &mut ExecCtx<'_, 's>, id: u16) -> Handle<'s> {
    ctx.new_native(NativeFnId(id), false).unwrap()
}

fn named<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, name: &str) -> Handle<'s> {
    let key = ctx.intern_str(name).unwrap();
    get(ctx, object, key).unwrap()
}

fn global<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str) -> Handle<'s> {
    let object = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
    named(ctx, object, name)
}

fn construct<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    name: &str,
    arguments: &[Handle<'s>],
) -> JsResult<Handle<'s>> {
    let ctor = global(ctx, name);
    let args = ctx.args(arguments)?;
    let value = call::construct(ctx, ctor, args, ctor)?;
    ctx.root(value)
}

fn invoke<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    name: &str,
    arguments: &[Handle<'s>],
) -> JsResult<Value> {
    let method = named(ctx, this, name);
    let args = ctx.args(arguments)?;
    call::call(ctx, method, this, args)
}

fn number<'s>(ctx: &mut ExecCtx<'_, 's>, n: i32) -> Handle<'s> {
    ctx.root(Value::int32(n)).unwrap()
}

fn text(ctx: &ExecCtx<'_, '_>, value: Value) -> Vec<u16> {
    let ValueKind::String(id) = value.decode() else {
        panic!("expected a string, got {value:?}");
    };
    ctx.heap().string_units(id).collect()
}

fn prop<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, name: &str, value: Handle<'s>) {
    let key = ctx.intern_str(name).unwrap();
    define_data(ctx, object, key, value, PropFlags::DEFAULT_DATA).unwrap();
}

fn getter<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, key: PropKey, get: Handle<'s>) {
    assert!(
        define_own_property(
            ctx,
            object,
            key,
            PropertyDescriptor {
                get: Some(get),
                configurable: Some(true),
                ..PropertyDescriptor::default()
            }
        )
        .unwrap()
    );
}

fn assert_error(ctx: &mut ExecCtx<'_, '_>, result: JsResult<impl Sized>, kind: ErrorKind) {
    assert_eq!(result.err(), Some(Abrupt::Thrown));
    let (value, _) = ctx.take_exception().unwrap();
    let ObjectData::Error(data) = &ctx.heap().object(value.as_object().unwrap()).data else {
        panic!("expected an error");
    };
    assert_eq!(data.kind, kind);
}

fn next_result<'s>(ctx: &mut ExecCtx<'_, 's>, iter: Handle<'s>) -> (Value, bool) {
    let result = invoke(ctx, iter, "next", &[]).unwrap();
    let result = ctx.root(result).unwrap();
    let value = property::get_property(ctx, result, Atom::get("value")).unwrap();
    let done = property::get_property(ctx, result, Atom::get("done")).unwrap();
    (value, done == Value::bool(true))
}

fn sequence<'s>(ctx: &mut ExecCtx<'_, 's>, collection: Handle<'s>) -> Vec<i32> {
    let result = invoke(ctx, collection, "keys", &[]).unwrap();
    let result = ctx.root(result).unwrap();
    let result = ctx.get(result);
    let mut values = Vec::new();
    loop {
        let (value, done) = ctx.with_scope(|ctx| {
            let iter = ctx.root(result).unwrap();
            next_result(ctx, iter)
        });
        if done {
            return values;
        }
        values.push(value.as_number().unwrap() as i32);
    }
}

#[test]
fn map_uses_same_value_zero_for_number_string_and_bigint_keys() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let map = construct(ctx, "Map", &[]).unwrap();
        let zero = number(ctx, 0);
        let negative_zero = ctx.root(Value::number(-0.0)).unwrap();
        let value = ctx.new_string("value").unwrap();
        assert_eq!(
            invoke(ctx, map, "set", &[negative_zero, value]).unwrap(),
            ctx.get(map)
        );
        assert_eq!(invoke(ctx, map, "get", &[zero]).unwrap(), ctx.get(value));
        let iter = invoke(ctx, map, "keys", &[]).unwrap();
        let iter = ctx.root(iter).unwrap();
        let (key, done) = next_result(ctx, iter);
        assert!(!done);
        assert_eq!(key.as_number().unwrap().to_bits(), 0);
        let nan = ctx.root(Value::number(f64::NAN)).unwrap();
        invoke(ctx, map, "set", &[nan, value]).unwrap();
        assert_eq!(invoke(ctx, map, "get", &[nan]).unwrap(), ctx.get(value));
        let flat = ctx.new_utf16(&[0xd800, 42]).unwrap();
        let left = ctx.new_utf16(&[0xd800]).unwrap();
        let right = ctx.new_string("*").unwrap();
        let rope = ctx.concat_strings(left, right).unwrap();
        invoke(ctx, map, "set", &[flat, value]).unwrap();
        assert_eq!(invoke(ctx, map, "get", &[rope]).unwrap(), ctx.get(value));
        let big = ctx
            .allocate(|runtime| {
                let heap = &mut runtime.heap;
                let limbs = TrackedVec::from_slice(&heap.ledger, &[9, 0])?;
                heap.bigints.insert(
                    &heap.ledger,
                    BigIntData {
                        negative: true,
                        limbs: limbs.into(),
                    },
                )
            })
            .unwrap();
        let big = ctx.root(Value::bigint(big)).unwrap();
        let short = ctx.root(Value::short_bigint(-9).unwrap()).unwrap();
        invoke(ctx, map, "set", &[big, value]).unwrap();
        assert_eq!(invoke(ctx, map, "get", &[short]).unwrap(), ctx.get(value));
        let size = named(ctx, map, "size");
        assert_eq!(ctx.get(size).as_number(), Some(4.0));
        let entries = named(ctx, map, "entries");
        let alias = get(ctx, map, wk(ctx, WellKnown::Iterator)).unwrap();
        assert_eq!(ctx.get(entries), ctx.get(alias));
        let map_ctor = global(ctx, "Map");
        let species = get(ctx, map_ctor, wk(ctx, WellKnown::Species)).unwrap();
        assert_eq!(ctx.get(species), ctx.get(map_ctor));
    });
}

#[test]
fn iterators_survive_compaction_and_clear_but_stay_done() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        for name in ["Map", "Set"] {
            let object = construct(ctx, name, &[]).unwrap();
            let raw = ctx.get(object);
            let is_set = name == "Set";
            let add = if is_set { "add" } else { "set" };
            for n in 0..10 {
                ctx.with_scope(|ctx| {
                    let object = ctx.root(raw).unwrap();
                    let key = number(ctx, n);
                    invoke(ctx, object, add, &[key]).unwrap();
                });
            }
            let iter = invoke(ctx, object, "keys", &[]).unwrap();
            let iter = ctx.root(iter).unwrap();
            let proto = if is_set {
                ctx.realm().set_iterator_proto
            } else {
                ctx.realm().map_iterator_proto
            };
            assert_eq!(get_prototype_of(ctx, iter).unwrap(), Some(proto));
            assert_eq!(
                ctx.heap().shape(ctx.heap().object(proto).shape).proto,
                Some(ctx.realm().iterator_proto)
            );
            assert_eq!(next_result(ctx, iter), (Value::int32(0), false));
            for n in 0..9 {
                ctx.with_scope(|ctx| {
                    let object = ctx.root(raw).unwrap();
                    let key = number(ctx, n);
                    assert_eq!(
                        invoke(ctx, object, "delete", &[key]).unwrap(),
                        Value::bool(true)
                    );
                });
            }
            let zero = number(ctx, 0);
            invoke(ctx, object, add, &[zero]).unwrap();
            assert_eq!(next_result(ctx, iter), (Value::int32(9), false));
            invoke(ctx, object, "clear", &[]).unwrap();
            invoke(ctx, object, add, &[zero]).unwrap();
            assert_eq!(next_result(ctx, iter), (Value::int32(0), false));
            assert_eq!(next_result(ctx, iter), (Value::UNDEFINED, true));
            let another = number(ctx, 2);
            invoke(ctx, object, add, &[another]).unwrap();
            assert_eq!(next_result(ctx, iter), (Value::UNDEFINED, true));
        }
    });
}

fn visit<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::bool(true));
    let [value, key, collection] = [0, 1, 2].map(|i| args.get(i).unwrap());
    let k = ctx.get(key).as_number().unwrap() as i32;
    let first = VISITED.with_borrow_mut(|seen| {
        let first = seen.is_empty();
        seen.push(k);
        first
    });
    let is_set = matches!(
        ctx.heap()
            .object(ctx.get(collection).as_object().unwrap())
            .data,
        ObjectData::Set(_)
    );
    assert_eq!(
        ctx.get(value).as_number().unwrap() as i32,
        if is_set {
            k
        } else if k == 1 && !first {
            111
        } else {
            k * 11
        }
    );
    if first {
        let two = number(ctx, 2);
        invoke(ctx, collection, "delete", &[key])?;
        invoke(ctx, collection, "delete", &[two])?;
        for (k, v) in [(4, 44), (1, 111)] {
            let k = number(ctx, k);
            let v = number(ctx, v);
            invoke(ctx, collection, if is_set { "add" } else { "set" }, &[k, v])?;
        }
    }
    Ok(Value::UNDEFINED)
}

#[test]
fn for_each_visits_new_and_reinserted_keys_in_order() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        for (name, add) in [("Map", "set"), ("Set", "add")] {
            let collection = construct(ctx, name, &[]).unwrap();
            for i in 1..4 {
                let key = number(ctx, i);
                let value = number(ctx, i * 11);
                invoke(ctx, collection, add, &[key, value]).unwrap();
            }
            let callback = native(ctx, 0);
            let this_arg = ctx.root(Value::bool(true)).unwrap();
            assert_eq!(
                invoke(ctx, collection, "forEach", &[callback, this_arg]).unwrap(),
                Value::UNDEFINED
            );
            assert_eq!(VISITED.with_borrow_mut(std::mem::take), [1, 3, 4, 1]);
        }
    });
}

fn next<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("next");
    let index = named(ctx, this, "index");
    let index = ctx.get(index).as_number().unwrap() as u32;
    let values = named(ctx, this, "items");
    let len = named(ctx, values, "length");
    let done = index >= ctx.get(len).as_number().unwrap() as u32;
    let item = if done {
        Value::UNDEFINED
    } else {
        let next = number(ctx, (index + 1) as i32);
        prop(ctx, this, "index", next);
        property::get_property(ctx, values, PropKey::index(index).unwrap())?
    };
    iterable::iter_result(ctx, item, done)
}

fn iterator_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    event("iterator");
    let iterator = named(ctx, this, "iterator");
    Ok(ctx.get(iterator))
}

fn close<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("return");
    let error = named(ctx, this, "closeError");
    if ctx.get(error) != Value::UNDEFINED {
        return Err(ctx.throw(error));
    }
    let value = ctx.new_object()?;
    Ok(ctx.get(value))
}

fn adder_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("get adder");
    let adder = named(ctx, this, "adder");
    Ok(ctx.get(adder))
}

fn throw_first<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    event("throw");
    Err(ctx.throw(args.get(0).unwrap()))
}

fn group<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    let index = ctx.get(args.get(1).unwrap()).as_number().unwrap();
    let value = ctx.get(args.get(0).unwrap()).as_number().unwrap();
    assert_eq!(value, index + 1.0);
    Ok(Value::number(if value % 2.0 == 0.0 { 2.0 } else { -0.0 }))
}

fn result_items<'s>(ctx: &mut ExecCtx<'_, 's>, items: &[Handle<'s>]) -> Handle<'s> {
    let values = ctx.new_array().unwrap();
    for (index, item) in items.iter().copied().enumerate() {
        iterable::array_push(ctx, values, index as u32, item).unwrap();
    }
    values
}

fn custom_iterator<'s>(ctx: &mut ExecCtx<'_, 's>, items: &[Handle<'s>]) -> Handle<'s> {
    let iter = ctx.new_object().unwrap();
    let values = result_items(ctx, items);
    let index = number(ctx, 0);
    let next = native(ctx, 1);
    let ret = native(ctx, 3);
    prop(ctx, iter, "items", values);
    prop(ctx, iter, "index", index);
    prop(ctx, iter, "next", next);
    prop(ctx, iter, "return", ret);
    iter
}

fn custom_iterable<'s>(ctx: &mut ExecCtx<'_, 's>, iterator: Handle<'s>) -> Handle<'s> {
    let object = ctx.new_object().unwrap();
    let method = native(ctx, 2);
    define_data(
        ctx,
        object,
        wk(ctx, WellKnown::Iterator),
        method,
        PropFlags::DEFAULT_DATA,
    )
    .unwrap();
    prop(ctx, object, "iterator", iterator);
    object
}

#[test]
fn constructors_get_adder_before_iterator_and_close_on_failure() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        for (name, adder_name) in [
            ("Map", "set"),
            ("Set", "add"),
            ("WeakMap", "set"),
            ("WeakSet", "add"),
        ] {
            let ctor = global(ctx, name);
            let undefined = ctx.root(Value::UNDEFINED).unwrap();
            let result = call::call(ctx, ctor, undefined, Args::empty());
            assert_error(ctx, result, ErrorKind::Type);

            let new_target = ctx.new_native(NativeFnId(0), true).unwrap();
            let proto = ctx.new_object().unwrap();
            prop(ctx, new_target, "prototype", proto);
            let get = native(ctx, 4);
            let adder = native(ctx, 5);
            prop(ctx, proto, "adder", adder);
            getter(ctx, proto, Atom::get(adder_name), get);
            let marker = ctx.new_object().unwrap();
            let competing_error = ctx.new_object().unwrap();
            let item = if adder_name == "add" {
                marker
            } else {
                result_items(ctx, &[marker])
            };
            let iter = custom_iterator(ctx, &[item]);
            prop(ctx, iter, "closeError", competing_error);
            let input = custom_iterable(ctx, iter);
            let args = ctx.args(&[input]).unwrap();
            assert_eq!(
                call::construct(ctx, ctor, args, new_target),
                Err(Abrupt::Thrown)
            );
            assert_eq!(ctx.take_exception().unwrap().0, ctx.get(marker));
            assert_eq!(
                events(),
                ["get adder", "iterator", "next", "throw", "return"]
            );
            let result = call::construct(ctx, ctor, Args::empty(), new_target).unwrap();
            let result = ctx.root(result).unwrap();
            assert_eq!(
                get_prototype_of(ctx, result).unwrap(),
                ctx.get(proto).as_object()
            );
            assert!(events().is_empty());
        }
        let number = number(ctx, 1);
        let iter = custom_iterator(ctx, &[number]);
        let input = custom_iterable(ctx, iter);
        let result = construct(ctx, "Map", &[input]);
        assert_error(ctx, result, ErrorKind::Type);
        assert_eq!(events(), ["iterator", "next", "return"]);
    });
}

#[test]
fn constructors_consume_entries_or_values_and_iterators_return_pairs() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let symbol = ctx.new_symbol(None, false).unwrap();
        let datum = ctx.new_object().unwrap();
        for name in ["Map", "WeakMap", "Set", "WeakSet"] {
            let is_set = name.ends_with("Set");
            let item = if is_set {
                symbol
            } else {
                result_items(ctx, &[symbol, datum])
            };
            let iter = custom_iterator(ctx, &[item]);
            let input = custom_iterable(ctx, iter);
            let collection = construct(ctx, name, &[input]).unwrap();
            assert_eq!(
                invoke(ctx, collection, "has", &[symbol]).unwrap(),
                Value::bool(true)
            );
            assert_eq!(events(), ["iterator", "next", "next"]);
            if !is_set {
                assert_eq!(
                    invoke(ctx, collection, "get", &[symbol]).unwrap(),
                    ctx.get(datum)
                );
            }
            if name.starts_with("Weak") {
                continue;
            }
            let iter = invoke(ctx, collection, "entries", &[]).unwrap();
            let iter = ctx.root(iter).unwrap();
            let (pair, done) = next_result(ctx, iter);
            assert!(!done);
            let pair = ctx.root(pair).unwrap();
            for (i, expected) in [symbol, if is_set { symbol } else { datum }]
                .into_iter()
                .enumerate()
            {
                let got =
                    property::get_property(ctx, pair, PropKey::index(i as u32).unwrap()).unwrap();
                assert_eq!(got, ctx.get(expected));
            }
            assert_eq!(next_result(ctx, iter), (Value::UNDEFINED, true));
        }
    });
}

#[test]
fn map_group_by_uses_callback_index_and_closes_on_throw() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = [1, 2, 3].map(|n| number(ctx, n));
        let iter = custom_iterator(ctx, &input);
        let items = custom_iterable(ctx, iter);
        let callback = native(ctx, 6);
        let map_ctor = global(ctx, "Map");
        let map = invoke(ctx, map_ctor, "groupBy", &[items, callback]).unwrap();
        let map = ctx.root(map).unwrap();
        assert_eq!(sequence(ctx, map), [0, 2]);
        let zero = number(ctx, 0);
        let result = invoke(ctx, map, "get", &[zero]).unwrap();
        let result = ctx.root(result).unwrap();
        for (index, val) in [(0, 1), (1, 3)] {
            let val = property::get_property(ctx, result, PropKey::index(index).unwrap())
                .unwrap()
                .as_number()
                .unwrap()
                == f64::from(val);
            assert!(val);
        }
        assert_eq!(events(), ["iterator", "next", "next", "next", "next"]);
        let iter = custom_iterator(ctx, &input);
        let iterable = custom_iterable(ctx, iter);
        let callback = native(ctx, 5);
        assert_eq!(
            invoke(ctx, map_ctor, "groupBy", &[iterable, callback]),
            Err(Abrupt::Thrown)
        );
        assert_eq!(ctx.take_exception().unwrap().0, ctx.get(input[0]));
        assert_eq!(events(), ["iterator", "next", "throw", "return"]);
    });
}

fn cleanup<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    assert_eq!(
        text(ctx, args.as_slice(ctx)[0]),
        "keep".encode_utf16().collect::<Vec<_>>()
    );
    event("cleanup");
    Ok(Value::UNDEFINED)
}

fn size_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("get size");
    let size = named(ctx, this, "reportedSize");
    Ok(ctx.get(size))
}

fn size_value_of<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("to number");
    let size = named(ctx, this, "size");
    Ok(ctx.get(size))
}

fn has_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("get has");
    let has = named(ctx, this, "selectedHas");
    Ok(ctx.get(has))
}

fn keys_getter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("get keys");
    let keys = named(ctx, this, "selectedKeys");
    Ok(ctx.get(keys))
}

fn has_two<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    event("has");
    let receiver = named(ctx, this, "reportedSize");
    assert!(ctx.get(receiver).as_object().is_some());
    Ok(Value::bool(
        ctx.get(args.get(0).unwrap()).as_number() == Some(2.0),
    ))
}

fn keys<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    event("keys");
    let iter = named(ctx, this, "iterator");
    Ok(ctx.get(iter))
}

static CALLBACKS: [NativeFn; 17] = [
    visit,
    next,
    iterator_method,
    close,
    adder_getter,
    throw_first,
    group,
    cleanup,
    size_getter,
    size_value_of,
    has_getter,
    keys_getter,
    has_two,
    keys,
    insert_during_callback,
    mutating_keys,
    mutating_has,
];

fn set<'s>(ctx: &mut ExecCtx<'_, 's>, items: &[i32]) -> Handle<'s> {
    let set = construct(ctx, "Set", &[]).unwrap();
    for &item in items {
        let key = number(ctx, item);
        invoke(ctx, set, "add", &[key]).unwrap();
    }
    set
}

fn set_like<'s>(ctx: &mut ExecCtx<'_, 's>, raw_size: f64, items: &[i32]) -> Handle<'s> {
    let set = ctx.new_object().unwrap();
    let size = ctx.new_object().unwrap();
    let raw_size = ctx.root(Value::number(raw_size)).unwrap();
    let coerce = native(ctx, 9);
    prop(ctx, size, "valueOf", coerce);
    prop(ctx, size, "size", raw_size);
    prop(ctx, set, "reportedSize", size);
    for (name, getter_id) in [("size", 8), ("has", 10), ("keys", 11)] {
        let get = native(ctx, getter_id);
        getter(ctx, set, Atom::get(name), get);
    }
    let has = native(ctx, 12);
    let keys = native(ctx, 13);
    prop(ctx, set, "selectedHas", has);
    prop(ctx, set, "selectedKeys", keys);
    let items: Vec<_> = items.iter().map(|&n| number(ctx, n)).collect();
    let iter = custom_iterator(ctx, &items);
    prop(ctx, set, "iterator", iter);
    set
}

#[test]
fn set_methods_use_set_record_order_and_both_algorithm_branches() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let base = set(ctx, &[1, 2, 3]);
        for (method, size, expected, behavior) in [
            ("union", -0.5, &[1, 2, 3, 4][..], "keys"),
            ("symmetricDifference", 9.0, &[1, 4][..], "keys"),
            ("intersection", 9.0, &[2][..], "has"),
            ("intersection", 1.0, &[3, 2][..], "keys"),
            ("difference", f64::INFINITY, &[1, 3][..], "has"),
            ("difference", 1.0, &[1][..], "keys"),
        ] {
            let other = set_like(ctx, size, &[3, 2, 4, 4]);
            let result = invoke(ctx, base, method, &[other]).unwrap();
            let result = ctx.root(result).unwrap();
            assert_eq!(sequence(ctx, result), expected, "{method}");
            let observed = events();
            assert_eq!(
                observed[..5],
                ["get size", "to number", "get has", "get keys", behavior]
            );
            assert!(!observed.contains(&if behavior == "keys" { "has" } else { "keys" }));
        }
        assert_eq!(sequence(ctx, base), [1, 2, 3]);
        let small = set_like(ctx, 1.0, &[3, 4]);
        assert_eq!(
            invoke(ctx, base, "isSupersetOf", &[small]).unwrap(),
            Value::bool(false)
        );
        assert_eq!(
            events(),
            [
                "get size",
                "to number",
                "get has",
                "get keys",
                "keys",
                "next",
                "next",
                "return"
            ]
        );
        let small = set_like(ctx, 1.0, &[3, 4]);
        assert_eq!(
            invoke(ctx, base, "isDisjointFrom", &[small]).unwrap(),
            Value::bool(false)
        );
        assert_eq!(
            events(),
            [
                "get size",
                "to number",
                "get has",
                "get keys",
                "keys",
                "next",
                "return"
            ]
        );
        let large = set_like(ctx, 9.0, &[]);
        assert_eq!(
            invoke(ctx, base, "isDisjointFrom", &[large]).unwrap(),
            Value::bool(false)
        );
        assert_eq!(
            events(),
            ["get size", "to number", "get has", "get keys", "has", "has"]
        );
        assert_eq!(
            invoke(ctx, base, "isSubsetOf", &[large]).unwrap(),
            Value::bool(false)
        );
        assert_eq!(
            events(),
            ["get size", "to number", "get has", "get keys", "has"]
        );
        assert_eq!(
            invoke(ctx, base, "isSupersetOf", &[large]).unwrap(),
            Value::bool(false)
        );
        assert_eq!(events(), ["get size", "to number", "get has", "get keys"]);
        let real = set(ctx, &[3, 2, 1, 4]);
        assert_eq!(
            invoke(ctx, base, "isSubsetOf", &[real]).unwrap(),
            Value::bool(true)
        );
        assert_eq!(
            invoke(ctx, real, "isSupersetOf", &[base]).unwrap(),
            Value::bool(true)
        );
        let values = named(ctx, base, "values");
        let keys = named(ctx, base, "keys");
        let iter = get(ctx, base, wk(ctx, WellKnown::Iterator)).unwrap();
        assert_eq!(ctx.get(values), ctx.get(keys));
        assert_eq!(ctx.get(values), ctx.get(iter));
        let negative_zero = ctx.root(Value::number(-0.0)).unwrap();
        invoke(ctx, base, "add", &[negative_zero]).unwrap();
        let iter = invoke(ctx, base, "values", &[]).unwrap();
        let iter = ctx.root(iter).unwrap();
        for _ in 0..3 {
            next_result(ctx, iter);
        }
        assert_eq!(next_result(ctx, iter).0.as_number().unwrap().to_bits(), 0);
    });
}

#[test]
fn set_methods_validate_size_and_close_before_returning_false() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let base = set(ctx, &[1, 2, 3]);
        for (size, error) in [(f64::NAN, ErrorKind::Type), (-1.5, ErrorKind::Range)] {
            let other = set_like(ctx, size, &[]);
            let result = invoke(ctx, base, "union", &[other]);
            assert_error(ctx, result, error);
            assert_eq!(events(), ["get size", "to number"]);
        }
        let other = set_like(ctx, 1.0, &[1]);
        let iter = named(ctx, other, "iterator");
        let error = ctx.new_object().unwrap();
        prop(ctx, iter, "closeError", error);
        assert_eq!(
            invoke(ctx, base, "isDisjointFrom", &[other]),
            Err(Abrupt::Thrown)
        );
        assert_eq!(ctx.take_exception().unwrap().0, ctx.get(error));
        assert_eq!(
            events(),
            [
                "get size",
                "to number",
                "get has",
                "get keys",
                "keys",
                "next",
                "return"
            ]
        );
    });
}

fn mutating_keys<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let base = named(ctx, this, "base");
    let one = number(ctx, 1);
    let three = number(ctx, 3);
    invoke(ctx, base, "delete", &[one])?;
    invoke(ctx, base, "add", &[three])?;
    keys(ctx, this, args)
}

fn mutating_has<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let base = named(ctx, this, "base");
    if ctx.get(args.get(0).unwrap()).as_number() == Some(1.0) {
        let two = number(ctx, 2);
        let three = number(ctx, 3);
        invoke(ctx, base, "delete", &[two])?;
        invoke(ctx, base, "add", &[three])?;
    }
    Ok(Value::bool(false))
}

#[test]
fn set_operations_copy_at_the_protocols_snapshot_step() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        for (method, size, expected) in [
            ("union", 0.0, &[2, 3, 4][..]),
            ("symmetricDifference", 0.0, &[2, 3, 4][..]),
            ("difference", 0.0, &[1, 2][..]),
            ("difference", 9.0, &[1, 2][..]),
        ] {
            let base = set(ctx, &[1, 2]);
            let other = set_like(ctx, size, &[4]);
            prop(ctx, other, "base", base);
            let keys = native(ctx, 15);
            let has = native(ctx, 16);
            prop(ctx, other, "selectedKeys", keys);
            prop(ctx, other, "selectedHas", has);
            let result = invoke(ctx, base, method, &[other]).unwrap();
            let result = ctx.root(result).unwrap();
            assert_eq!(sequence(ctx, result), expected);
            assert_eq!(
                sequence(ctx, base),
                if size == 0.0 { [2, 3] } else { [1, 3] }
            );
            events();
        }
    });
}

fn insert_during_callback<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    event("compute");
    let key = args.get(0).unwrap();
    assert_eq!(ctx.get(key).as_number().unwrap().to_bits(), 0);
    let early = number(ctx, 1);
    invoke(ctx, this, "set", &[key, early])?;
    invoke(ctx, this, "set", &[early, key])?;
    Ok(Value::int32(9))
}

#[test]
fn map_upsert_replaces_a_callback_insertion_in_place() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let map = construct(ctx, "Map", &[]).unwrap();
        let zero = ctx.root(Value::number(-0.0)).unwrap();
        let callback = native(ctx, 14);
        let callback = ctx.bind_function(callback, map, Args::empty()).unwrap();
        assert_eq!(
            invoke(ctx, map, "getOrInsertComputed", &[zero, callback]).unwrap(),
            Value::int32(9)
        );
        assert_eq!(events(), ["compute"]);
        assert_eq!(sequence(ctx, map), [0, 1]);
        assert_eq!(
            invoke(ctx, map, "getOrInsertComputed", &[zero, callback]).unwrap(),
            Value::int32(9)
        );
        assert!(events().is_empty());
        let result = invoke(ctx, map, "getOrInsertComputed", &[zero, zero]);
        assert_error(ctx, result, ErrorKind::Type);
        assert_eq!(
            invoke(ctx, map, "getOrInsert", &[zero, zero]).unwrap(),
            Value::int32(9)
        );
    });
}

#[test]
fn weak_collections_accept_local_symbols_and_prune_dead_entries() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let map = construct(ctx, "WeakMap", &[]).unwrap();
        let set = construct(ctx, "WeakSet", &[]).unwrap();
        let symbol = global(ctx, "Symbol");
        let (map_raw, set_raw, symbol_raw) = (ctx.get(map), ctx.get(set), ctx.get(symbol));
        let local = ctx.with_scope(|ctx| {
            let map = ctx.root(map_raw).unwrap();
            let set = ctx.root(set_raw).unwrap();
            let symbol = ctx.root(symbol_raw).unwrap();
            let undefined = ctx.root(Value::UNDEFINED).unwrap();
            let local = call::call(ctx, symbol, undefined, Args::empty()).unwrap();
            let key = ctx.root(local).unwrap();
            let value = number(ctx, 8);
            assert_eq!(
                invoke(ctx, map, "getOrInsert", &[key, value]).unwrap(),
                ctx.get(value)
            );
            assert_eq!(invoke(ctx, map, "has", &[key]).unwrap(), Value::bool(true));
            assert_eq!(invoke(ctx, map, "get", &[key]).unwrap(), ctx.get(value));
            assert_eq!(invoke(ctx, set, "add", &[key]).unwrap(), ctx.get(set));
            assert_eq!(invoke(ctx, set, "has", &[key]).unwrap(), Value::bool(true));
            assert_eq!(
                invoke(ctx, map, "delete", &[key]).unwrap(),
                Value::bool(true)
            );
            assert_eq!(
                invoke(ctx, set, "delete", &[key]).unwrap(),
                Value::bool(true)
            );
            invoke(ctx, map, "set", &[key, value]).unwrap();
            invoke(ctx, set, "add", &[key]).unwrap();
            let ValueKind::Symbol(local) = local.decode() else {
                unreachable!()
            };
            local
        });
        ctx.collect();
        assert!(ctx.heap().symbols.get_weak(local).is_none());
        let map_id = ctx.object_id(map).unwrap();
        let set_id = ctx.object_id(set).unwrap();
        let ObjectData::WeakMap(map_data) = &ctx.heap().object(map_id).data else {
            unreachable!()
        };
        let ObjectData::WeakSet(set_data) = &ctx.heap().object(set_id).data else {
            unreachable!()
        };
        assert_eq!(map_data.entries.iter().len(), 0);
        assert_eq!(set_data.entries.iter().len(), 0);
        let key = ctx.new_string("registered").unwrap();
        let symbol = invoke(ctx, symbol, "for", &[key]).unwrap();
        let symbol = ctx.root(symbol).unwrap();
        let result = invoke(ctx, map, "set", &[symbol, key]);
        assert_error(ctx, result, ErrorKind::Type);
        let result = invoke(ctx, set, "add", &[symbol]);
        assert_error(ctx, result, ErrorKind::Type);
        assert_eq!(
            invoke(ctx, map, "get", &[symbol]).unwrap(),
            Value::UNDEFINED
        );
        assert_eq!(
            invoke(ctx, set, "has", &[symbol]).unwrap(),
            Value::bool(false)
        );
        assert_eq!(
            invoke(ctx, map, "delete", &[symbol]).unwrap(),
            Value::bool(false)
        );
    });
}

#[test]
fn weak_ref_constructor_and_deref_keep_targets_during_the_job() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let (reference, target) = ctx.with_scope(|ctx| {
            let target = ctx.new_object().unwrap();
            let reference = construct(ctx, "WeakRef", &[target]).unwrap();
            (ctx.get(reference), ctx.get(target))
        });
        let reference = ctx.root(reference).unwrap();
        ctx.collect();
        assert_eq!(invoke(ctx, reference, "deref", &[]).unwrap(), target);
        let raw = ctx.get(reference);
        ctx.with_scope(|ctx| {
            let reference = ctx.root(raw).unwrap();
            let _target = ctx.root(target).unwrap();
            ctx.clear_kept_objects();
            assert_eq!(invoke(ctx, reference, "deref", &[]).unwrap(), target);
        });
        ctx.collect();
        assert_eq!(invoke(ctx, reference, "deref", &[]).unwrap(), target);
        ctx.clear_kept_objects();
        ctx.collect();
        assert_eq!(
            invoke(ctx, reference, "deref", &[]).unwrap(),
            Value::UNDEFINED
        );
        let symbol = global(ctx, "Symbol");
        let key = ctx.new_string("registered").unwrap();
        let registered = invoke(ctx, symbol, "for", &[key]).unwrap();
        let registered = ctx.root(registered).unwrap();
        let result = construct(ctx, "WeakRef", &[registered]);
        assert_error(ctx, result, ErrorKind::Type);
    });
}

#[test]
fn weak_constructors_validate_inputs_before_accessing_new_target_prototype() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let new_target = ctx.new_native(NativeFnId(0), true).unwrap();
        let getter_fn = native(ctx, 4);
        getter(ctx, new_target, PropKey::atom(Atom::PROTOTYPE), getter_fn);
        let invalid = number(ctx, 1);
        let args = ctx.args(&[invalid]).unwrap();
        for name in ["WeakRef", "FinalizationRegistry"] {
            let ctor = global(ctx, name);
            let result = call::construct(ctx, ctor, args, new_target);
            assert_error(ctx, result, ErrorKind::Type);
            assert!(events().is_empty());
            let undefined = ctx.root(Value::UNDEFINED).unwrap();
            let result = call::call(ctx, ctor, undefined, args);
            assert_error(ctx, result, ErrorKind::Type);
        }
    });
}

#[test]
fn finalization_registry_unregisters_tokens_and_queues_cleanup() {
    let mut rt = runtime();
    let realm = rt.default_realm();
    let registry = rt.with_scope(realm, |ctx| {
        let callback = native(ctx, 7);
        let registry = construct(ctx, "FinalizationRegistry", &[callback]).unwrap();
        let persistent = ctx.persist(registry).unwrap();
        let registry = ctx.get(registry);
        ctx.with_scope(|ctx| {
            let registry = ctx.root(registry).unwrap();
            let target = ctx.new_object().unwrap();
            let drop = ctx.new_string("drop").unwrap();
            let token = ctx.new_symbol(None, false).unwrap();
            let result = invoke(ctx, registry, "register", &[target, target]);
            assert_error(ctx, result, ErrorKind::Type);
            assert_eq!(
                invoke(ctx, registry, "register", &[target, drop, token]).unwrap(),
                Value::UNDEFINED
            );
            assert_eq!(
                invoke(ctx, registry, "unregister", &[token]).unwrap(),
                Value::bool(true)
            );
            assert_eq!(
                invoke(ctx, registry, "unregister", &[token]).unwrap(),
                Value::bool(false)
            );
            let held = ctx.new_string("keep").unwrap();
            let undefined = ctx.root(Value::UNDEFINED).unwrap();
            invoke(ctx, registry, "register", &[target, held, undefined]).unwrap();
        });
        persistent
    });
    rt.collect();
    assert_eq!(rt.queued_jobs(), 1);
    assert!(events().is_empty());
    assert_eq!(rt.run_one_job(), Ok(true));
    assert_eq!(events(), ["cleanup"]);
    assert_eq!(rt.run_one_job(), Ok(false));
    rt.release_persistent(registry).unwrap();
}

#[test]
fn symbol_methods_and_well_known_descriptors() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let symbol = global(ctx, "Symbol");
        let undefined = ctx.root(Value::UNDEFINED).unwrap();
        let result = call::construct(ctx, symbol, Args::empty(), symbol);
        assert_error(ctx, result, ErrorKind::Type);
        let proto = named(ctx, symbol, "prototype");
        let anonymous = call::call(ctx, symbol, undefined, Args::empty()).unwrap();
        let anonymous = ctx.root(anonymous).unwrap();
        let key = ctx.intern_str("description").unwrap();
        let getter = get_own_property_descriptor(ctx, proto, key)
            .unwrap()
            .unwrap()
            .get
            .unwrap();
        let getter = ctx.root(getter).unwrap();
        assert_eq!(
            call::call(ctx, getter, anonymous, Args::empty()).unwrap(),
            Value::UNDEFINED
        );
        let empty = ctx.new_string("").unwrap();
        let args = ctx.args(&[empty]).unwrap();
        let described = call::call(ctx, symbol, undefined, args).unwrap();
        let described = ctx.root(described).unwrap();
        let second = call::call(ctx, symbol, undefined, args).unwrap();
        assert_ne!(ctx.get(described), second);
        let desc = call::call(ctx, getter, described, Args::empty()).unwrap();
        assert!(text(ctx, desc).is_empty());
        let key = ctx.new_utf16(&[0xd800, 'x' as u16]).unwrap();
        let registered = invoke(ctx, symbol, "for", &[key]).unwrap();
        let registered = ctx.root(registered).unwrap();
        assert_eq!(
            invoke(ctx, symbol, "for", &[key]).unwrap(),
            ctx.get(registered)
        );
        let result = invoke(ctx, symbol, "keyFor", &[registered]).unwrap();
        assert_eq!(text(ctx, result), [0xd800, 'x' as u16]);
        assert_eq!(
            invoke(ctx, symbol, "keyFor", &[described]).unwrap(),
            Value::UNDEFINED
        );
        let wrapper = crate::ops::convert::to_object(ctx, registered).unwrap();
        assert_eq!(
            invoke(ctx, wrapper, "valueOf", &[]).unwrap(),
            ctx.get(registered)
        );
        let output = invoke(ctx, wrapper, "toString", &[]).unwrap();
        let expected: Vec<_> = "Symbol("
            .encode_utf16()
            .chain([0xd800, 'x' as u16, ')' as u16])
            .collect();
        assert_eq!(text(ctx, output), expected);
        let to_prim = get(ctx, proto, wk(ctx, WellKnown::ToPrimitive)).unwrap();
        assert_eq!(
            call::call(ctx, to_prim, wrapper, Args::empty()).unwrap(),
            ctx.get(registered)
        );
        let invalid = call::call(ctx, to_prim, proto, Args::empty());
        assert_error(ctx, invalid, ErrorKind::Type);
        let invalid = invoke(ctx, symbol, "keyFor", &[wrapper]);
        assert_error(ctx, invalid, ErrorKind::Type);
        let mut names = Vec::new();
        for wk in WellKnown::ALL {
            let name = wk.description().strip_prefix("Symbol.").unwrap();
            names.push(name);
            let key = ctx.intern_str(name).unwrap();
            let desc = get_own_property_descriptor(ctx, symbol, key)
                .unwrap()
                .unwrap();
            assert_eq!(
                desc.value,
                Some(Value::symbol(ctx.heap().symbol_registry.well_known(wk)))
            );
            assert_eq!(
                (desc.writable, desc.enumerable, desc.configurable),
                (Some(false), Some(false), Some(false))
            );
        }
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "asyncDispose",
                "asyncIterator",
                "dispose",
                "hasInstance",
                "isConcatSpreadable",
                "iterator",
                "match",
                "matchAll",
                "replace",
                "search",
                "species",
                "split",
                "toPrimitive",
                "toStringTag",
                "unscopables"
            ]
        );
    });
}
