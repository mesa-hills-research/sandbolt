use qjs_syntax::{ParseGoal, SourceText};

use crate::api::{EvalOptions, display_value, eval_text, parse_options};
use crate::atom::Atom;
use crate::ops::property;
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::value::*;
use crate::vm::exec;

use super::{VM_INTERNAL_BASE as BASE, configure_ctor, define_data, install_method};

pub(crate) const EVAL: NativeFnId = NativeFnId(BASE);
const FUNCTION: NativeFnId = NativeFnId(BASE + 1);
const GENERATOR_FUNCTION: NativeFnId = NativeFnId(BASE + 2);
const ASYNC_FUNCTION: NativeFnId = NativeFnId(BASE + 3);
const ASYNC_GENERATOR_FUNCTION: NativeFnId = NativeFnId(BASE + 4);
const GENERATOR_NEXT: NativeFnId = NativeFnId(BASE + 8);
const GENERATOR_RETURN: NativeFnId = NativeFnId(BASE + 9);
const GENERATOR_THROW: NativeFnId = NativeFnId(BASE + 10);
const ASYNC_GENERATOR_NEXT: NativeFnId = NativeFnId(BASE + 11);
const ASYNC_GENERATOR_RETURN: NativeFnId = NativeFnId(BASE + 12);
const ASYNC_GENERATOR_THROW: NativeFnId = NativeFnId(BASE + 13);
const ASYNC_FROM_SYNC_NEXT: NativeFnId = NativeFnId(BASE + 14);
const ASYNC_FROM_SYNC_RETURN: NativeFnId = NativeFnId(BASE + 15);
const ASYNC_FROM_SYNC_THROW: NativeFnId = NativeFnId(BASE + 16);
const FOR_IN_NEXT: NativeFnId = NativeFnId(BASE + 17);
const HOST: NativeFnId = NativeFnId(BASE + 18);
const PRINT: NativeFnId = NativeFnId(BASE + 19);
pub(crate) const THROW_TYPE_ERROR: NativeFnId = NativeFnId(BASE + 20);
pub(crate) const AWAIT_FULFILLED: NativeFnId = NativeFnId(BASE + 21);
pub(crate) const AWAIT_REJECTED: NativeFnId = NativeFnId(BASE + 22);
pub(crate) const ASYNC_UNWRAP: NativeFnId = NativeFnId(BASE + 23);
pub(crate) const ASYNC_CLOSE_REJECT: NativeFnId = NativeFnId(BASE + 24);
pub(crate) const SYNC_DISPOSER: NativeFnId = NativeFnId(BASE + 25);
pub(crate) const DYNAMIC_IMPORT_JOB: NativeFnId = NativeFnId(BASE + 26);
pub(crate) const MODULE_DONE: NativeFnId = NativeFnId(BASE + 27);
pub(crate) const MODULE_READY: NativeFnId = NativeFnId(BASE + 28);
pub(crate) const MODULE_FAILED: NativeFnId = NativeFnId(BASE + 29);
pub(crate) const MODULE_IMPORT_DONE: NativeFnId = NativeFnId(BASE + 30);
pub(crate) const MODULE_IMPORT_FAILED: NativeFnId = NativeFnId(BASE + 31);

pub fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let ctor = configure_ctor(ctx, ctx.realm().function_ctor, FUNCTION, "Function", 1)?;
    define_data(
        ctx,
        global,
        Atom::get("Function"),
        ctor,
        PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
    )?;
    install_method(ctx, global, Atom::get("eval"), EVAL, "eval", 1)?;
    for (proto, ids) in [
        (
            ctx.realm().generator_proto,
            [GENERATOR_NEXT, GENERATOR_RETURN, GENERATOR_THROW],
        ),
        (
            ctx.realm().async_generator_proto,
            [
                ASYNC_GENERATOR_NEXT,
                ASYNC_GENERATOR_RETURN,
                ASYNC_GENERATOR_THROW,
            ],
        ),
        (
            ctx.realm().async_from_sync_iterator_proto,
            [
                ASYNC_FROM_SYNC_NEXT,
                ASYNC_FROM_SYNC_RETURN,
                ASYNC_FROM_SYNC_THROW,
            ],
        ),
    ] {
        let proto = ctx.root(Value::object(proto))?;
        for (name, id) in ["next", "return", "throw"].into_iter().zip(ids) {
            install_method(ctx, proto, Atom::get(name), id, name, 1)?;
        }
    }
    let proto = ctx.root(Value::object(ctx.realm().for_in_iterator_proto))?;
    install_method(ctx, proto, Atom::get("next"), FOR_IN_NEXT, "next", 0)?;
    Ok(())
}

