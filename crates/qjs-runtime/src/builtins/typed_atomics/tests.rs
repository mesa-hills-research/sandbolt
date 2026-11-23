use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;
use crate::object::ErrorKind;
use crate::value::ValueKind;
use crate::vm::MiniOp;
use crate::{Abrupt, EngineLimits, Runtime};

fn runtime() -> Runtime {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.install_builtins(runtime.default_realm()).unwrap();
    runtime
}

fn global<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str) -> Handle<'s> {
    let global = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
    get(ctx, global, name)
}

fn root<'s>(ctx: &mut ExecCtx<'_, 's>, value: Value) -> Handle<'s> {
    ctx.root(value).unwrap()
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>, name: &str) -> Handle<'s> {
    let value = named(ctx, obj, name).unwrap();
    root(ctx, value)
}

fn length<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>) -> usize {
    named(ctx, obj, "length").unwrap().as_number().unwrap() as usize
}

fn construct<'s>(ctx: &mut ExecCtx<'_, 's>, name: &str, args: &[Handle<'s>]) -> Handle<'s> {
    let ctor = global(ctx, name);
    let args = ctx.args(args).unwrap();
    let result = call::construct(ctx, ctor, args, ctor).unwrap();
    root(ctx, result)
}

fn invoke<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    receiver: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> JsResult<Value> {
    let callee = get(ctx, receiver, name);
    let args = ctx.args(args)?;
    call::call(ctx, callee, receiver, args)
}

fn call_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    receiver: Handle<'s>,
    name: &str,
    args: &[Handle<'s>],
) -> Handle<'s> {
    let result = invoke(ctx, receiver, name, args).unwrap();
    root(ctx, result)
}

fn text(ctx: &ExecCtx<'_, '_>, value: Value) -> String {
    let ValueKind::String(id) = value.decode() else {
        panic!("Expected a string: {value:?}");
    };
    String::from_utf16(&ctx.heap().string_units(id).collect::<Vec<_>>()).unwrap()
}

fn number_prop<'s>(ctx: &mut ExecCtx<'_, 's>, obj: Handle<'s>, prop: &str) -> f64 {
    named(ctx, obj, prop).unwrap().as_number().unwrap()
}

fn error(ctx: &mut ExecCtx<'_, '_>, result: JsResult<Value>, kind: ErrorKind) {
    assert_eq!(result, Err(Abrupt::Thrown));
    let (exc, oom) = ctx.take_exception().unwrap();
    assert!(!oom);
    let ObjectData::Error(exc) = &ctx.heap().object(exc.as_object().unwrap()).data else {
        panic!("Missing Error payload");
    };
    assert_eq!(exc.kind, kind);
}

fn buffer<'s>(ctx: &mut ExecCtx<'_, 's>, class: &str, len: usize, max: usize) -> Handle<'s> {
    let size = root(ctx, number(len));
    let options = ctx.new_object().unwrap();
    put(
        ctx,
        options,
        "maxByteLength",
        number(max),
        PropFlags::DEFAULT_DATA,
    )
    .unwrap();
    construct(ctx, class, &[size, options])
}

fn resize_during<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    buffer: Handle<'s>,
    size: usize,
    returns: Value,
) -> Handle<'s> {
    let resize = get(ctx, buffer, "resize");
    let len = root(ctx, number(size));
    let result = root(ctx, returns);
    let constants = ctx.args(&[resize, buffer, len, result]).unwrap();
    let f = ctx
        .new_bytecode(
            &[
                MiniOp::Constant(0),
                MiniOp::Constant(1),
                MiniOp::Constant(2),
                MiniOp::Call(1),
                MiniOp::Pop,
                MiniOp::Constant(3),
                MiniOp::Return,
            ],
            constants,
            Args::empty(),
            false,
        )
        .unwrap();
    let obj = ctx.new_object().unwrap();
    put(ctx, obj, "valueOf", ctx.get(f), PropFlags::DEFAULT_DATA).unwrap();
    obj
}

