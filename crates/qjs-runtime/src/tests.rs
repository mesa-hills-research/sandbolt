use std::cell::Cell;

use crate::atom::{Atom, PropKey};
use crate::object::{ErrorKind, ObjectData};
use crate::ops::{call, compare, convert, property::*, weak::*};
use crate::realm::NativeFn;
use crate::value::StrId;
use crate::vm::MiniOp as Op;
use crate::*;

const LENGTH: PropKey = PropKey::atom(Atom::LENGTH);
const MESSAGE: PropKey = PropKey::atom(Atom::MESSAGE);
const INDEX_0: PropKey = PropKey::index(0).unwrap();
const INDEX_3: PropKey = PropKey::index(3).unwrap();

fn string_id(value: Value) -> StrId {
    let ValueKind::String(id) = value.decode() else {
        panic!("expected a string, got {value:?}")
    };
    id
}

fn error_is(ctx: &mut ExecCtx<'_, '_>, error: Value, kind: ErrorKind, text: &str) {
    let error = ctx.root(error).unwrap();
    let id = ctx.get(error).as_object().unwrap();
    let ObjectData::Error(data) = &ctx.heap().object(id).data else {
        panic!("expected an Error")
    };
    assert_eq!(data.kind, kind);
    let text_id = string_id(get_property(ctx, error, MESSAGE).unwrap());
    assert!(ctx.heap().string_units(text_id).eq(text.encode_utf16()));
}

fn outer<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let callback = args.get(0).unwrap();
    let argument = args.get(1).unwrap();
    let forwarded = ctx.args(&[argument])?;
    let result = call::call(ctx, callback, this, forwarded)?;
    ctx.new_object()?;
    assert_eq!(result, ctx.get(argument));
    assert_eq!(get_property(ctx, this, LENGTH)?, Value::number(1.0));
    assert_eq!(get_property(ctx, this, INDEX_3)?, Value::UNDEFINED);
    Ok(result)
}

fn inner<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let one = ctx.root(Value::int32(1))?;
    assert!(set_property(ctx, this, LENGTH, one)?);
    let marker = ctx.intern_str("marker")?;
    assert!(set_property(ctx, this, marker, one)?);
    let wide = ctx.new_utf16(&[0xd800, 0xe000])?;
    assert_eq!(ctx.heap().string(string_id(ctx.get(wide))).len(), 2);
    let arg = args.get(0).unwrap();
    assert_eq!(args.as_slice(ctx), &[ctx.get(arg)]);
    Ok(ctx.get(arg))
}

static CALLBACKS: [NativeFn; 2] = [outer, inner];

#[test]
fn native_bytecode_native_reentry_keeps_arguments_and_mutated_receiver() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&CALLBACKS);
    let realm = runtime.default_realm();
    let before = runtime.heap().collection_count();
    runtime.with_scope(realm, |ctx| {
        let array = ctx.new_array().unwrap();
        let value = ctx.new_object().unwrap();
        assert!(set_property(ctx, array, INDEX_3, value).unwrap());
        let outer = ctx.new_native(NativeFnId(0), false).unwrap();
        let inner = ctx.new_native(NativeFnId(1), false).unwrap();
        let closure = ctx.args(&[inner]).unwrap();
        let script = ctx
            .new_bytecode(
                &[
                    Op::Capture(0),
                    Op::This,
                    Op::Arg(0),
                    Op::Call(1),
                    Op::Return,
                ],
                Args::empty(),
                closure,
                false,
            )
            .unwrap();
        let args = ctx.args(&[script, value]).unwrap();
        assert_eq!(call::call(ctx, outer, array, args).unwrap(), ctx.get(value));
        let marker = ctx.intern_str("marker").unwrap();
        assert_eq!(get_property(ctx, array, marker).unwrap(), Value::int32(1));
    });
    assert_eq!(runtime.vm.stack_len(), 0);
    assert_eq!(runtime.vm.frame_count(), 0);
    assert_eq!(runtime.vm.reentry_depth(), 0);
    #[cfg(feature = "gc_stress")]
    assert!(runtime.heap().collection_count() - before >= 12);
    #[cfg(not(feature = "gc_stress"))]
    assert_eq!(runtime.heap().collection_count(), before);
}

