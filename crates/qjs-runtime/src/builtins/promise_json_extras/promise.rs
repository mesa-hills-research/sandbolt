use super::*;
use crate::ledger::TrackedBox;
use crate::object::{ErrorData, ErrorKind, PromiseData, Reaction};
use crate::ops::call::{construct, function_realm};
use crate::ops::convert::to_boolean;
use crate::value::{Abrupt, ObjId, RealmId};
use crate::vm::Job;

const CONSTRUCTOR: NativeFnId = NativeFnId(BASE);
const THEN: NativeFnId = NativeFnId(BASE + 1);
const CATCH: NativeFnId = NativeFnId(BASE + 2);
const FINALLY: NativeFnId = NativeFnId(BASE + 3);
const RESOLVE: NativeFnId = NativeFnId(BASE + 4);
const REJECT: NativeFnId = NativeFnId(BASE + 5);
const ALL: NativeFnId = NativeFnId(BASE + 6);
const ALL_SETTLED: NativeFnId = NativeFnId(BASE + 7);
const RACE: NativeFnId = NativeFnId(BASE + 8);
const ANY: NativeFnId = NativeFnId(BASE + 9);
const TRY: NativeFnId = NativeFnId(BASE + 10);
const WITH_RESOLVERS: NativeFnId = NativeFnId(BASE + 11);
const SPECIES: NativeFnId = NativeFnId(BASE + 12);
const RESOLVE_FN: NativeFnId = NativeFnId(BASE + 13);
const REJECT_FN: NativeFnId = NativeFnId(BASE + 14);
const CAPABILITY_EXECUTOR: NativeFnId = NativeFnId(BASE + 15);
const FULFILL_JOB: NativeFnId = NativeFnId(BASE + 16);
const REJECT_JOB: NativeFnId = NativeFnId(BASE + 17);
const THENABLE_JOB: NativeFnId = NativeFnId(BASE + 18);
const THEN_FINALLY: NativeFnId = NativeFnId(BASE + 19);
const CATCH_FINALLY: NativeFnId = NativeFnId(BASE + 20);
const RETURN_VALUE: NativeFnId = NativeFnId(BASE + 21);
const THROW_REASON: NativeFnId = NativeFnId(BASE + 22);
const ALL_ELEMENT: NativeFnId = NativeFnId(BASE + 23);
const SETTLED_FULFILLED: NativeFnId = NativeFnId(BASE + 24);
const SETTLED_REJECTED: NativeFnId = NativeFnId(BASE + 25);
const ANY_ELEMENT: NativeFnId = NativeFnId(BASE + 26);

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let ctor =
        super::super::configure_ctor(ctx, ctx.realm().promise_ctor, CONSTRUCTOR, "Promise", 1)?;
    let proto = ctx.root(Value::object(ctx.realm().promise_proto))?;
    for (name, id, length) in [
        ("then", THEN, 2),
        ("catch", CATCH, 1),
        ("finally", FINALLY, 1),
    ] {
        define_method(ctx, proto, name, id, length)?;
    }
    for (name, id, length) in [
        ("resolve", RESOLVE, 1),
        ("reject", REJECT, 1),
        ("all", ALL, 1),
        ("allSettled", ALL_SETTLED, 1),
        ("race", RACE, 1),
        ("any", ANY, 1),
        ("try", TRY, 1),
        ("withResolvers", WITH_RESOLVERS, 0),
    ] {
        define_method(ctx, ctor, name, id, length)?;
    }
    let species = well_known(ctx, WellKnown::Species);
    super::super::install_accessor(
        ctx,
        ctor,
        species,
        Some((SPECIES, "get [Symbol.species]")),
        None,
        true,
    )?;
    tag(ctx, proto, "Promise")?;
    let global = global(ctx)?;
    data(ctx, global, "Promise", ctor, mutable())
}