#[test]
fn transfer_moves_bytes_and_detach_releases_capacity() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let buf = buffer(ctx, "ArrayBuffer", 5000, 8192);
        let view = construct(ctx, "Uint8Array", &[buf]);
        let view_id = ctx.object_id(view).unwrap();
        let buf_id = ctx.object_id(buf).unwrap();
        let byte = root(ctx, Value::int32(0x42));
        typed_array_set_index(ctx, view_id, Some(5), byte).unwrap();
        let size = root(ctx, Value::int32(32));
        call_method(ctx, buf, "resize", &[size]);
        assert_eq!(length(ctx, view), 32);
        let size = root(ctx, Value::int32(8192));
        let transferred = call_method(ctx, buf, "transfer", &[size]);
        assert_eq!(named(ctx, buf, "detached").unwrap(), Value::bool(true));
        assert_eq!(number_prop(ctx, buf, "byteLength"), 0.0);
        assert_eq!(number_prop(ctx, buf, "maxByteLength"), 0.0);
        assert_eq!(typed_array_length(ctx.heap(), view_id), None);
        assert!(buffer::info(ctx.heap(), buf_id).unwrap().detached);
        assert_eq!(number_prop(ctx, transferred, "byteLength"), 8192.0);
        let bytes = construct(ctx, "Uint8Array", &[transferred]);
        let bytes = ctx.object_id(bytes).unwrap();
        assert_eq!(
            typed_array_get_index(ctx, bytes, 5).unwrap().as_number(),
            Some(66.0)
        );
        assert_eq!(
            typed_array_get_index(ctx, bytes, 8191).unwrap().as_number(),
            Some(0.0)
        );
        let fixed = call_method(ctx, transferred, "transferToFixedLength", &[]);
        assert_eq!(named(ctx, fixed, "resizable").unwrap(), Value::bool(false));
        let fixed_id = ctx.object_id(fixed).unwrap();
        let before = ctx.heap().ledger.used_bytes();
        detach_array_buffer(ctx, fixed_id).unwrap();
        assert_eq!(ctx.heap().ledger.used_bytes(), before - 8192);
        detach_array_buffer(ctx, fixed_id).unwrap();
        assert_eq!(ctx.heap().ledger.used_bytes(), before - 8192);
    });
}

#[test]
fn buffer_allocations_obey_the_heap_cap_before_growing() {
    let limits = EngineLimits {
        heap_bytes: 512 * 1024,
        ..EngineLimits::default()
    };
    let mut rt = Runtime::new(limits).unwrap();
    rt.install_builtins(rt.default_realm()).unwrap();
    rt.with_scope(rt.default_realm(), |ctx| {
        let buf = buffer(ctx, "ArrayBuffer", 16, limits.heap_bytes);
        let large = root(ctx, number(limits.heap_bytes));
        for method in ["resize", "transfer"] {
            assert_eq!(invoke(ctx, buf, method, &[large]), Err(Abrupt::Thrown));
            assert!(ctx.take_exception().unwrap().1);
            assert_eq!(number_prop(ctx, buf, "byteLength"), 16.0);
        }
        let sab = global(ctx, "SharedArrayBuffer");
        let options = ctx.new_object().unwrap();
        put(
            ctx,
            options,
            "maxByteLength",
            ctx.get(large),
            PropFlags::DEFAULT_DATA,
        )
        .unwrap();
        let zero = root(ctx, Value::int32(0));
        let args = ctx.args(&[zero, options]).unwrap();
        assert_eq!(call::construct(ctx, sab, args, sab), Err(Abrupt::Thrown));
        assert!(ctx.take_exception().unwrap().1);
        assert!(ctx.heap().ledger.peak_bytes() <= limits.heap_bytes);
    });
}