static TOO_LARGE: [u16; 256 * 1024] = [0xe000; 256 * 1024];
fn too_large(ctx: &mut ExecCtx<'_, '_>, _: Handle<'_>, _: Args<'_>) -> JsResult<Value> {
    ctx.new_utf16(&TOO_LARGE)?;
    Ok(Value::UNDEFINED)
}
static OOM_NATIVE: [NativeFn; 1] = [too_large];

#[test]
fn hard_cap_oom_is_catchable_and_classification_is_not_the_message() {
    let limits = EngineLimits {
        heap_bytes: 96 * 1024,
        ..EngineLimits::default()
    };
    let mut runtime = Runtime::new(limits).unwrap();
    runtime.set_native_dispatch(&OOM_NATIVE);
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let native = ctx.new_native(NativeFnId(0), false).unwrap();
        let this = ctx.root(Value::UNDEFINED).unwrap();
        let constants = ctx.args(&[native]).unwrap();
        let script = ctx
            .new_bytecode(
                &[
                    Op::Try {
                        target: 8,
                        finally: false,
                    },
                    Op::Constant(0),
                    Op::This,
                    Op::Call(0),
                    Op::Pop,
                    Op::EndTry,
                    Op::Push(Value::NULL),
                    Op::Return,
                    Op::Return,
                ],
                constants,
                Args::empty(),
                false,
            )
            .unwrap();
        assert_eq!(
            call::call(ctx, native, this, Args::empty()),
            Err(Abrupt::Thrown)
        );
        assert!(ctx.is_oom());
        let caught = call::call(ctx, script, this, Args::empty()).unwrap();
        assert!(!ctx.is_oom());
        error_is(ctx, caught, ErrorKind::Internal, "out of memory");
        let caught = ctx.root(caught).unwrap();
        let changed = ctx.new_string("changed").unwrap();
        assert!(set_property(ctx, caught, MESSAGE, changed).unwrap());
        assert_eq!(ctx.throw(caught), Abrupt::Thrown);
        assert!(ctx.is_oom());
        assert_eq!(
            get_property(ctx, caught, MESSAGE).unwrap(),
            ctx.get(changed)
        );
        assert!(delete_property(ctx, caught, MESSAGE).unwrap());
        assert!(prevent_extensions(ctx, caught).unwrap());
        assert_eq!(
            call::call(ctx, native, this, Args::empty()),
            Err(Abrupt::Thrown)
        );
        assert!(ctx.is_oom());
        error_is(
            ctx,
            ctx.pending_exception().unwrap(),
            ErrorKind::Internal,
            "out of memory",
        );
        assert!(ctx.heap().ledger.peak_bytes() <= limits.heap_bytes);
    });
}

#[test]
fn failpoint_sweep_reports_oom_until_every_growth_succeeds() {
    let limits = EngineLimits::default();
    let mut successes = 0;
    let mut failures = 0;
    for fail_at in 0..24 {
        let mut runtime = Runtime::new(limits).unwrap();
        let realm = runtime.default_realm();
        runtime.with_scope(realm, |ctx| {
            ctx.heap().ledger.fail_after(Some(fail_at));
            let result = (|| {
                let object = ctx.new_object()?;
                let key = ctx.intern_str("fresh key")?;
                let value = ctx.new_utf16(&[0x1337; 100])?;
                assert!(define_own_property(
                    ctx,
                    object,
                    key,
                    PropertyDescriptor::data(value)
                )?);
                let value = ctx.concat_strings(value, value)?;
                ctx.flatten_string(value)?;
                let retained = ctx.persist(object)?;
                assert_eq!(
                    ctx.runtime.release_persistent(retained),
                    Some(ctx.get(object))
                );
                Ok(())
            })();
            ctx.heap().ledger.fail_after(None);
            match result {
                Ok(()) => successes += 1,
                Err(Abrupt::Thrown) => {
                    failures += 1;
                    assert!(ctx.is_oom());
                    error_is(
                        ctx,
                        ctx.pending_exception().unwrap(),
                        ErrorKind::Internal,
                        "out of memory",
                    );
                    ctx.collect();
                }
                Err(Abrupt::Interrupted) => panic!("unexpected interruption"),
            }
            assert!(ctx.heap().ledger.peak_bytes() <= limits.heap_bytes);
        });
    }
    assert!(
        failures >= 8 && successes != 0,
        "{failures} failpoints, {successes} completions"
    );
}

