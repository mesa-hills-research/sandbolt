use sandbolt_bytecode::FunctionKind;
use sandbolt_syntax::{Expr, FunctionBody, ParseErrorKind, SourceText, Stmt};

use crate::api::{EvalOptions, display_value, eval_text, parse_options};
use crate::atom::{Atom, PropKey};
use crate::ops::property;
use crate::realm::NativeFnId;
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::value::*;
use crate::vm::exec;

use super::{
    VM_INTERNAL_BASE as BASE, configure_ctor, define_data, install_method, make_native_fn,
};

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
const SELF: NativeFnId = NativeFnId(BASE + 32);

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
    install_function_kinds(ctx)?;
    let async_iterator = ctx.root(Value::object(ctx.realm().async_iterator_proto))?;
    let async_iterator_key = symbol(ctx, crate::symbol::WellKnown::AsyncIterator);
    install_method(
        ctx,
        async_iterator,
        async_iterator_key,
        SELF,
        "[Symbol.asyncIterator]",
        0,
    )?;
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

fn symbol(ctx: &ExecCtx<'_, '_>, symbol: crate::symbol::WellKnown) -> PropKey {
    let symbol = ctx.heap().symbol_registry.well_known(symbol);
    ctx.heap().symbols.get(symbol).unwrap().key()
}

fn install_function_kinds(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let function_proto = ctx.root(Value::object(ctx.realm().function_proto))?;
    let function_ctor = ctx.root(Value::object(ctx.realm().function_ctor))?;
    let tuple = ctx.new_object_with_proto(None)?;
    let tag = symbol(ctx, crate::symbol::WellKnown::ToStringTag);
    for (index, (name, id, instance)) in [
        (
            "GeneratorFunction",
            GENERATOR_FUNCTION,
            Some((ctx.realm().generator_proto, "Generator")),
        ),
        ("AsyncFunction", ASYNC_FUNCTION, None),
        (
            "AsyncGeneratorFunction",
            ASYNC_GENERATOR_FUNCTION,
            Some((ctx.realm().async_generator_proto, "AsyncGenerator")),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let proto = ctx.new_object_with_proto(Some(function_proto))?;
        let ctor = make_native_fn(ctx, id, name, 1, true)?;
        property::set_prototype_of(ctx, ctor, Some(function_ctor))?;
        define_data(
            ctx,
            ctor,
            Atom::get("prototype"),
            proto,
            PropFlags::default(),
        )?;
        define_data(
            ctx,
            proto,
            Atom::get("constructor"),
            ctor,
            PropFlags::CONFIGURABLE,
        )?;
        let name = ctx.new_string(name)?;
        define_data(ctx, proto, tag, name, PropFlags::CONFIGURABLE)?;
        if let Some((instance, name)) = instance {
            let instance = ctx.root(Value::object(instance))?;
            define_data(
                ctx,
                proto,
                Atom::get("prototype"),
                instance,
                PropFlags::CONFIGURABLE,
            )?;
            define_data(
                ctx,
                instance,
                Atom::get("constructor"),
                proto,
                PropFlags::CONFIGURABLE,
            )?;
            let name = ctx.new_string(name)?;
            define_data(ctx, instance, tag, name, PropFlags::CONFIGURABLE)?;
        }
        define_data(
            ctx,
            tuple,
            PropKey::index(index as u32).unwrap(),
            proto,
            PropFlags::default(),
        )?;
    }
    property::private_define(
        ctx,
        ctx.realm().function_proto,
        exec::brand_key(),
        ctx.get(tuple),
    )
}

pub(crate) fn function_prototype(ctx: &ExecCtx<'_, '_>, kind: FunctionKind) -> ObjId {
    function_prototype_for(ctx, ctx.realm_id(), kind)
}

fn function_prototype_for(ctx: &ExecCtx<'_, '_>, realm: RealmId, kind: FunctionKind) -> ObjId {
    let normal = ctx.heap().realm(realm).function_proto;
    let index = match kind {
        FunctionKind::Normal => return normal,
        FunctionKind::Generator => 0,
        FunctionKind::Async => 1,
        FunctionKind::AsyncGenerator => 2,
    };
    property::private_own(ctx, normal, exec::brand_key())
        .and_then(Value::as_object)
        .and_then(|tuple| {
            property::private_own(ctx, tuple, PropKey::index(index).unwrap())?.as_object()
        })
        .unwrap_or(normal)
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
            exec::eval::execute_indirect(ctx, arg)
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
        SELF => Ok(ctx.get(this)),
        _ => Err(ctx.throw_type_error("unknown runtime builtin")),
    }
}