#[test]
fn concrete_arrays_use_their_intrinsics_and_numeric_conversions() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let n = root(ctx, Value::int32(2));
        let ab = global(ctx, "ArrayBuffer");
        let view_func = get(ctx, ab, "isView");
        let undefined = root(ctx, Value::UNDEFINED);
        let samples: [(TypedArrayKind, f64, f64); 10] = [
            (TypedArrayKind::Int8, 258.2, 2.0),
            (TypedArrayKind::Uint8, -1.0, 255.0),
            (TypedArrayKind::Uint8Clamped, 2.5, 2.0),
            (TypedArrayKind::Int16, 65_535.0, -1.0),
            (TypedArrayKind::Uint16, -1.0, 65_535.0),
            (TypedArrayKind::Int32, 4_294_967_295.0, -1.0),
            (TypedArrayKind::Uint32, -1.0, 4_294_967_295.0),
            (
                TypedArrayKind::Float16,
                1.0 + 3.0 / 2048.0,
                1.0 + 1.0 / 512.0,
            ),
            (TypedArrayKind::Float32, 1.0 + 2.0_f64.powi(-24), 1.0),
            (TypedArrayKind::Float64, -0.0, -0.0),
        ];
        for (kind, input, expected) in samples {
            ctx.with_scope(|ctx| {
                let ctor = global(ctx, kind.name());
                let proto = get(ctx, ctor, "prototype");
                assert_eq!(
                    property::get_prototype_of(ctx, ctor).unwrap(),
                    Some(ctx.realm().typed_array_ctor)
                );
                assert_eq!(
                    property::get_prototype_of(ctx, proto).unwrap(),
                    Some(ctx.realm().typed_array_proto)
                );
                assert_eq!(
                    number_prop(ctx, ctor, "BYTES_PER_ELEMENT"),
                    kind.size() as f64
                );
                assert_eq!(
                    number_prop(ctx, proto, "BYTES_PER_ELEMENT"),
                    kind.size() as f64
                );
                let n = root(ctx, Value::int32(2));
                let arr = construct(ctx, kind.name(), &[n]);
                let obj = ctx.object_id(arr).unwrap();
                let input = root(ctx, Value::number(input));
                typed_array_set_index(ctx, obj, Some(1), input).unwrap();
                assert_eq!(
                    typed_array_get_index(ctx, obj, 1)
                        .unwrap()
                        .as_number()
                        .unwrap()
                        .to_bits(),
                    expected.to_bits()
                );
                assert_eq!(
                    call::call(ctx, ctor, arr, Args::empty()),
                    Err(Abrupt::Thrown)
                );
                ctx.take_exception();
            });
        }
        for (kind, unsigned) in [
            (TypedArrayKind::BigInt64, false),
            (TypedArrayKind::BigUint64, true),
        ] {
            let arr = construct(ctx, kind.name(), &[n]);
            let arr_obj = ctx.object_id(arr).unwrap();
            let args = ctx.args(&[arr]).unwrap();
            assert_eq!(
                call::call(ctx, view_func, undefined, args).unwrap(),
                Value::bool(true)
            );
            let input = ctx.new_string("18446744073709551615").unwrap();
            typed_array_set_index(ctx, arr_obj, Some(0), input).unwrap();
            let found = typed_array_get_index(ctx, arr_obj, 0).unwrap();
            if unsigned {
                let ValueKind::BigInt(id) = found.decode() else {
                    panic!("Expected a large BigInt");
                };
                assert_eq!(&*ctx.heap().bigints.get(id).unwrap().limbs, &[u32::MAX; 2]);
            } else {
                assert_eq!(found, Value::short_bigint(-1).unwrap());
            }
        }
    });
}

#[test]
fn float16_round_trips_and_rounds_without_float32_intermediates() {
    for bits in 0..=u16::MAX {
        let n = elements::from_half(bits);
        if n.is_nan() {
            assert_eq!(elements::to_half(n), 0x7e00);
        } else {
            assert_eq!(elements::to_half(n), bits);
        }
    }
    let tie = 1.0_f64 + 2.0_f64.powi(-11);
    let above = f64::from_bits(tie.to_bits() + 1);
    assert_eq!(elements::to_half(tie), 0x3c00);
    assert_eq!(elements::to_half(above), 0x3c01);
    assert_eq!(elements::to_half(2.0_f64.powi(-25)), 0);
    assert_eq!(elements::to_half(-2.0_f64.powi(-25)), 0x8000);
    assert_eq!(elements::to_half(65_520.0), 0x7c00);
    assert_eq!(elements::to_half(65_519.0), 0x7bff);
}

