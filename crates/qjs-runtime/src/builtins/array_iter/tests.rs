use std::cell::Cell;
use std::rc::Rc;

use super::*;
use crate::builtins::promise_json_extras::fulfill_promise;
use crate::object::ErrorKind;
use crate::ops::call::construct;
use crate::ops::promise::new_promise;
use crate::ops::property::{
    get_own_property_descriptor, get_prototype_of, has_property, new_proxy, revoke_proxy,
    set_property,
};
use crate::realm::{NativeFn, Runtime};
use crate::{EngineLimits, ValueKind};

const ZERO: PropKey = PropKey::index(0).unwrap();
const ONE: PropKey = PropKey::index(1).unwrap();
const TWO: PropKey = PropKey::index(2).unwrap();

fn num(n: u32) -> Value {
    Value::int32(n as i32)
}

fn setup(limits: EngineLimits) -> Runtime {
    let mut runtime = Runtime::new(limits).unwrap();
    runtime.set_native_dispatch(&CALLBACKS);
    runtime.install_builtins(runtime.default_realm()).unwrap();
    runtime
}

fn value<'s>(ctx: &mut ExecCtx<'_, 's>, n: u32) -> Handle<'s> {
    ctx.root(num(n)).unwrap()
}

fn ctor<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str) -> Handle<'s> {
    let global = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
    let ctor = named(ctx, global, name).unwrap();
    ctx.root(ctor).unwrap()
}

fn native<'s>(ctx: &mut ExecCtx<'_, 's>, id: u16) -> Handle<'s> {
    ctx.new_native(NativeFnId(id), false).unwrap()
}

fn method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> JsResult<Value> {
    let callee = named(ctx, object, name)?;
    let callee = ctx.root(callee)?;
    let args = ctx.args(args)?;
    call(ctx, callee, object, args)
}

fn method_root<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> Handle<'s> {
    let result = method(ctx, object, name, args).unwrap();
    ctx.root(result).unwrap()
}

fn array<'s>(ctx: &mut ExecCtx<'_, 's>, items: &[Value]) -> Handle<'s> {
    let array = ctx.new_array().unwrap();
    for (i, &item) in items.iter().enumerate() {
        create_index(ctx, array, i as u64, item).unwrap();
    }
    array
}

fn create_named<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, name: &str, value: Handle<'s>) {
    let k = key(ctx, name).unwrap();
    create(ctx, object, k, value).unwrap();
}

fn error(ctx: &mut ExecCtx<'_, '_>, kind: ErrorKind) {
    let (error, _) = ctx.take_exception().unwrap();
    assert!(
        matches!(&ctx.heap().object(error.as_object().unwrap()).data, ObjectData::Error(data) if data.kind == kind)
    );
}

#[test]
fn array_constructor_validates_length_and_uses_new_target() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let constructor = ctor(ctx, "Array");
        let other = ctx.new_native(NativeFnId(3), true).unwrap();
        let prototype = ctx.new_object().unwrap();
        create(ctx, other, PropKey::atom(Atom::PROTOTYPE), prototype).unwrap();
        let length = value(ctx, 1000);
        let args = ctx.args(&[length]).unwrap();
        let result = construct(ctx, constructor, args, other).unwrap();
        let result = ctx.root(result).unwrap();
        assert_eq!(
            get_prototype_of(ctx, result).unwrap(),
            ctx.get(prototype).as_object()
        );
        assert_eq!(get_length(ctx, result).unwrap(), 1000);
        assert!(!has_property(ctx, result, ZERO).unwrap());
        let bad_length = ctx.root(Value::number(0.5)).unwrap();
        let bad_args = ctx.args(&[bad_length]).unwrap();
        assert_eq!(call(ctx, constructor, other, bad_args), Err(Abrupt::Thrown));
        error(ctx, ErrorKind::Range);
        let handler = ctx.new_object().unwrap();
        let proxy = new_proxy(ctx, result, handler).unwrap();
        assert_eq!(
            method(ctx, constructor, "isArray", &[proxy]).unwrap(),
            Value::bool(true)
        );
        revoke_proxy(ctx, proxy).unwrap();
        assert_eq!(
            method(ctx, constructor, "isArray", &[proxy]),
            Err(Abrupt::Thrown)
        );
        error(ctx, ErrorKind::Type);
    });
}