thread_local! { static HITS: Cell<usize> = const { Cell::new(0) }; }
fn hit<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    HITS.with(|hits| hits.set(hits.get() + 1));
    Ok(args.get(0).map_or(Value::UNDEFINED, |arg| ctx.get(arg)))
}
static HIT_NATIVE: [NativeFn; 1] = [hit];

#[test]
fn interrupted_loop_bypasses_catch_and_finally() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&HIT_NATIVE);
    let mut polls = 0;
    runtime.set_interrupt_handler(Some(Box::new(move || {
        polls += 1;
        polls == 30
    })));
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let native = ctx.new_native(NativeFnId(0), false).unwrap();
        let constants = ctx.args(&[native]).unwrap();
        let script = ctx
            .new_bytecode(
                &[
                    Op::Try {
                        target: 7,
                        finally: true,
                    },
                    Op::Try {
                        target: 3,
                        finally: false,
                    },
                    Op::Jump(2),
                    Op::Constant(0),
                    Op::This,
                    Op::Call(0),
                    Op::Throw,
                    Op::Constant(0),
                    Op::This,
                    Op::Call(0),
                    Op::Return,
                ],
                constants,
                Args::empty(),
                false,
            )
            .unwrap();
        let this = ctx.root(Value::UNDEFINED).unwrap();
        HITS.set(0);
        assert_eq!(
            call::call(ctx, script, this, Args::empty()),
            Err(Abrupt::Interrupted)
        );
        assert_eq!(HITS.get(), 0);
        assert!(ctx.pending_exception().is_none());
    });
    assert_eq!(runtime.vm.frame_count(), 0);
    assert_eq!(runtime.vm.stack_len(), 0);
}

#[test]
fn thrown_values_run_finally_and_propagate_to_catch() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&HIT_NATIVE);
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let native = ctx.new_native(NativeFnId(0), false).unwrap();
        let constants = ctx.args(&[native]).unwrap();
        let script = ctx
            .new_bytecode(
                &[
                    Op::Try {
                        target: 10,
                        finally: false,
                    },
                    Op::Try {
                        target: 4,
                        finally: true,
                    },
                    Op::Push(Value::int32(42)),
                    Op::Throw,
                    Op::Constant(0),
                    Op::This,
                    Op::Call(0),
                    Op::Pop,
                    Op::ResumeThrow,
                    Op::Jump(9),
                    Op::Return,
                ],
                constants,
                Args::empty(),
                false,
            )
            .unwrap();
        let this = ctx.root(Value::UNDEFINED).unwrap();
        HITS.set(0);
        assert_eq!(
            call::call(ctx, script, this, Args::empty()),
            Ok(Value::int32(42))
        );
        assert_eq!(HITS.get(), 1);
    });
}

#[test]
fn javascript_recursion_hits_the_vm_stack_limit() {
    let limits = EngineLimits {
        stack_bytes: 512,
        ..EngineLimits::default()
    };
    let mut runtime = Runtime::new(limits).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let script = ctx
            .new_bytecode(
                &[Op::Callee, Op::This, Op::Call(0), Op::Return],
                Args::empty(),
                Args::empty(),
                false,
            )
            .unwrap();
        let this = ctx.root(Value::UNDEFINED).unwrap();
        assert_eq!(
            call::call(ctx, script, this, Args::empty()),
            Err(Abrupt::Thrown)
        );
        assert!(!ctx.is_oom());
        error_is(
            ctx,
            ctx.pending_exception().unwrap(),
            ErrorKind::Range,
            "Maximum call stack size exceeded",
        );
    });
    assert_eq!(runtime.vm.frame_count(), 0);
    assert_eq!(runtime.vm.stack_len(), 0);
}