#[test]
fn data_view_rechecks_bounds_and_preserves_byte_order() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let buf = buffer(ctx, "ArrayBuffer", 8, 32);
        let zero = root(ctx, number(0));
        let two = root(ctx, number(2));
        let view = construct(ctx, "DataView", &[buf, two]);
        let n = root(ctx, Value::int32(0x3412));
        let little = root(ctx, Value::bool(true));
        call_method(ctx, view, "setUint16", &[zero, n, little]);
        assert_eq!(
            invoke(ctx, view, "getUint16", &[zero]).unwrap().as_number(),
            Some(0x1234 as f64)
        );
        assert_eq!(
            invoke(ctx, view, "getUint16", &[zero, little])
                .unwrap()
                .as_number(),
            Some(0x3412 as f64)
        );
        let one_half = root(ctx, Value::number(1.5));
        call_method(ctx, view, "setFloat16", &[two, one_half]);
        assert_eq!(
            invoke(ctx, view, "getUint16", &[two]).unwrap().as_number(),
            Some(0x3e00 as f64)
        );
        let shrink = resize_during(ctx, buf, 2, number(0));
        let result = invoke(ctx, view, "getInt8", &[shrink]);
        error(ctx, result, ErrorKind::Range);
        assert_eq!(number_prop(ctx, view, "byteLength"), 0.0);
        let grow = root(ctx, number(16));
        call_method(ctx, buf, "resize", &[grow]);
        assert_eq!(number_prop(ctx, view, "byteLength"), 14.0);
        let shrink = resize_during(ctx, buf, 1, number(0));
        let result = invoke(ctx, view, "setUint16", &[zero, shrink]);
        error(ctx, result, ErrorKind::Type);
        let id = ctx.object_id(buf).unwrap();
        detach_array_buffer(ctx, id).unwrap();
        let result = invoke(ctx, view, "getFloat16", &[zero]);
        error(ctx, result, ErrorKind::Type);
        assert_eq!(named(ctx, view, "buffer").unwrap(), ctx.get(buf));
    });
}

#[test]
fn typed_array_methods_use_snapshot_lengths_and_live_elements() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let buf = buffer(ctx, "ArrayBuffer", 4, 16);
        let view = construct(ctx, "Int8Array", &[buf]);
        let seven = root(ctx, number(7));
        call_method(ctx, view, "fill", &[seven]);
        let grow = resize_during(ctx, buf, 6, number(4));
        assert_eq!(invoke(ctx, view, "at", &[grow]).unwrap(), Value::UNDEFINED);
        let shrink = resize_during(ctx, buf, 2, Value::int32(-1));
        assert_eq!(
            invoke(ctx, view, "lastIndexOf", &[seven, shrink])
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        let minus = root(ctx, Value::int32(-1));
        let changed = call_method(ctx, view, "with", &[minus, minus]);
        let text_value = invoke(ctx, changed, "join", &[]).unwrap();
        assert_eq!(text(ctx, text_value), "7,-1");
        let three = root(ctx, Value::int32(3));
        let ctor = global(ctx, "Int16Array");
        let arr = call_method(ctx, ctor, "of", &[three, seven, minus]);
        let sorted = call_method(ctx, arr, "toSorted", &[]);
        let text_value = invoke(ctx, sorted, "join", &[]).unwrap();
        assert_eq!(text(ctx, text_value), "-1,3,7");
        let text_value = invoke(ctx, arr, "join", &[]).unwrap();
        assert_eq!(text(ctx, text_value), "3,7,-1");
        let one = root(ctx, number(1));
        let sub = call_method(ctx, arr, "subarray", &[one]);
        assert_eq!(
            named(ctx, sub, "buffer").unwrap(),
            named(ctx, arr, "buffer").unwrap()
        );
        assert_eq!(number_prop(ctx, sub, "byteOffset"), 2.0);
    });
}