#[test]
fn map_preserves_holes_when_callback_shrinks_receiver() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let array = array(ctx, &[num(11), num(12), num(13)]);
        let cb = native(ctx, 0);
        let mapped = method_root(ctx, array, "map", &[cb]);
        assert_eq!(get_length(ctx, mapped).unwrap(), 3);
        assert_eq!(get_property(ctx, mapped, ZERO).unwrap(), num(11));
        assert!(!has_property(ctx, mapped, ONE).unwrap());
        let undef = ctx.root(Value::UNDEFINED).unwrap();
        assert_eq!(
            method(ctx, mapped, "includes", &[undef]).unwrap(),
            Value::bool(true)
        );
        assert_eq!(
            method(ctx, mapped, "indexOf", &[undef]).unwrap(),
            Value::int32(-1)
        );
        let copy = method_root(ctx, mapped, "toReversed", &[]);
        assert!(has_property(ctx, copy, ZERO).unwrap());
        assert_eq!(get_property(ctx, copy, TWO).unwrap(), num(11));
        assert_eq!(get_length(ctx, array).unwrap(), 1);
    });
}

#[test]
fn species_constructor_and_concat_spreadability() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = array(ctx, &[num(11), num(12)]);
        let holder = ctx.new_object().unwrap();
        let species = ctx.new_native(NativeFnId(3), true).unwrap();
        create(ctx, holder, symbol(ctx, WellKnown::Species), species).unwrap();
        create_named(ctx, input, "constructor", holder);
        let sliced = method_root(ctx, input, "slice", &[]);
        assert!(matches!(
            ctx.heap().object(ctx.get(sliced).as_object().unwrap()).data,
            ObjectData::Ordinary
        ));
        assert_eq!(
            named(ctx, sliced, "requested").unwrap().as_number(),
            Some(2.0)
        );
        assert_eq!(get_property(ctx, sliced, ZERO).unwrap(), num(11));
        let spreadable = ctx.new_object().unwrap();
        let yes = ctx.root(Value::bool(true)).unwrap();
        create(
            ctx,
            spreadable,
            symbol(ctx, WellKnown::IsConcatSpreadable),
            yes,
        )
        .unwrap();
        let len = value(ctx, 1);
        create(ctx, spreadable, LENGTH, len).unwrap();
        let n = value(ctx, 13);
        create(ctx, spreadable, ZERO, n).unwrap();
        let concatenated = method_root(ctx, input, "concat", &[spreadable]);
        assert_eq!(
            named(ctx, concatenated, "requested").unwrap().as_number(),
            Some(0.0)
        );
        assert_eq!(get_length(ctx, concatenated).unwrap(), 3);
        assert_eq!(get_property(ctx, concatenated, TWO).unwrap(), num(13));
    });
}

#[test]
fn sort_is_stable_and_uses_snapshot_after_comparator_changes_length() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let a = ctx.new_object().unwrap();
        let b = ctx.new_object().unwrap();
        let c = ctx.new_object().unwrap();
        let one = value(ctx, 1);
        let zero = value(ctx, 0);
        create_named(ctx, a, "rank", one);
        create_named(ctx, b, "rank", one);
        create_named(ctx, c, "rank", zero);
        let array = array(ctx, &[ctx.get(a), ctx.get(b), Value::UNDEFINED, ctx.get(c)]);
        let six = value(ctx, 6);
        assert!(set_property(ctx, array, LENGTH, six).unwrap());
        let callback = native(ctx, 2);
        let callback = ctx.bind_function(callback, array, Args::empty()).unwrap();
        assert_eq!(
            method(ctx, array, "sort", &[callback]).unwrap(),
            ctx.get(array)
        );
        assert_eq!(get_property(ctx, array, ZERO).unwrap(), ctx.get(c));
        assert_eq!(get_property(ctx, array, ONE).unwrap(), ctx.get(a));
        assert_eq!(get_property(ctx, array, TWO).unwrap(), ctx.get(b));
        assert_eq!(
            get_property(ctx, array, PropKey::index(3).unwrap()).unwrap(),
            Value::UNDEFINED
        );
        assert!(has_property(ctx, array, PropKey::index(3).unwrap()).unwrap());
        assert!(!has_property(ctx, array, PropKey::index(4).unwrap()).unwrap());
    });
}