pub(super) fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        CONSTRUCTOR => constructor(ctx, args),
        THEN | CATCH | FINALLY => method(ctx, id, this, args),
        RESOLVE | REJECT | TRY | WITH_RESOLVERS => static_method(ctx, id, this, args),
        ALL | ALL_SETTLED | RACE | ANY => combine(ctx, id, this, args),
        SPECIES | RETURN_VALUE => Ok(ctx.get(this)),
        THROW_REASON => Err(ctx.throw(this)),
        CAPABILITY_EXECUTOR => capability_executor(ctx, this, args),
        RESOLVE_FN | REJECT_FN => resolve_fn(ctx, this, args, id == REJECT_FN),
        FULFILL_JOB | REJECT_JOB => reaction_job(ctx, this, args, id == REJECT_JOB),
        THENABLE_JOB => thenable_job(ctx, this),
        THEN_FINALLY | CATCH_FINALLY => finally_callback(ctx, this, args, id == CATCH_FINALLY),
        ALL_ELEMENT | SETTLED_FULFILLED | SETTLED_REJECTED | ANY_ELEMENT => {
            combine_element(ctx, id, this, args)
        }
        _ => Err(ctx.throw_type_error("unknown Promise builtin function")),
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PromiseCapability<'s> {
    pub(crate) promise: Handle<'s>,
    pub(crate) resolve: Handle<'s>,
    pub(crate) reject: Handle<'s>,
}

pub(crate) fn new_promise_capability<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    constructor: Handle<'s>,
) -> JsResult<PromiseCapability<'s>> {
    if !is_constructor(ctx, ctx.get(constructor)) {
        return Err(ctx.throw_type_error("expected a Promise constructor"));
    }
    // Intrinsic Promise has a non-writable, non-configurable prototype, so we
    // can allocate it directly without routing through CAPABILITY_EXECUTOR
    // (which would let an engine OOM during call setup be converted into a
    // rejected promise and then misreported as a missing-resolver TypeError).
    if let Some(ctor_id) = ctx.get(constructor).as_object()
        && let Some(ctor_realm) = function_realm(ctx.heap(), ctor_id)
        && ctx.heap().realm(ctor_realm).promise_ctor == ctor_id
    {
        let proto = ctx.heap().realm(ctor_realm).promise_proto;
        let promise = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
            Ok(ObjectData::Promise(TrackedBox::new(
                ledger,
                PromiseData::default(),
            )?))
        })?;
        return resolving_functions(ctx, promise);
    }
    let state = host(
        ctx,
        HostObjectData::CapabilityExecutor {
            resolve: Value::UNDEFINED,
            reject: Value::UNDEFINED,
        },
    )?;
    let executor = closure(ctx, CAPABILITY_EXECUTOR, state, 2)?;
    let args = ctx.args(&[executor])?;
    let promise = construct(ctx, constructor, args, constructor)?;
    let promise = ctx.root(promise)?;
    let HostObjectData::CapabilityExecutor { resolve, reject } = state_data(ctx, state) else {
        unreachable!()
    };
    if !callable(ctx, resolve) || !callable(ctx, reject) {
        return Err(ctx
            .throw_type_error("Promise constructor did not provide resolve and reject functions"));
    }
    Ok(PromiseCapability {
        promise,
        resolve: ctx.root(resolve)?,
        reject: ctx.root(reject)?,
    })
}