#[test]
fn base64_and_hex_enforce_alphabet_chunk_and_target_bounds() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let ctor = global(ctx, "Uint8Array");
        let input = ctx.new_string("00FFfe").unwrap();
        let view = call_method(ctx, ctor, "fromHex", &[input]);
        let str = invoke(ctx, view, "toHex", &[]).unwrap();
        assert_eq!(text(ctx, str), "00fffe");
        let str = invoke(ctx, view, "toBase64", &[]).unwrap();
        assert_eq!(text(ctx, str), "AP/+");
        let options = ctx.new_object().unwrap();
        let url = ctx.new_string("base64url").unwrap();
        put(
            ctx,
            options,
            "alphabet",
            ctx.get(url),
            PropFlags::DEFAULT_DATA,
        )
        .unwrap();
        let str = invoke(ctx, view, "toBase64", &[options]).unwrap();
        assert_eq!(text(ctx, str), "AP_-");
        let strict = ctx.new_string("strict").unwrap();
        put(
            ctx,
            options,
            "lastChunkHandling",
            ctx.get(strict),
            PropFlags::DEFAULT_DATA,
        )
        .unwrap();
        let invalid = ctx.new_string("Zh==").unwrap();
        let result = invoke(ctx, ctor, "fromBase64", &[invalid, options]);
        error(ctx, result, ErrorKind::Syntax);
        let input = ctx.new_string(" Zm9v  X").unwrap();
        let result = call_method(ctx, view, "setFromBase64", &[input, options]);
        assert_eq!(number_prop(ctx, result, "read"), 5.0);
        assert_eq!(number_prop(ctx, result, "written"), 3.0);
        let str = invoke(ctx, view, "toHex", &[]).unwrap();
        assert_eq!(text(ctx, str), "666f6f");
        let invalid = ctx.new_string("ffzz").unwrap();
        let result = invoke(ctx, view, "setFromHex", &[invalid]);
        error(ctx, result, ErrorKind::Syntax);
        let str = invoke(ctx, view, "toHex", &[]).unwrap();
        assert_eq!(text(ctx, str), "ff6f6f");
        let stop = ctx.new_string("stop-before-partial").unwrap();
        put(
            ctx,
            options,
            "lastChunkHandling",
            ctx.get(stop),
            PropFlags::DEFAULT_DATA,
        )
        .unwrap();
        let partial = ctx.new_string("Zm9v YQ=").unwrap();
        let out = call_method(ctx, ctor, "fromBase64", &[partial, options]);
        let str = invoke(ctx, out, "toHex", &[]).unwrap();
        assert_eq!(text(ctx, str), "666f6f");
    });
}