#[test]
fn huge_fill_charges_dense_storage_before_writing() {
    let mut rt = setup(EngineLimits {
        heap_bytes: 256 * 1024,
        ..EngineLimits::default()
    });
    rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = ctor(ctx, "Array");
        let length = value(ctx, 1_000_000_000);
        let args = ctx.args(&[length]).unwrap();
        let array = construct(ctx, ctor, args, ctor).unwrap();
        let array = ctx.root(array).unwrap();
        let zero = value(ctx, 0);
        assert_eq!(method(ctx, array, "fill", &[zero]), Err(Abrupt::Thrown));
        assert!(ctx.is_oom());
        assert!(!has_property(ctx, array, ZERO).unwrap());
        assert_eq!(get_length(ctx, array).unwrap(), 1_000_000_000);
    });
}

#[test]
fn fill_polls_interrupt_between_dense_chunks() {
    let mut rt = setup(EngineLimits::default());
    let checks = Rc::new(Cell::new(0));
    let count = checks.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        count.set(count.get() + 1);
        count.get() >= 6
    })));
    rt.with_scope(rt.default_realm(), |ctx| {
        let array = super::array::array_of_length(ctx, 10_000).unwrap();
        let zero = value(ctx, 0);
        assert_eq!(
            method(ctx, array, "fill", &[zero]),
            Err(Abrupt::Interrupted)
        );
        assert_eq!(get_property(ctx, array, ZERO).unwrap(), num(0));
        assert!(!has_property(ctx, array, PropKey::index(9999).unwrap()).unwrap());
    });
    assert_eq!(checks.get(), 6);
}

#[test]
fn join_stops_cycles_and_preserves_utf16_code_units() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let lone = ctx.new_utf16(&[0xd800]).unwrap();
        let array = array(ctx, &[ctx.get(lone)]);
        create(ctx, array, ONE, array).unwrap();
        let result = method(ctx, array, "join", &[]).unwrap();
        let ValueKind::String(result) = result.decode() else {
            panic!("expected a string");
        };
        assert!(ctx.heap().string_units(result).eq([0xd800, b',' as u16]));
    });
}

#[test]
fn array_locale_string_does_not_forward_non_intl_arguments() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = ctx.new_object().unwrap();
        let method_fn = native(ctx, 14);
        create_named(ctx, input, "toLocaleString", method_fn);
        let array = array(ctx, &[ctx.get(input)]);
        let locale = ctx.new_string("en").unwrap();
        let result = method(ctx, array, "toLocaleString", &[locale]).unwrap();
        let ValueKind::String(result) = result.decode() else {
            panic!("expected a string");
        };
        assert!(ctx.heap().string_units(result).eq("ok".encode_utf16()));
    });
}

#[test]
fn array_to_string_fallback_keeps_proxy_array_brand() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let array = ctx.new_array().unwrap();
        let undefined = ctx.root(Value::UNDEFINED).unwrap();
        create_named(ctx, array, "join", undefined);
        let handler = ctx.new_object().unwrap();
        let proxy = new_proxy(ctx, array, handler).unwrap();
        let result = method(ctx, proxy, "toString", &[]).unwrap();
        let ValueKind::String(result) = result.decode() else {
            panic!("expected a string");
        };
        assert!(
            ctx.heap()
                .string_units(result)
                .eq("[object Array]".encode_utf16())
        );
    });
}

#[test]
fn array_iterator_is_live_until_done_and_values_alias_iterator_symbol() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let array = array(ctx, &[num(9)]);
        assert_eq!(
            named(ctx, array, "values").unwrap(),
            get_property(ctx, array, symbol(ctx, WellKnown::Iterator)).unwrap()
        );
        let iter = method_root(ctx, array, "entries", &[]);
        let item = method_root(ctx, iter, "next", &[]);
        let pair = named(ctx, item, "value").unwrap();
        let pair = ctx.root(pair).unwrap();
        assert_eq!(get_index(ctx, pair, 0).unwrap().as_number(), Some(0.0));
        assert_eq!(get_index(ctx, pair, 1).unwrap(), num(9));
        let second = value(ctx, 7);
        create(ctx, array, ONE, second).unwrap();
        let next = method_root(ctx, iter, "next", &[]);
        assert_eq!(named(ctx, next, "done").unwrap(), Value::bool(false));
        let done = method_root(ctx, iter, "next", &[]);
        assert_eq!(named(ctx, done, "done").unwrap(), Value::bool(true));
        create(ctx, array, TWO, second).unwrap();
        let done = method_root(ctx, iter, "next", &[]);
        assert_eq!(named(ctx, done, "done").unwrap(), Value::bool(true));
    });
}