fn is_constructor(ctx: &ExecCtx<'_, '_>, value: Value) -> bool {
    value.as_object().is_some_and(|id| {
        ctx.heap()
            .object(id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
    })
}

fn state_data(ctx: &ExecCtx<'_, '_>, state: Handle<'_>) -> HostObjectData {
    let id = ctx.runtime.vm.stack[state.index()].as_object().unwrap();
    let ObjectData::HostObject(data) = &ctx.heap().object(id).data else {
        unreachable!()
    };
    **data
}

fn update_state<'a>(ctx: &'a mut ExecCtx<'_, '_>, state: Handle<'_>) -> &'a mut HostObjectData {
    let id = ctx.runtime.vm.stack[state.index()].as_object().unwrap();
    let ObjectData::HostObject(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
    else {
        unreachable!()
    };
    data
}

fn capability_executor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let resolve = args.get(0).map_or(Value::UNDEFINED, |arg| ctx.get(arg));
    let reject = args.get(1).map_or(Value::UNDEFINED, |arg| ctx.get(arg));
    let slot = update_state(ctx, state);
    let HostObjectData::CapabilityExecutor {
        resolve: old_resolve,
        reject: old_reject,
    } = *slot
    else {
        unreachable!()
    };
    if old_resolve != Value::UNDEFINED || old_reject != Value::UNDEFINED {
        return Err(ctx.throw_type_error("Promise capability executor called twice"));
    }
    *slot = HostObjectData::CapabilityExecutor { resolve, reject };
    Ok(Value::UNDEFINED)
}

fn resolving_functions<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
) -> JsResult<PromiseCapability<'s>> {
    let state = host(
        ctx,
        HostObjectData::ResolvingPair {
            promise: ctx.get(promise),
            resolved: false,
        },
    )?;
    Ok(PromiseCapability {
        promise,
        resolve: closure(ctx, RESOLVE_FN, state, 1)?,
        reject: closure(ctx, REJECT_FN, state, 1)?,
    })
}

fn constructor<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let new_target = ctx
        .new_target()
        .ok_or_else(|| ctx.throw_type_error("Promise must be called with new"))?;
    let executor = arg(ctx, args, 0)?;
    if !callable(ctx, ctx.get(executor)) {
        return Err(ctx.throw_type_error("Promise executor is not callable"));
    }
    let promise = ctx.ordinary_create_from_constructor(
        new_target,
        |realm| realm.promise_proto,
        ObjectFlags::EXTENSIBLE,
        |heap| {
            Ok(ObjectData::Promise(TrackedBox::new(
                &heap.ledger,
                PromiseData::default(),
            )?))
        },
    )?;
    let capability = resolving_functions(ctx, promise)?;
    let undefined = ctx.root(Value::UNDEFINED)?;
    let call_args = ctx.args(&[capability.resolve, capability.reject])?;
    if let Err(error) = crate::ops::call::call(ctx, executor, undefined, call_args) {
        reject_abrupt(ctx, capability.reject, error)?;
    }
    Ok(ctx.get(promise))
}

fn reason<'s>(ctx: &mut ExecCtx<'_, 's>, error: Abrupt) -> JsResult<Handle<'s>> {
    if error == Abrupt::Interrupted {
        return Err(error);
    }
    let (value, _) = ctx.take_exception().expect("throw without an exception");
    ctx.root(value)
}

fn reject_abrupt<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    reject: Handle<'s>,
    error: Abrupt,
) -> JsResult<Value> {
    let reason = reason(ctx, error)?;
    call_one(ctx, reject, reason)
}

fn call_one<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    function: Handle<'s>,
    arg: Handle<'s>,
) -> JsResult<Value> {
    let undefined = ctx.root(Value::UNDEFINED)?;
    call_with(ctx, function, undefined, &[arg])
}

fn resolve_fn<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    args: Args<'s>,
    reject: bool,
) -> JsResult<Value> {
    let HostObjectData::ResolvingPair { promise, resolved } = update_state(ctx, state) else {
        unreachable!()
    };
    if *resolved {
        return Ok(Value::UNDEFINED);
    }
    *resolved = true;
    let promise = *promise;
    let promise = ctx.root(promise)?;
    let value = arg(ctx, args, 0)?;
    if reject {
        reject_promise(ctx, promise, value)?;
    } else if ctx.get(promise) == ctx.get(value) {
        let error = ctx.throw_type_error("cannot resolve a promise with itself");
        let error = reason(ctx, error)?;
        reject_promise(ctx, promise, error)?;
    } else if ctx.get(value).as_object().is_none() {
        fulfill_promise(ctx, promise, value)?;
    } else {
        let then = match get_named(ctx, value, "then") {
            Ok(then) => then,
            Err(error) => {
                let error = reason(ctx, error)?;
                reject_promise(ctx, promise, error)?;
                return Ok(Value::UNDEFINED);
            }
        };
        if callable(ctx, ctx.get(then)) {
            let job_state = host(
                ctx,
                HostObjectData::ThenableJob {
                    value: ctx.get(value),
                    then: ctx.get(then),
                    promise: ctx.get(promise),
                },
            )?;
            let job = reaction(ctx, THENABLE_JOB, job_state, ctx.get(then))?;
            enqueue(ctx, job, Value::UNDEFINED)?;
        } else {
            fulfill_promise(ctx, promise, value)?;
        }
    }
    Ok(Value::UNDEFINED)
}