fn make_object(ctx: &mut ExecCtx<'_, '_>, _: Handle<'_>, _: Args<'_>) -> JsResult<Value> {
    let object = ctx.new_object()?;
    Ok(ctx.get(object))
}
static MAKE_NATIVE: [NativeFn; 1] = [make_object];

#[test]
fn bytecode_native_and_construct_use_the_callee_realm() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&MAKE_NATIVE);
    let first = runtime.default_realm();
    let second = runtime.create_realm().unwrap();
    let first_proto = runtime.heap().realm(first).object_proto;
    let second_proto = runtime.heap().realm(second).object_proto;
    assert_ne!(first_proto, second_proto);
    assert_ne!(
        runtime.heap().realm(first).global_object,
        runtime.heap().realm(second).global_object
    );
    let function = runtime.with_scope(second, |ctx| {
        let native = ctx.new_native(NativeFnId(0), false).unwrap();
        let constants = ctx.args(&[native]).unwrap();
        let script = ctx
            .new_bytecode(
                &[Op::Constant(0), Op::This, Op::Call(0), Op::Return],
                constants,
                Args::empty(),
                true,
            )
            .unwrap();
        ctx.persist(script).unwrap()
    });
    runtime.with_scope(first, |ctx| {
        let script = ctx.get_persistent(&function).unwrap().unwrap();
        let this = ctx.root(Value::UNDEFINED).unwrap();
        let object = call::call(ctx, script, this, Args::empty()).unwrap();
        let object = ctx.root(object).unwrap();
        assert_eq!(get_prototype_of(ctx, object).unwrap(), Some(second_proto));
        let bound = ctx.bind_function(script, this, Args::empty()).unwrap();
        let constructed = call::construct(ctx, bound, Args::empty(), bound).unwrap();
        let constructed = ctx.root(constructed).unwrap();
        assert_eq!(
            get_prototype_of(ctx, constructed).unwrap(),
            Some(second_proto)
        );
        let local = ctx.new_object().unwrap();
        assert_eq!(get_prototype_of(ctx, local).unwrap(), Some(first_proto));
    });
    runtime.release_persistent(function).unwrap();
}

#[test]
fn weak_maps_reach_ephemeron_fixpoint_and_drop_unreachable_cycles() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let first = ctx.mut_root(Value::UNDEFINED).unwrap();
        let map_b = new_weak_map(ctx).unwrap();
        let map_a = new_weak_map(ctx).unwrap();
        let (map_a, map_b) = (ctx.get(map_a), ctx.get(map_b));
        let (a, b, c) = ctx.with_scope(|ctx| {
            let map_a = ctx.root(map_a).unwrap();
            let map_b = ctx.root(map_b).unwrap();
            let a = ctx.new_object().unwrap();
            let b = ctx.new_object().unwrap();
            let c = ctx.new_object().unwrap();
            weak_map_set(ctx, map_b, b, c).unwrap();
            weak_map_set(ctx, map_a, a, b).unwrap();
            weak_map_set(ctx, map_b, c, a).unwrap();
            (
                ctx.get(a).as_object().unwrap(),
                ctx.get(b).as_object().unwrap(),
                ctx.get(c).as_object().unwrap(),
            )
        });
        ctx.set(&first, Value::object(a));
        ctx.collect();
        assert!(ctx.heap().objects.get_weak(b).is_some());
        assert!(ctx.heap().objects.get_weak(c).is_some());
        ctx.set(&first, Value::UNDEFINED);
        ctx.collect();
        for id in [a, b, c] {
            assert!(ctx.heap().objects.get_weak(id).is_none());
        }
    });
}