#[test]
fn iterator_helpers_chain_lazily_and_reduce() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let input = array(ctx, &[num(1), num(2), num(3), num(4)]);
        let iterator = method_root(ctx, input, "values", &[]);
        let double = native(ctx, 1);
        let mapped = method_root(ctx, iterator, "map", &[double]);
        let keep = native(ctx, 4);
        let filtered = method_root(ctx, mapped, "filter", &[keep]);
        let one = value(ctx, 1);
        let dropped = method_root(ctx, filtered, "drop", &[one]);
        let taken = method_root(ctx, dropped, "take", &[one]);
        let duplicate = native(ctx, 5);
        let flat = method_root(ctx, taken, "flatMap", &[duplicate]);
        let sum = native(ctx, 6);
        assert_eq!(
            method(ctx, flat, "reduce", &[sum]).unwrap().as_number(),
            Some(12.0)
        );
        let exhausted = method_root(ctx, flat, "toArray", &[]);
        assert_eq!(get_length(ctx, exhausted).unwrap(), 0);
    });
}

#[test]
fn iterator_helper_guards_reentry_and_closes_when_mapper_throws() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let raw = ctx.new_object().unwrap();
        let next = native(ctx, 8);
        let close = native(ctx, 9);
        create_named(ctx, raw, "next", next);
        create_named(ctx, raw, "return", close);
        let ctor = ctor(ctx, "Iterator");
        let wrapped = method_root(ctx, ctor, "from", &[raw]);
        let callback = native(ctx, 7);
        let callback = ctx.bind_function(callback, raw, Args::empty()).unwrap();
        let helper = method_root(ctx, wrapped, "map", &[callback]);
        create_named(ctx, raw, "helper", helper);
        assert_eq!(method(ctx, helper, "next", &[]), Err(Abrupt::Thrown));
        error(ctx, ErrorKind::Type);
        assert_eq!(named(ctx, raw, "closed").unwrap(), Value::bool(true));
        let done = method_root(ctx, helper, "next", &[]);
        assert_eq!(named(ctx, done, "done").unwrap(), Value::bool(true));
    });
}

#[test]
fn iterator_zip_longest_keyed_and_concat() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let iter = ctor(ctx, "Iterator");
        let a = array(ctx, &[num(1), num(2)]);
        let b = array(ctx, &[num(3)]);
        let inputs = array(ctx, &[ctx.get(a), ctx.get(b)]);
        let options = ctx.new_object().unwrap();
        let mode = ctx.new_string("longest").unwrap();
        create_named(ctx, options, "mode", mode);
        let padding = array(ctx, &[num(8), num(9)]);
        create_named(ctx, options, "padding", padding);
        let zip = method_root(ctx, iter, "zip", &[inputs, options]);
        let rows = method_root(ctx, zip, "toArray", &[]);
        assert_eq!(get_length(ctx, rows).unwrap(), 2);
        let second = get_property(ctx, rows, ONE).unwrap();
        let second = ctx.root(second).unwrap();
        assert_eq!(get_property(ctx, second, ZERO).unwrap(), num(2));
        assert_eq!(get_property(ctx, second, ONE).unwrap(), num(9));
        let input = ctx.new_object().unwrap();
        create_named(ctx, input, "alpha", a);
        create_named(ctx, input, "beta", b);
        let zip = method_root(ctx, iter, "zipKeyed", &[input]);
        let rows = method_root(ctx, zip, "toArray", &[]);
        assert_eq!(get_length(ctx, rows).unwrap(), 1);
        let row = get_property(ctx, rows, ZERO).unwrap();
        let row = ctx.root(row).unwrap();
        assert_eq!(get_prototype_of(ctx, row).unwrap(), None);
        assert_eq!(named(ctx, row, "alpha").unwrap(), num(1));
        assert_eq!(named(ctx, row, "beta").unwrap(), num(3));
        let concat = method_root(ctx, iter, "concat", &[a, b]);
        let result = method_root(ctx, concat, "toArray", &[]);
        assert_eq!(get_length(ctx, result).unwrap(), 3);
        assert_eq!(get_property(ctx, result, TWO).unwrap(), num(3));
    });
}