fn thenable_job<'s>(ctx: &mut ExecCtx<'_, 's>, state: Handle<'s>) -> JsResult<Value> {
    let HostObjectData::ThenableJob {
        promise,
        then,
        value,
    } = state_data(ctx, state)
    else {
        unreachable!()
    };
    let promise = ctx.root(promise)?;
    let then = ctx.root(then)?;
    let value = ctx.root(value)?;
    let capability = resolving_functions(ctx, promise)?;
    if let Err(error) = call_with(ctx, then, value, &[capability.resolve, capability.reject]) {
        reject_abrupt(ctx, capability.reject, error)?;
    }
    Ok(Value::UNDEFINED)
}

fn promise_id<'s>(ctx: &mut ExecCtx<'_, 's>, promise: Handle<'s>) -> JsResult<ObjId> {
    match ctx.get(promise).as_object() {
        Some(id) if matches!(ctx.heap().object(id).data, ObjectData::Promise(_)) => Ok(id),
        _ => Err(ctx.throw_type_error("expected a Promise")),
    }
}

fn read_promise<'a>(ctx: &'a ExecCtx<'_, '_>, id: ObjId) -> &'a PromiseData {
    let ObjectData::Promise(data) = &ctx.heap().object(id).data else {
        unreachable!()
    };
    data
}

pub(crate) fn fulfill_promise<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<()> {
    settle(ctx, promise, value, false)
}

pub(crate) fn reject_promise<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<()> {
    settle(ctx, promise, value, true)
}

fn settle<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    argument: Handle<'s>,
    reject: bool,
) -> JsResult<()> {
    let id = promise_id(ctx, promise)?;
    let argument = ctx.get(argument);
    let data = read_promise(ctx, id);
    if data.result.is_some() {
        return Ok(());
    }
    let count = if reject {
        data.reject_reactions.len()
    } else {
        data.reactions.len()
    };
    ctx.allocate(|runtime| {
        if count > 0 {
            runtime.reserve_jobs(count)?;
        }
        let ObjectData::Promise(data) = &mut runtime.heap.objects.get_mut(id).unwrap().data else {
            unreachable!()
        };
        let reactions = if reject {
            &data.reject_reactions
        } else {
            &data.reactions
        };
        for reaction in reactions.iter() {
            runtime.job_queue.push_reserved(Job {
                callback: reaction.callback,
                argument,
                realm: reaction.realm,
            });
        }
        data.reactions = TrackedVec::new();
        data.reject_reactions = TrackedVec::new();
        data.result = Some(argument);
        data.rejected = reject;
        Ok(())
    })
}