#[test]
fn symbols_are_weak_keys_and_deref_keeps_the_target_for_the_job() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let map = new_weak_map(ctx).unwrap();
        let map = ctx.get(map);
        let (reference, sym) = ctx.with_scope(|ctx| {
            let map = ctx.root(map).unwrap();
            let symbol = ctx.new_symbol(None, false).unwrap();
            let reference = new_weak_ref(ctx, symbol).unwrap();
            let value = ctx.root(Value::int32(7)).unwrap();
            weak_map_set(ctx, map, symbol, value).unwrap();
            assert_eq!(weak_map_get(ctx, map, symbol).unwrap(), Value::int32(7));
            (ctx.get(reference), ctx.get(symbol))
        });
        let reference = ctx.root(reference).unwrap();
        ctx.collect();
        assert_eq!(weak_ref_deref(ctx, reference).unwrap(), sym);
        ctx.clear_kept_objects();
        assert_eq!(weak_ref_deref(ctx, reference).unwrap(), sym);
        ctx.collect();
        assert_eq!(weak_ref_deref(ctx, reference).unwrap(), sym);
        ctx.clear_kept_objects();
        ctx.collect();
        assert_eq!(weak_ref_deref(ctx, reference).unwrap(), Value::UNDEFINED);
        let name = ctx.new_string("registered").unwrap();
        let registered = ctx.symbol_for(name).unwrap();
        let again = ctx.symbol_for(name).unwrap();
        assert_eq!(ctx.get(again), ctx.get(registered));
        let map = ctx.root(map).unwrap();
        let value = ctx.root(Value::int32(7)).unwrap();
        assert_eq!(
            weak_map_set(ctx, map, registered, value),
            Err(Abrupt::Thrown)
        );
        assert_eq!(new_weak_ref(ctx, registered).err(), Some(Abrupt::Thrown));
    });
}

#[test]
fn finalization_queues_cleanup_without_allocating_or_calling_javascript() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&HIT_NATIVE);
    HITS.set(0);
    let registry = runtime.with_scope(runtime.default_realm(), |ctx| {
        let callback = ctx.new_native(NativeFnId(0), false).unwrap();
        let registry = new_finalization_registry(ctx, callback).unwrap();
        let persistent = ctx.persist(registry).unwrap();
        let raw = ctx.get(registry);
        ctx.with_scope(|ctx| {
            let registry = ctx.root(raw).unwrap();
            let target = ctx.new_object().unwrap();
            let held = ctx.new_string("hold me").unwrap();
            finalization_register(ctx, registry, target, held, None).unwrap();
        });
        persistent
    });
    runtime.heap().ledger.fail_after(Some(0));
    runtime.collect();
    assert_eq!(runtime.queued_jobs(), 1);
    assert_eq!(HITS.get(), 0);
    let job = runtime.job_queue[0];
    assert!(
        runtime
            .heap()
            .string_units(string_id(job.argument))
            .eq("hold me".encode_utf16())
    );
    assert_eq!(
        runtime.heap().ledger.charge_bytes(1),
        Err(crate::ledger::AllocError::Failpoint)
    );
    runtime.release_persistent(registry).unwrap();
    runtime.collect();
    assert_eq!(runtime.run_one_job(), Ok(true));
    assert_eq!(runtime.run_one_job(), Ok(false));
    assert_eq!(HITS.get(), 1);
}

#[test]
fn ropes_flatten_in_place_without_losing_surrogates() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let head = ctx.new_string("e\u{e9}").unwrap();
        let tail = ctx.new_utf16(&[0xd800, 0xdc00, 0xdfff]).unwrap();
        let string = ctx.concat_strings(head, tail).unwrap();
        let id = string_id(ctx.get(string));
        assert_eq!(ctx.heap().string(id).depth(), 1);
        ctx.flatten_string(string).unwrap();
        assert_eq!(ctx.heap().string(id).depth(), 0);
        assert!(
            ctx.heap()
                .string_units(id)
                .eq([101, 233, 0xd800, 0xdc00, 0xdfff])
        );
        let mut rope = tail;
        for _ in 0..80 {
            rope = ctx.concat_strings(head, rope).unwrap();
        }
        assert_eq!(ctx.heap().string(string_id(ctx.get(rope))).len(), 163);
    });
}