#[test]
fn disposable_stack_move_adopt_and_defer_chain_suppressed_errors() {
    let mut rt = setup(EngineLimits::default());
    rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = ctor(ctx, "DisposableStack");
        let stack = construct(ctx, ctor, Args::empty(), ctor).unwrap();
        let stack = ctx.root(stack).unwrap();
        let dispose = named(ctx, stack, "dispose").unwrap();
        assert_eq!(
            dispose,
            get_property(ctx, stack, symbol(ctx, WellKnown::Dispose)).unwrap()
        );
        let first = value(ctx, 1);
        let second = value(ctx, 2);
        let throw_arg = native(ctx, 10);
        let throw_undefined = native(ctx, 11);
        assert_eq!(
            method(ctx, stack, "adopt", &[first, throw_arg]).unwrap(),
            num(1)
        );
        method(ctx, stack, "adopt", &[second, throw_arg]).unwrap();
        method(ctx, stack, "defer", &[throw_undefined]).unwrap();
        let moved = method_root(ctx, stack, "move", &[]);
        assert_eq!(named(ctx, stack, "disposed").unwrap(), Value::bool(true));
        assert_eq!(
            method(ctx, stack, "dispose", &[]).unwrap(),
            Value::UNDEFINED
        );
        assert_eq!(method(ctx, moved, "dispose", &[]), Err(Abrupt::Thrown));
        let (thrown, _) = ctx.take_exception().unwrap();
        let thrown = ctx.root(thrown).unwrap();
        assert_eq!(
            get_prototype_of(ctx, thrown).unwrap(),
            Some(ctx.realm().suppressed_error_proto)
        );
        assert_eq!(named(ctx, thrown, "error").unwrap(), num(1));
        let k = key(ctx, "error").unwrap();
        assert_eq!(
            get_own_property_descriptor(ctx, thrown, k)
                .unwrap()
                .unwrap()
                .enumerable,
            Some(false)
        );
        let suppressed = named(ctx, thrown, "suppressed").unwrap();
        let suppressed = ctx.root(suppressed).unwrap();
        assert_eq!(named(ctx, suppressed, "error").unwrap(), num(2));
        assert_eq!(
            named(ctx, suppressed, "suppressed").unwrap(),
            Value::UNDEFINED
        );
        assert_eq!(
            method(ctx, moved, "dispose", &[]).unwrap(),
            Value::UNDEFINED
        );
        assert_eq!(
            method(ctx, moved, "defer", &[throw_undefined]),
            Err(Abrupt::Thrown)
        );
        error(ctx, ErrorKind::Reference);
    });
}

#[test]
fn array_from_async_awaits_values_and_mapper_results() {
    let mut rt = setup(EngineLimits::default());
    let root = rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = ctor(ctx, "Array");
        let one = value(ctx, 4);
        let promise = new_promise(ctx).unwrap();
        fulfill_promise(ctx, promise, one).unwrap();
        let input = array(ctx, &[ctx.get(promise), num(5)]);
        let mapper = native(ctx, 12);
        let result = method_root(ctx, ctor, "fromAsync", &[input, mapper]);
        assert_eq!(promise_result(ctx, result), (None, false));
        ctx.persist(result).unwrap()
    });
    rt.run_jobs().unwrap();
    rt.with_scope(rt.default_realm(), |ctx| {
        let result = ctx.get_persistent(&root).unwrap().unwrap();
        let (value, rejected) = promise_result(ctx, result);
        assert!(!rejected);
        let array = ctx.root(value.unwrap()).unwrap();
        assert_eq!(get_index(ctx, array, 0).unwrap().as_number(), Some(8.0));
        assert_eq!(get_index(ctx, array, 1).unwrap().as_number(), Some(10.0));
    });
    rt.release_persistent(root);
}