pub(crate) fn install_host(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let key = ctx.intern_str("__host")?;
    install_method(ctx, global, key, HOST, "__host", 1)
}

pub(crate) fn install_print(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let key = ctx.intern_str("print")?;
    install_method(ctx, global, key, PRINT, "print", 0)
}

pub fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        EVAL => {
            let Some(arg) = args.get(0) else {
                return Ok(Value::UNDEFINED);
            };
            eval_value(ctx, arg, ParseGoal::Script, false)
        }
        FUNCTION | GENERATOR_FUNCTION | ASYNC_FUNCTION | ASYNC_GENERATOR_FUNCTION => {
            function_constructor(ctx, id, args)
        }
        GENERATOR_NEXT
        | GENERATOR_RETURN
        | GENERATOR_THROW
        | ASYNC_GENERATOR_NEXT
        | ASYNC_GENERATOR_RETURN
        | ASYNC_GENERATOR_THROW => exec::resume_generator(ctx, this, args, id.0 - GENERATOR_NEXT.0),
        ASYNC_FROM_SYNC_NEXT | ASYNC_FROM_SYNC_RETURN | ASYNC_FROM_SYNC_THROW => {
            exec::async_from_sync_call(ctx, this, args, id.0 - ASYNC_FROM_SYNC_NEXT.0)
        }
        AWAIT_FULFILLED | AWAIT_REJECTED => {
            exec::coroutines::resume_await(ctx, this, args, id == AWAIT_REJECTED)
        }
        ASYNC_UNWRAP => {
            let arg = match args.get(0) {
                Some(arg) => arg,
                None => ctx.root(Value::UNDEFINED)?,
            };
            exec::iterator_result(ctx, arg, ctx.get(this) == Value::bool(true))
        }
        ASYNC_CLOSE_REJECT => {
            let error = args.get(0).map_or(Value::UNDEFINED, |arg| ctx.get(arg));
            exec::close_original(ctx, this, error).map(|()| Value::UNDEFINED)
        }
        SYNC_DISPOSER => {
            let undefined = ctx.root(Value::UNDEFINED)?;
            match crate::ops::call::call(ctx, this, undefined, Args::empty()) {
                Ok(_) => Ok(Value::UNDEFINED),
                Err(Abrupt::Thrown) => {
                    let reason = ctx.take_exception().unwrap().0;
                    let reason = ctx.root(reason)?;
                    let ctor = ctx.root(Value::object(ctx.realm().promise_ctor))?;
                    let cap =
                        crate::builtins::promise_json_extras::new_promise_capability(ctx, ctor)?;
                    let args = ctx.args(&[reason])?;
                    crate::ops::call::call(ctx, cap.reject, undefined, args)?;
                    Ok(ctx.get(cap.promise))
                }
                Err(error) => Err(error),
            }
        }
        DYNAMIC_IMPORT_JOB => exec::modules::dynamic_job(ctx, args),
        MODULE_DONE | MODULE_READY | MODULE_FAILED => {
            exec::modules::module_job(ctx, id, this, args)
        }
        MODULE_IMPORT_DONE | MODULE_IMPORT_FAILED => exec::modules::finish_import(ctx, this, args),
        FOR_IN_NEXT => {
            let [value, done] = exec::for_in_next(ctx, this)?;
            let value = ctx.root(value)?;
            exec::iterator_result(ctx, value, done == Value::bool(true))
        }
        HOST => {
            let arg = args
                .get(0)
                .ok_or_else(|| ctx.throw_type_error("__host expects a string"))?;
            let ValueKind::String(id) = ctx.get(arg).decode() else {
                return Err(ctx.throw_type_error("__host expects a string"));
            };
            let text = exec::utf16_lossy(ctx.heap(), id);
            let answer = ctx
                .runtime
                .vm
                .host_callback
                .as_mut()
                .map(|callback| callback(&text))
                .ok_or_else(|| ctx.throw_type_error("host callback is not installed"))?;
            ctx.check_interrupt()?;
            let answer = ctx.new_string(&answer)?;
            Ok(ctx.get(answer))
        }
        PRINT => {
            let mut line = String::new();
            for i in 0..args.len() {
                if i != 0 {
                    line.push(' ');
                }
                line.push_str(&display_value(ctx, args.get(i).unwrap())?);
            }
            if let Some(print) = &mut ctx.runtime.vm.print_callback {
                print(&line);
            }
            Ok(Value::UNDEFINED)
        }
        THROW_TYPE_ERROR => Err(ctx.throw_type_error("restricted arguments property")),
        _ => Err(ctx.throw_type_error("unknown runtime builtin")),
    }
}