#[test]
fn primitive_string_length_read_does_not_allocate_a_wrapper() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let string = ctx.new_string("😀").unwrap();
        let objects = ctx.heap().objects.len();
        assert_eq!(
            get_property(ctx, string, LENGTH).unwrap().as_number(),
            Some(2.0)
        );
        assert_eq!(ctx.heap().objects.len(), objects);
    });
}

#[test]
fn numeric_inline_keys_do_not_allocate_strings() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        for (number, index) in [(Value::int32(3), 3), (Value::number(-0.0), 0)] {
            let value = ctx.root(number).unwrap();
            let strings = ctx.heap().strings.len();
            assert_eq!(
                convert::to_property_key(ctx, value).unwrap(),
                PropKey::index(index).unwrap()
            );
            assert_eq!(ctx.heap().strings.len(), strings);
        }
    });
}

#[test]
fn ordered_keys_fast_array_downgrade_and_prototype_invalidation() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    let realm = runtime.default_realm();
    let other = runtime.create_realm().unwrap();
    runtime.with_scope(realm, |ctx| {
        let array = ctx.new_array().unwrap();
        let value = ctx.root(Value::int32(42)).unwrap();
        let name = ctx.intern_str("named").unwrap();
        let sym = ctx.new_symbol(None, false).unwrap();
        let sym = convert::to_property_key(ctx, sym).unwrap();
        let large = ctx.intern_str("2147483648").unwrap();
        for key in [INDEX_3, sym, name, INDEX_0, large] {
            assert!(set_property(ctx, array, key, value).unwrap());
        }
        let keys = own_property_keys(ctx, array).unwrap();
        assert_eq!(&*keys, &[INDEX_0, INDEX_3, large, LENGTH, name, sym]);
        let id = ctx.get(array).as_object().unwrap();
        assert!(
            !ctx.heap()
                .object(id)
                .flags
                .contains(crate::object::ObjectFlags::FAST_ARRAY)
        );
        let proto = ctx.root(Value::object(ctx.realm().array_proto)).unwrap();
        assert!(set_property(ctx, proto, INDEX_0, value).unwrap());
        assert!(!ctx.realm().std_array_prototype_intact);
        assert!(ctx.heap().realm(other).std_array_prototype_intact);
    });
}

#[test]
fn numeric_abstract_operations_keep_signed_zero_and_bigint_precision() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let neg_zero = ctx.root(Value::number(-0.0)).unwrap();
        let zero = ctx.root(Value::int32(0)).unwrap();
        assert!(!compare::same_value(ctx, neg_zero, zero));
        assert!(compare::same_value_zero(ctx, neg_zero, zero));
        let bigint = ctx.root(Value::short_bigint(0x1234).unwrap()).unwrap();
        let text = ctx.new_string(" \u{feff}0x1234\n").unwrap();
        assert!(compare::abstract_eq(ctx, bigint, text).unwrap());
        assert_eq!(convert::to_number(ctx, text).unwrap(), 0x1234 as f64);
        let fractional = ctx.root(Value::number(-0.5)).unwrap();
        assert_eq!(
            compare::abstract_relational(ctx, zero, fractional, false).unwrap(),
            Some(false)
        );
        let rest = ctx.root(Value::number(-1.0)).unwrap();
        assert_eq!(convert::to_uint32(ctx, rest).unwrap(), u32::MAX);
        let boxed = convert::to_object(ctx, rest).unwrap();
        assert_eq!(convert::to_number(ctx, boxed).unwrap(), -1.0);
        let object = ctx.new_object().unwrap();
        let text = convert::to_string(ctx, object).unwrap();
        assert!(
            ctx.heap()
                .string_units(string_id(ctx.get(text)))
                .eq("[object Object]".encode_utf16())
        );
    });
}