pub(crate) fn perform_promise_then<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    promise: Handle<'s>,
    on_fulfilled: Handle<'s>,
    on_rejected: Handle<'s>,
    result: Option<PromiseCapability<'s>>,
) -> JsResult<()> {
    let id = promise_id(ctx, promise)?;
    let (resolve, reject) = result.map_or((Value::UNDEFINED, Value::UNDEFINED), |result| {
        (ctx.get(result.resolve), ctx.get(result.reject))
    });
    let state = |handler| HostObjectData::ReactionJob {
        handler,
        resolve,
        reject,
    };
    let fulfilled = host(ctx, state(ctx.get(on_fulfilled)))?;
    let fulfilled = reaction(ctx, FULFILL_JOB, fulfilled, ctx.get(on_fulfilled))?;
    let rejected = host(ctx, state(ctx.get(on_rejected)))?;
    let rejected = reaction(ctx, REJECT_JOB, rejected, ctx.get(on_rejected))?;
    let promise_data = read_promise(ctx, id);
    if let Some(argument) = promise_data.result {
        enqueue(
            ctx,
            if promise_data.rejected {
                rejected
            } else {
                fulfilled
            },
            argument,
        )?;
    } else {
        ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let ObjectData::Promise(data) = &mut heap.objects.get_mut(id).unwrap().data else {
                unreachable!()
            };
            data.reactions.try_reserve(&heap.ledger, 1)?;
            data.reject_reactions.try_reserve(&heap.ledger, 1)?;
            data.reactions.push_reserved(fulfilled);
            data.reject_reactions.push_reserved(rejected);
            Ok(())
        })?;
    }
    let ObjectData::Promise(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        unreachable!()
    };
    data.is_handled = true;
    Ok(())
}

fn reaction<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    state: Handle<'s>,
    handler: Value,
) -> JsResult<Reaction> {
    let realm = handler_realm(ctx, handler);
    let state = ctx.get(state);
    let callback = ctx.runtime.with_scope(realm, |ctx| {
        let state = ctx.root(state)?;
        let native = ctx.new_native(id, false)?;
        let job = ctx.bind_function(native, state, Args::empty())?;
        ctx.object_id(job)
    })?;
    ctx.root(Value::object(callback))?;
    Ok(Reaction { callback, realm })
}

fn handler_realm(ctx: &ExecCtx<'_, '_>, handler: Value) -> RealmId {
    handler
        .as_object()
        .filter(|&id| is_callable(ctx.heap(), id))
        .and_then(|id| function_realm(ctx.heap(), id))
        .unwrap_or_else(|| ctx.realm_id())
}

fn enqueue(ctx: &mut ExecCtx<'_, '_>, reaction: Reaction, argument: Value) -> JsResult<()> {
    ctx.root(argument)?;
    ctx.allocate(|runtime| {
        runtime.reserve_jobs(1)?;
        runtime.job_queue.push_reserved(Job {
            callback: reaction.callback,
            realm: reaction.realm,
            argument,
        });
        Ok(())
    })
}

fn reaction_job<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    args: Args<'s>,
    rejecting: bool,
) -> JsResult<Value> {
    let HostObjectData::ReactionJob {
        handler,
        resolve,
        reject,
    } = state_data(ctx, state)
    else {
        unreachable!()
    };
    let input = arg(ctx, args, 0)?;
    let (function, output) = if callable(ctx, handler) {
        let handler = ctx.root(handler)?;
        let result = call_one(ctx, handler, input);
        if resolve == Value::UNDEFINED {
            return result;
        }
        match result {
            Ok(value) => (resolve, ctx.root(value)?),
            Err(error) => (reject, reason(ctx, error)?),
        }
    } else {
        (if rejecting { reject } else { resolve }, input)
    };
    if function == Value::UNDEFINED {
        return Ok(Value::UNDEFINED);
    }
    let function = ctx.root(function)?;
    call_one(ctx, function, output)
}

fn species_constructor<'s>(ctx: &mut ExecCtx<'_, 's>, promise: Handle<'s>) -> JsResult<Handle<'s>> {
    let default = Value::object(ctx.realm().promise_ctor);
    let constructor = get_named(ctx, promise, "constructor")?;
    if ctx.get(constructor) == Value::UNDEFINED {
        return ctx.root(default);
    }
    if ctx.get(constructor).as_object().is_none() {
        return Err(ctx.throw_type_error("Promise constructor property is not an object"));
    }
    let species = get(ctx, constructor, well_known(ctx, WellKnown::Species))?;
    if matches!(ctx.get(species), Value::NULL | Value::UNDEFINED) {
        return ctx.root(default);
    }
    if !is_constructor(ctx, ctx.get(species)) {
        return Err(ctx.throw_type_error("Promise species is not a constructor"));
    }
    Ok(species)
}