fn function_constructor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    id: NativeFnId,
    args: Args<'s>,
) -> JsResult<Value> {
    let new_target = ctx.new_target();
    let (prefix, kind) = match id {
        GENERATOR_FUNCTION => ("(function* ", FunctionKind::Generator),
        ASYNC_FUNCTION => ("(async function ", FunctionKind::Async),
        ASYNC_GENERATOR_FUNCTION => ("(async function* ", FunctionKind::AsyncGenerator),
        _ => ("(function ", FunctionKind::Normal),
    };
    let mut text: Vec<u16> = prefix.encode_utf16().collect();
    text.extend("anonymous(".encode_utf16());
    let parameter_count = args.len().saturating_sub(1);
    for i in 0..parameter_count {
        if i != 0 {
            text.push(u16::from(b','));
        }
        text.extend(string_units(ctx, args.get(i).unwrap())?);
    }
    let parameter_end = encoded_len(&text);
    text.extend("\n) {\n".encode_utf16());
    let body_start = encoded_len(&text);
    if let Some(body) = args.get(parameter_count) {
        text.extend(string_units(ctx, body)?);
    }
    text.extend("\n})".encode_utf16());
    let parse = parse_options(ctx.runtime, &EvalOptions::default());
    if encoded_len(&text) > parse.max_bytes {
        return Err(ctx.throw_oom());
    }
    let rendered_source = ctx.new_utf16(&text[1..text.len() - 1])?;
    text[prefix.len()..prefix.len() + "anonymous".len()].fill(u16::from(b' '));
    let source = SourceText::from_utf16("<Function>", text);
    validate_function_source(ctx, &source, parse, parameter_end, body_start)?;
    let function = eval_text(ctx, &source, parse)?;
    let function = ctx.root(function)?;
    let function_id = ctx.object_id(function)?;
    let crate::object::ObjectData::BytecodeFunction(data) = &ctx.heap().object(function_id).data
    else {
        unreachable!()
    };
    let code = data.code;
    let ValueKind::String(rendered_source) = ctx.get(rendered_source).decode() else {
        unreachable!()
    };
    ctx.runtime
        .heap
        .bytecode
        .get_mut(code)
        .unwrap()
        .compiled
        .as_mut()
        .unwrap()
        .source = Some(rendered_source);
    let name = ctx.new_string("anonymous")?;
    define_data(
        ctx,
        function,
        Atom::get("name"),
        name,
        PropFlags::CONFIGURABLE,
    )?;
    if let Some(new_target) = new_target {
        let target = ctx.root(Value::object(new_target))?;
        let prototype = property::get_property(ctx, target, Atom::get("prototype"))?;
        let prototype = if let Some(prototype) = prototype.as_object() {
            prototype
        } else {
            let realm = crate::ops::call::get_function_realm(ctx, new_target)?;
            function_prototype_for(ctx, realm, kind)
        };
        let prototype = ctx.root(Value::object(prototype))?;
        property::set_prototype_of(ctx, function, Some(prototype))?;
    }
    Ok(ctx.get(function))
}

fn encoded_len(text: &[u16]) -> usize {
    text.iter()
        .map(|unit| match unit {
            0..=0x7f => 1,
            0x80..=0x7ff => 2,
            _ => 3,
        })
        .sum()
}

fn validate_function_source(
    ctx: &mut ExecCtx<'_, '_>,
    source: &SourceText,
    parse: sandbolt_syntax::ParseOptions,
    parameter_end: usize,
    body_start: usize,
) -> JsResult<()> {
    use crate::object::ErrorKind;
    let program = sandbolt_syntax::parse(source, parse).map_err(|error| match error.kind {
        ParseErrorKind::Syntax => crate::vm::exec::stack::throw_syntax_error(
            ctx,
            &error.message,
            &source.filename,
            error.line,
            error.col,
        ),
        ParseErrorKind::OutOfMemory => ctx.throw_oom(),
        ParseErrorKind::StackOverflow => ctx.stack_overflow(),
    })?;
    let invalid = |ctx: &mut ExecCtx<'_, '_>| {
        ctx.throw_error(ErrorKind::Syntax, "invalid dynamic function source")
    };
    let [statement] = program.body.as_slice() else {
        return Err(invalid(ctx));
    };
    let Stmt::Expression(expression) = program.arena.stmt(*statement).kind else {
        return Err(invalid(ctx));
    };
    let Expr::Parenthesized(expression) = program.arena.expr(expression).kind else {
        return Err(invalid(ctx));
    };
    let Expr::Function(function) = &program.arena.expr(expression).kind else {
        return Err(invalid(ctx));
    };
    let FunctionBody::Block(body) = &function.body else {
        return Err(invalid(ctx));
    };
    if function.span.end as usize != source.as_bytes().len() - 1
        || function
            .params
            .iter()
            .any(|&parameter| program.arena.pattern(parameter).span.end as usize > parameter_end)
        || body
            .iter()
            .any(|&statement| (program.arena.stmt(statement).span.start as usize) < body_start)
    {
        return Err(invalid(ctx));
    }
    Ok(())
}

fn string_units<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Vec<u16>> {
    let text = crate::ops::convert::to_string(ctx, value)?;
    let ValueKind::String(id) = ctx.get(text).decode() else {
        unreachable!()
    };
    Ok(ctx.heap().string_units(id).collect())
}