#[test]
fn promise_fulfillment_keeps_its_callback_and_argument_until_the_job() {
    use crate::ops::promise::*;
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&HIT_NATIVE);
    HITS.set(0);
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let promise = new_promise(ctx).unwrap();
        let raw_promise = ctx.get(promise);
        ctx.with_scope(|ctx| {
            let promise = ctx.root(raw_promise).unwrap();
            let callback = ctx.new_native(NativeFnId(0), false).unwrap();
            react_to_fulfillment(ctx, promise, callback).unwrap();
        });
        ctx.collect();
        let result = ctx.new_string("ready").unwrap();
        fulfill(ctx, promise, result).unwrap();
        fulfill(ctx, promise, result).unwrap();
    });
    assert_eq!(runtime.queued_jobs(), 1);
    assert_eq!(HITS.get(), 0);
    runtime.collect();
    assert!(
        runtime
            .heap()
            .string_units(string_id(runtime.job_queue[0].argument))
            .eq("ready".encode_utf16())
    );
    assert_eq!(runtime.run_one_job(), Ok(true));
    assert_eq!(HITS.get(), 1);
}

fn reenter<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let self_ref = args.get(0).unwrap();
    call::call(ctx, self_ref, self_ref, args)
}
static REENTER_NATIVE: [NativeFn; 1] = [reenter];

#[test]
fn native_recursion_hits_the_reentry_limit() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&REENTER_NATIVE);
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let native = ctx.new_native(NativeFnId(0), false).unwrap();
        let args = ctx.args(&[native]).unwrap();
        assert_eq!(call::call(ctx, native, native, args), Err(Abrupt::Thrown));
        error_is(
            ctx,
            ctx.pending_exception().unwrap(),
            ErrorKind::Range,
            "Maximum call stack size exceeded",
        );
    });
    assert_eq!(runtime.vm.reentry_depth(), 0);
}

fn get_trap<'s>(ctx: &mut ExecCtx<'_, 's>, _this: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let key = string_id(ctx.get(args.get(1).unwrap()));
    assert!(ctx.heap().string_units(key).eq("read me".encode_utf16()));
    let receiver = args.get(2).unwrap();
    let id = ctx.object_id(receiver)?;
    assert!(matches!(ctx.heap().object(id).data, ObjectData::Proxy(_)));
    ctx.new_string("allocation inside trap")?;
    Ok(Value::int32(42))
}
static GET_NATIVE: [NativeFn; 1] = [get_trap];

#[test]
fn proxy_get_trap_receives_the_original_receiver_and_cannot_change_frozen_data() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.set_native_dispatch(&GET_NATIVE);
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let target = ctx.new_object().unwrap();
        let handler = ctx.new_object().unwrap();
        let trap = ctx.new_native(NativeFnId(0), false).unwrap();
        let get = ctx.intern_str("get").unwrap();
        assert!(set_property(ctx, handler, get, trap).unwrap());
        let key = ctx.intern_str("read me").unwrap();
        let proxy = new_proxy(ctx, target, handler).unwrap();
        assert_eq!(get_property(ctx, proxy, key).unwrap(), Value::int32(42));
        let other = ctx.root(Value::int32(1)).unwrap();
        assert!(
            define_own_property(
                ctx,
                target,
                key,
                PropertyDescriptor {
                    value: Some(other),
                    ..PropertyDescriptor::default()
                }
            )
            .unwrap()
        );
        assert_eq!(get_property(ctx, proxy, key), Err(Abrupt::Thrown));
        revoke_proxy(ctx, proxy).unwrap();
        assert_eq!(get_property(ctx, proxy, key), Err(Abrupt::Thrown));
    });
}

#[test]
fn html_dda_is_falsy_and_loosely_equal_to_nullish_values() {
    use crate::object::ObjectFlags;
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let object = ctx
            .make_object(
                Some(ctx.realm().object_proto),
                ObjectFlags::IS_HTML_DDA,
                |_| Ok(ObjectData::Ordinary),
            )
            .unwrap();
        assert!(!convert::to_boolean(ctx, object));
        assert_eq!(convert::typeof_str(ctx, object), "undefined");
        for nullish in [Value::NULL, Value::UNDEFINED] {
            let nullish = ctx.root(nullish).unwrap();
            assert!(compare::abstract_eq(ctx, object, nullish).unwrap());
            assert!(compare::abstract_eq(ctx, nullish, object).unwrap());
            assert!(!compare::strict_eq(ctx, object, nullish));
        }
    });
}