fn method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        THEN => {
            promise_id(ctx, this)?;
            let constructor = species_constructor(ctx, this)?;
            let capability = new_promise_capability(ctx, constructor)?;
            let fulfilled = arg(ctx, args, 0)?;
            let rejected = arg(ctx, args, 1)?;
            perform_promise_then(ctx, this, fulfilled, rejected, Some(capability))?;
            Ok(ctx.get(capability.promise))
        }
        CATCH => {
            let rejected = arg(ctx, args, 0)?;
            let then = get_named(ctx, this, "then")?;
            let undefined = ctx.root(Value::UNDEFINED)?;
            call_with(ctx, then, this, &[undefined, rejected])
        }
        FINALLY => {
            ctx.object_id(this)?;
            let constructor = species_constructor(ctx, this)?;
            let callback = arg(ctx, args, 0)?;
            let (fulfilled, rejected) = if callable(ctx, ctx.get(callback)) {
                let state = host(
                    ctx,
                    HostObjectData::FinallyHandler {
                        constructor: ctx.get(constructor),
                        callback: ctx.get(callback),
                    },
                )?;
                (
                    closure(ctx, THEN_FINALLY, state, 1)?,
                    closure(ctx, CATCH_FINALLY, state, 1)?,
                )
            } else {
                (callback, callback)
            };
            let then = get_named(ctx, this, "then")?;
            call_with(ctx, then, this, &[fulfilled, rejected])
        }
        _ => unreachable!(),
    }
}

fn finally_callback<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    state: Handle<'s>,
    args: Args<'s>,
    rejecting: bool,
) -> JsResult<Value> {
    let HostObjectData::FinallyHandler {
        constructor,
        callback,
    } = state_data(ctx, state)
    else {
        unreachable!()
    };
    let callback = ctx.root(callback)?;
    let undefined = ctx.root(Value::UNDEFINED)?;
    let result = call(ctx, callback, undefined, Args::empty())?;
    let result = ctx.root(result)?;
    let constructor = ctx.root(constructor)?;
    let promise = promise_resolve(ctx, constructor, result)?;
    let input = arg(ctx, args, 0)?;
    let thunk = closure(
        ctx,
        if rejecting {
            THROW_REASON
        } else {
            RETURN_VALUE
        },
        input,
        0,
    )?;
    let then = get_named(ctx, promise, "then")?;
    call_with(ctx, then, promise, &[thunk])
}

pub(crate) fn promise_resolve<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    constructor: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<Handle<'s>> {
    if ctx
        .get(value)
        .as_object()
        .is_some_and(|id| matches!(ctx.heap().object(id).data, ObjectData::Promise(_)))
    {
        let actual = get_named(ctx, value, "constructor")?;
        if ctx.get(actual) == ctx.get(constructor) {
            return Ok(value);
        }
    }
    let capability = new_promise_capability(ctx, constructor)?;
    call_one(ctx, capability.resolve, value)?;
    Ok(capability.promise)
}

fn static_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    constructor: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    if id == RESOLVE {
        ctx.object_id(constructor)?;
        let value = arg(ctx, args, 0)?;
        let result = promise_resolve(ctx, constructor, value)?;
        return Ok(ctx.get(result));
    }
    let capability = new_promise_capability(ctx, constructor)?;
    match id {
        REJECT => {
            let value = arg(ctx, args, 0)?;
            call_one(ctx, capability.reject, value)?;
        }
        TRY => {
            let callback = arg(ctx, args, 0)?;
            let undefined = ctx.root(Value::UNDEFINED)?;
            let forwarded = if args.is_empty() {
                Args::empty()
            } else {
                Args::new(args.start as usize + 1, args.len() - 1)
            };
            match call(ctx, callback, undefined, forwarded) {
                Ok(value) => {
                    let value = ctx.root(value)?;
                    call_one(ctx, capability.resolve, value)?;
                }
                Err(error) => {
                    reject_abrupt(ctx, capability.reject, error)?;
                }
            }
        }
        WITH_RESOLVERS => {
            let result = ctx.new_object()?;
            data(ctx, result, "promise", capability.promise, DATA)?;
            data(ctx, result, "resolve", capability.resolve, DATA)?;
            data(ctx, result, "reject", capability.reject, DATA)?;
            return Ok(ctx.get(result));
        }
        _ => unreachable!(),
    }
    Ok(ctx.get(capability.promise))
}