#[test]
fn async_disposal_awaits_in_lifo_order_and_preserves_rejections() {
    let mut rt = setup(EngineLimits::default());
    let (promise, marker) = rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = ctor(ctx, "AsyncDisposableStack");
        let stack = construct(ctx, ctor, Args::empty(), ctor).unwrap();
        let stack = ctx.root(stack).unwrap();
        let marker = ctx.new_object().unwrap();
        let earlier = native(ctx, 9);
        let earlier = ctx.bind_function(earlier, marker, Args::empty()).unwrap();
        method(ctx, stack, "defer", &[earlier]).unwrap();
        let later = native(ctx, 13);
        method(ctx, stack, "defer", &[later]).unwrap();
        let promise = method_root(ctx, stack, "disposeAsync", &[]);
        assert_eq!(named(ctx, stack, "disposed").unwrap(), Value::bool(true));
        assert_eq!(named(ctx, marker, "closed").unwrap(), Value::UNDEFINED);
        assert_eq!(promise_result(ctx, promise), (None, false));
        (ctx.persist(promise).unwrap(), ctx.persist(marker).unwrap())
    });
    rt.run_jobs().unwrap();
    rt.with_scope(rt.default_realm(), |ctx| {
        let promise = ctx.get_persistent(&promise).unwrap().unwrap();
        let marker = ctx.get_persistent(&marker).unwrap().unwrap();
        assert_eq!(named(ctx, marker, "closed").unwrap(), Value::bool(true));
        assert_eq!(promise_result(ctx, promise), (Some(num(53)), true));
    });
    rt.release_persistent(promise);
    rt.release_persistent(marker);
}

fn promise_result<'s>(ctx: &ExecCtx<'_, 's>, promise: Handle<'s>) -> (Option<Value>, bool) {
    let ObjectData::Promise(data) = &ctx
        .heap()
        .object(ctx.get(promise).as_object().unwrap())
        .data
    else {
        panic!("expected a Promise");
    };
    (data.result, data.rejected)
}

fn shrink<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let array = args.get(2).unwrap();
    let one = ctx.root(num(1))?;
    assert!(set_property(ctx, array, LENGTH, one)?);
    ctx.collect();
    Ok(ctx.get(args.get(0).unwrap()))
}

fn double<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    Ok(Value::number(
        ctx.get(args.get(0).unwrap()).as_number().unwrap() * 2.0,
    ))
}

fn compare<'s>(ctx: &mut ExecCtx<'_, 's>, array: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let zero = ctx.root(num(0))?;
    assert!(set_property(ctx, array, LENGTH, zero)?);
    ctx.collect();
    let a = named(ctx, args.get(0).unwrap(), "rank")?
        .as_number()
        .unwrap();
    let b = named(ctx, args.get(1).unwrap(), "rank")?
        .as_number()
        .unwrap();
    Ok(Value::number(a - b))
}

fn species<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    create_named(ctx, this, "requested", args.get(0).unwrap());
    Ok(ctx.get(this))
}

fn keep<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    Ok(Value::bool(
        ctx.get(args.get(0).unwrap()).as_number().unwrap() >= 4.0,
    ))
}

fn duplicate<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let item = ctx.get(args.get(0).unwrap());
    let values = array(ctx, &[item, item]);
    Ok(ctx.get(values))
}

fn sum<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let a = ctx.get(args.get(0).unwrap()).as_number().unwrap();
    let b = ctx.get(args.get(1).unwrap()).as_number().unwrap();
    Ok(Value::number(a + b))
}

fn reenter<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let helper = named(ctx, this, "helper")?;
    let helper = ctx.root(helper)?;
    method(ctx, helper, "next", &[])
}

fn next<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    super::iterator::result(ctx, Some(num(1)))
}

fn close<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let yes = ctx.root(Value::bool(true))?;
    create_named(ctx, this, "closed", yes);
    super::iterator::result(ctx, None)
}

fn throw_argument<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    assert_eq!(args.len(), 1);
    Err(ctx.throw(args.get(0).unwrap()))
}

fn throw_undefined<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    assert_eq!(ctx.get(this), Value::UNDEFINED);
    assert!(args.is_empty());
    Err(ctx.throw(this))
}

fn double_async<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let doubled = double(ctx, this, args)?;
    let doubled = ctx.root(doubled)?;
    let promise = new_promise(ctx)?;
    fulfill_promise(ctx, promise, doubled)?;
    Ok(ctx.get(promise))
}

fn no_args<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    assert!(args.is_empty());
    let string = ctx.new_string("ok")?;
    Ok(ctx.get(string))
}

fn reject_53<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let promise = new_promise(ctx)?;
    let value = ctx.root(num(53))?;
    reject_promise(ctx, promise, value)?;
    Ok(ctx.get(promise))
}

static CALLBACKS: [NativeFn; 15] = [
    shrink,
    double,
    compare,
    species,
    keep,
    duplicate,
    sum,
    reenter,
    next,
    close,
    throw_argument,
    throw_undefined,
    double_async,
    reject_53,
    no_args,
];