#[test]
fn proxy_handler_lookup_uses_the_reentry_limit() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let target = ctx.new_object().unwrap();
        let mut handler = ctx.new_object().unwrap();
        for _ in 0..crate::vm::MAX_REENTRY_DEPTH + 3 {
            handler = new_proxy(ctx, target, handler).unwrap();
        }
        assert_eq!(get_property(ctx, handler, MESSAGE), Err(Abrupt::Thrown));
        error_is(
            ctx,
            ctx.pending_exception().unwrap(),
            ErrorKind::Range,
            "Maximum call stack size exceeded",
        );
    });
}

#[test]
fn unchanged_accessor_does_not_allocate() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let object = ctx.new_object().unwrap();
        let getter = ctx.root(Value::UNDEFINED).unwrap();
        let key = ctx.intern_str("accessor").unwrap();
        let desc = PropertyDescriptor {
            get: Some(getter),
            ..PropertyDescriptor::default()
        };
        assert!(define_own_property(ctx, object, key, desc).unwrap());
        while ctx.heap().accessors.len() < ctx.heap().accessors.capacity() {
            let other = ctx.new_object().unwrap();
            assert!(define_own_property(ctx, other, key, desc).unwrap());
        }
        ctx.heap().ledger.fail_after(Some(0));
        assert_eq!(define_own_property(ctx, object, key, desc), Ok(true));
        assert_eq!(
            ctx.heap().ledger.charge_bytes(1),
            Err(crate::ledger::AllocError::Failpoint)
        );
    });
}

#[test]
fn allocation_retries_after_collecting_at_the_cap() {
    let limits = EngineLimits {
        heap_bytes: 96 * 1024,
        ..EngineLimits::default()
    };
    let mut runtime = Runtime::new(limits).unwrap();
    #[cfg(feature = "gc_stress")]
    runtime.set_gc_stress_interval(usize::MAX);
    runtime.heap().ledger.set_gc_threshold(usize::MAX);
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let ledger = &ctx.heap().ledger;
        let remaining = limits.heap_bytes - ledger.reserve_bytes() - ledger.used_bytes();
        let units = &TOO_LARGE[..remaining * 3 / 8];
        let dead = ctx.with_scope(|ctx| {
            let value = ctx.new_utf16(units).unwrap();
            string_id(ctx.get(value))
        });
        let before = ctx.heap().collection_count();
        assert!(ctx.heap().strings.get_weak(dead).is_some());
        let live = ctx.new_utf16(units).unwrap();
        assert_eq!(ctx.heap().collection_count(), before + 1);
        assert!(ctx.heap().strings.get_weak(dead).is_none());
        assert_eq!(
            ctx.heap().string(string_id(ctx.get(live))).len() as usize,
            units.len()
        );
        assert!(ctx.heap().ledger.peak_bytes() <= limits.heap_bytes);
    });
}

#[test]
fn collection_prunes_weak_atoms_and_shape_transitions() {
    let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
    runtime.with_scope(runtime.default_realm(), |ctx| {
        let anchor = ctx.new_object().unwrap();
        let anchor_id = ctx.object_id(anchor).unwrap();
        let base = ctx.heap().object(anchor_id).shape;
        let (atom, evolved) = ctx.with_scope(|ctx| {
            let object = ctx.new_object().unwrap();
            let key = ctx.intern_str("temporary key").unwrap();
            let value = ctx.root(Value::NULL).unwrap();
            assert!(set_property(ctx, object, key, value).unwrap());
            let crate::atom::KeyKind::String(atom) = key.decode() else {
                panic!("expected atom")
            };
            let id = ctx.object_id(object).unwrap();
            (atom, ctx.heap().object(id).shape)
        });
        assert_eq!(ctx.heap().shape(base).transitions.len(), 1);
        assert!(ctx.heap().atoms.dynamic_string(atom).is_some());
        ctx.collect();
        assert!(ctx.heap().shape(base).transitions.is_empty());
        assert!(ctx.heap().shapes.get_weak(evolved).is_none());
        assert!(ctx.heap().atoms.dynamic_string(atom).is_none());
    });
}