fn combine<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    constructor: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let capability = new_promise_capability(ctx, constructor)?;
    let result = (|| {
        let resolve = get_named(ctx, constructor, "resolve")?;
        if !callable(ctx, ctx.get(resolve)) {
            return Err(ctx.throw_type_error("Promise.resolve is not callable"));
        }
        let iterable = arg(ctx, args, 0)?;
        let iterator_fn = get(ctx, iterable, well_known(ctx, WellKnown::Iterator))?;
        let iterator = call(ctx, iterator_fn, iterable, Args::empty())?;
        let iterator = ctx.root(iterator)?;
        ctx.object_id(iterator)?;
        let next = get_named(ctx, iterator, "next")?;
        let state = if id == RACE {
            None
        } else {
            let values = ctx.new_array()?;
            Some(host(
                ctx,
                HostObjectData::AggregateState {
                    values: ctx.get(values),
                    settle: ctx.get(if id == ANY {
                        capability.reject
                    } else {
                        capability.resolve
                    }),
                    remaining: 1,
                },
            )?)
        };
        for index in 0.. {
            ctx.check_interrupt()?;
            let (iterator, next, constructor, resolve) = (
                ctx.get(iterator),
                ctx.get(next),
                ctx.get(constructor),
                ctx.get(resolve),
            );
            let state = state.map(|state| ctx.get(state));
            let (fulfill, reject) = (ctx.get(capability.resolve), ctx.get(capability.reject));
            let step = ctx.with_scope(|ctx| {
                let iterator = ctx.root(iterator)?;
                let next = ctx.root(next)?;
                let result = call(ctx, next, iterator, Args::empty())?;
                let result = ctx.root(result)?;
                ctx.object_id(result)?;
                let done = get_named(ctx, result, "done")?;
                if to_boolean(ctx, done) {
                    return Ok(false);
                }
                let value = get_named(ctx, result, "value")?;
                let step = (|| {
                    let constructor = ctx.root(constructor)?;
                    let resolve = ctx.root(resolve)?;
                    let fulfill = ctx.root(fulfill)?;
                    let reject = ctx.root(reject)?;
                    let promise = call_with(ctx, resolve, constructor, &[value])?;
                    let promise = ctx.root(promise)?;
                    let (fulfill, reject) = if let Some(state) = state {
                        let state = ctx.root(state)?;
                        make_element_callbacks(ctx, id, state, index, fulfill, reject)?
                    } else {
                        (fulfill, reject)
                    };
                    let then = get_named(ctx, promise, "then")?;
                    call_with(ctx, then, promise, &[fulfill, reject])?;
                    Ok(())
                })();
                match step {
                    Ok(()) => Ok(true),
                    Err(error) => close_iterator_on_throw(ctx, iterator, error),
                }
            })?;
            if !step {
                break;
            }
        }
        if let Some(state) = state {
            finish_element(ctx, id, state)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        reject_abrupt(ctx, capability.reject, error)?;
    }
    Ok(ctx.get(capability.promise))
}

fn close_iterator_on_throw<'s, T>(
    ctx: &mut ExecCtx<'_, 's>,
    iterator: Handle<'s>,
    error: Abrupt,
) -> JsResult<T> {
    let thrown = reason(ctx, error)?;
    let close = (|| {
        let return_ = get_named(ctx, iterator, "return")?;
        if !matches!(ctx.get(return_), Value::UNDEFINED | Value::NULL) {
            call(ctx, return_, iterator, Args::empty())?;
        }
        Ok(())
    })();
    if close == Err(Abrupt::Interrupted) {
        close?;
    }
    Err(ctx.throw(thrown))
}