#[test]
fn atomics_rw_validates_views_and_returns_unwrapped_store_value() {
    let mut rt = runtime();
    rt.with_scope(rt.default_realm(), |ctx| {
        let atomics = global(ctx, "Atomics");
        let one = root(ctx, number(1));
        let zero = root(ctx, number(0));
        let input = root(ctx, Value::int32(258));
        let ta = construct(ctx, "Int8Array", &[one]);
        assert_eq!(
            invoke(ctx, atomics, "store", &[ta, zero, input])
                .unwrap()
                .as_number(),
            Some(258.0)
        );
        for (name, value, old) in [
            ("add", 3, 2),
            ("sub", 2, 5),
            ("or", 12, 3),
            ("xor", 8, 15),
            ("and", 3, 7),
            ("exchange", -6, 3),
        ] {
            let n = root(ctx, Value::int32(value));
            assert_eq!(
                invoke(ctx, atomics, name, &[ta, zero, n])
                    .unwrap()
                    .as_number(),
                Some(old as f64)
            );
        }
        let neg6 = root(ctx, Value::int32(-6));
        assert_eq!(
            invoke(ctx, atomics, "compareExchange", &[ta, zero, neg6, one])
                .unwrap()
                .as_number(),
            Some(-6.0)
        );
        assert_eq!(
            invoke(ctx, atomics, "load", &[ta, zero])
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        let result = invoke(ctx, atomics, "load", &[ta, one]);
        error(ctx, result, ErrorKind::Range);
        let floats = construct(ctx, "Float16Array", &[one]);
        let result = invoke(ctx, atomics, "load", &[floats, zero]);
        error(ctx, result, ErrorKind::Type);
        let ordinary = construct(ctx, "Int32Array", &[one]);
        let result = invoke(ctx, atomics, "wait", &[ordinary, zero, zero, zero]);
        error(ctx, result, ErrorKind::Type);
        assert_eq!(
            invoke(ctx, atomics, "notify", &[ordinary, zero])
                .unwrap()
                .as_number(),
            Some(0.0)
        );
        let rab = buffer(ctx, "ArrayBuffer", 8, 16);
        let ints = construct(ctx, "BigInt64Array", &[rab]);
        let value = resize_during(ctx, rab, 0, Value::short_bigint(12).unwrap());
        let result = invoke(ctx, atomics, "store", &[ints, zero, value]);
        error(ctx, result, ErrorKind::Range);
    });
}

#[test]
fn shared_growth_and_waiters_are_visible_across_runtimes() {
    let mut rt = runtime();
    let backing = rt.with_scope(rt.default_realm(), |ctx| {
        let shared = buffer(ctx, "SharedArrayBuffer", 4, 32);
        let id = ctx.object_id(shared).unwrap();
        buffer::shared(ctx.heap(), id).unwrap().clone()
    });
    for _ in 0..4 {
        let (done_tx, done_rx) = mpsc::channel();
        let mut children = Vec::new();
        for _ in 0..3 {
            let backing = backing.clone();
            let done_tx = done_tx.clone();
            children.push(std::thread::spawn(move || {
                let mut worker = runtime();
                worker.with_scope(worker.default_realm(), |ctx| {
                    let shared = create_shared_array_buffer_from_arc(ctx, backing).unwrap();
                    let typed = construct(ctx, "Int32Array", &[shared]);
                    let zero = root(ctx, number(0));
                    let timeout = root(ctx, number(5000));
                    let atomics = global(ctx, "Atomics");
                    let result =
                        invoke(ctx, atomics, "wait", &[typed, zero, zero, timeout]).unwrap();
                    assert_eq!(text(ctx, result), "ok");
                    assert_eq!(length(ctx, typed), 8);
                });
                done_tx.send(()).unwrap();
            }));
        }
        rt.with_scope(rt.default_realm(), |ctx| {
            let shared = create_shared_array_buffer_from_arc(ctx, backing.clone()).unwrap();
            let typed = construct(ctx, "Int32Array", &[shared]);
            let bytes = root(ctx, number(32));
            call_method(ctx, shared, "grow", &[bytes]);
            assert_eq!(length(ctx, typed), 8);
            let atomics = global(ctx, "Atomics");
            let zero = root(ctx, number(0));
            let one = root(ctx, number(1));
            let started = Instant::now();
            let mut notified = 0;
            while notified < 3 {
                let (atomics, typed, zero, one) = (
                    ctx.get(atomics),
                    ctx.get(typed),
                    ctx.get(zero),
                    ctx.get(one),
                );
                notified += ctx.with_scope(|ctx| {
                    let atomics = root(ctx, atomics);
                    let typed = root(ctx, typed);
                    let zero = root(ctx, zero);
                    let one = root(ctx, one);
                    invoke(ctx, atomics, "notify", &[typed, zero, one])
                        .unwrap()
                        .as_number()
                        .unwrap() as usize
                });
                assert!(started.elapsed() < Duration::from_secs(4));
                std::thread::yield_now();
            }
        });
        for _ in 0..3 {
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        for child in children {
            child.join().unwrap();
        }
    }
}

#[test]
fn atomics_wait_checks_blocking_permission_and_interrupts() {
    let mut rt = runtime();
    rt.set_can_block(false);
    rt.with_scope(rt.default_realm(), |ctx| {
        let shared = buffer(ctx, "SharedArrayBuffer", 4, 4);
        let typed = construct(ctx, "Int32Array", &[shared]);
        let zero = root(ctx, number(0));
        let atomics = global(ctx, "Atomics");
        let result = invoke(ctx, atomics, "wait", &[typed, zero, zero, zero]);
        error(ctx, result, ErrorKind::Type);
    });
    rt.set_can_block(true);
    rt.with_scope(rt.default_realm(), |ctx| {
        let shared = buffer(ctx, "SharedArrayBuffer", 4, 4);
        let typed = construct(ctx, "Int32Array", &[shared]);
        let zero = root(ctx, number(0));
        let one = root(ctx, number(1));
        let atomics = global(ctx, "Atomics");
        let result = invoke(ctx, atomics, "wait", &[typed, zero, zero, zero]).unwrap();
        assert_eq!(text(ctx, result), "timed-out");
        let result = invoke(ctx, atomics, "wait", &[typed, zero, one, zero]).unwrap();
        assert_eq!(text(ctx, result), "not-equal");
        let start = Instant::now();
        ctx.runtime.set_interrupt_handler(Some(Box::new(move || {
            start.elapsed() >= Duration::from_millis(15)
        })));
        let timeout = root(ctx, number(2000));
        assert_eq!(
            invoke(ctx, atomics, "wait", &[typed, zero, zero, timeout]),
            Err(Abrupt::Interrupted)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        ctx.runtime.set_interrupt_handler(None);
        assert_eq!(
            invoke(ctx, atomics, "notify", &[typed, zero])
                .unwrap()
                .as_number(),
            Some(0.0)
        );
    });
}