pub(crate) fn eval_value<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    arg: Handle<'s>,
    goal: ParseGoal,
    strict: bool,
) -> JsResult<Value> {
    let value = ctx.get(arg);
    let ValueKind::String(id) = value.decode() else {
        return Ok(value);
    };
    let opts = EvalOptions {
        strict,
        ..Default::default()
    };
    let parse = qjs_syntax::ParseOptions {
        goal,
        ..parse_options(ctx.runtime, &opts)
    };
    let units: Vec<_> = ctx.heap().string_units(id).collect();
    if units.len().saturating_mul(3) > parse.max_bytes {
        return Err(ctx.throw_oom());
    }
    let source = SourceText::from_utf16(opts.filename, units);
    eval_text(ctx, &source, parse)
}

fn function_constructor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    args: Args<'s>,
) -> JsResult<Value> {
    let new_target = ctx.new_target();
    let mut text: Vec<u16> = match id {
        GENERATOR_FUNCTION => "(function* anonymous(",
        ASYNC_FUNCTION => "(async function anonymous(",
        ASYNC_GENERATOR_FUNCTION => "(async function* anonymous(",
        _ => "(function anonymous(",
    }
    .encode_utf16()
    .collect();
    let parameter_count = args.len().saturating_sub(1);
    for i in 0..parameter_count {
        if i != 0 {
            text.push(u16::from(b','));
        }
        text.extend(string_units(ctx, args.get(i).unwrap())?);
    }
    text.extend("\n) {\n".encode_utf16());
    if let Some(body) = args.get(parameter_count) {
        text.extend(string_units(ctx, body)?);
    }
    text.extend("\n})".encode_utf16());
    let source = SourceText::from_utf16("<Function>", text);
    let parse = parse_options(ctx.runtime, &EvalOptions::default());
    let function = eval_text(ctx, &source, parse)?;
    if let Some(new_target) = new_target {
        let func = ctx.root(function)?;
        let target = ctx.root(Value::object(new_target))?;
        let prototype = property::get_property(ctx, target, Atom::get("prototype"))?;
        if prototype.as_object().is_some() {
            let prototype = ctx.root(prototype)?;
            property::set_prototype_of(ctx, func, Some(prototype))?;
        }
    }
    Ok(function)
}

fn string_units<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Vec<u16>> {
    let text = crate::ops::convert::to_string(ctx, value)?;
    let ValueKind::String(id) = ctx.get(text).decode() else {
        unreachable!()
    };
    Ok(ctx.heap().string_units(id).collect())
}