fn make_element_callbacks<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    state: Handle<'s>,
    index: u64,
    fulfill: Handle<'s>,
    reject: Handle<'s>,
) -> JsResult<(Handle<'s>, Handle<'s>)> {
    let HostObjectData::AggregateState { values, .. } = state_data(ctx, state) else {
        unreachable!()
    };
    let values = ctx.root(values)?;
    let key = index_key(ctx, index)?;
    let undefined = ctx.root(Value::UNDEFINED)?;
    super::super::define_data(ctx, values, key, undefined, DATA)?;
    let element = host(
        ctx,
        HostObjectData::AggregateElement {
            state: ctx.get(state),
            index,
            called: false,
        },
    )?;
    let HostObjectData::AggregateState { remaining, .. } = update_state(ctx, state) else {
        unreachable!()
    };
    *remaining += 1;
    Ok(match id {
        ALL => (closure(ctx, ALL_ELEMENT, element, 1)?, reject),
        ALL_SETTLED => (
            closure(ctx, SETTLED_FULFILLED, element, 1)?,
            closure(ctx, SETTLED_REJECTED, element, 1)?,
        ),
        ANY => (fulfill, closure(ctx, ANY_ELEMENT, element, 1)?),
        _ => unreachable!(),
    })
}

fn combine_element<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    element: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let HostObjectData::AggregateElement {
        state,
        index,
        called,
    } = update_state(ctx, element)
    else {
        unreachable!()
    };
    if *called {
        return Ok(Value::UNDEFINED);
    }
    *called = true;
    let (state, index) = (*state, *index);
    let state = ctx.root(state)?;
    let HostObjectData::AggregateState { values, .. } = state_data(ctx, state) else {
        unreachable!()
    };
    let values = ctx.root(values)?;
    let mut value = arg(ctx, args, 0)?;
    if id == SETTLED_FULFILLED || id == SETTLED_REJECTED {
        let entry = ctx.new_object()?;
        let (status, name) = if id == SETTLED_REJECTED {
            ("rejected", "reason")
        } else {
            ("fulfilled", "value")
        };
        let status = ctx.new_string(status)?;
        data(ctx, entry, "status", status, DATA)?;
        data(ctx, entry, name, value, DATA)?;
        value = entry;
    }
    let key = index_key(ctx, index)?;
    super::super::define_data(ctx, values, key, value, DATA)?;
    finish_element(ctx, id, state)?;
    Ok(Value::UNDEFINED)
}

fn finish_element<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    state: Handle<'s>,
) -> JsResult<()> {
    let HostObjectData::AggregateState {
        values,
        settle,
        remaining,
    } = update_state(ctx, state)
    else {
        unreachable!()
    };
    *remaining -= 1;
    if *remaining != 0 {
        return Ok(());
    }
    let (values, settle) = (*values, *settle);
    let values = ctx.root(values)?;
    let settle = ctx.root(settle)?;
    let argument = if matches!(id, ANY | ANY_ELEMENT) {
        aggregate_error(ctx, values)?
    } else {
        values
    };
    call_one(ctx, settle, argument)?;
    Ok(())
}

fn aggregate_error<'s>(ctx: &mut ExecCtx<'_, 's>, errors: Handle<'s>) -> JsResult<Handle<'s>> {
    let proto = ctx.realm().aggregate_error_proto;
    let error = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
        Ok(ObjectData::Error(TrackedBox::new(
            ledger,
            ErrorData {
                kind: ErrorKind::Aggregate,
                message: Value::UNDEFINED,
                stack: Value::UNDEFINED,
                frames: TrackedVec::new(),
            },
        )?))
    })?;
    data(ctx, error, "errors", errors, mutable())?;
    Ok(error)
}
