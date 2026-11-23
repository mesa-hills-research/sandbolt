use crate::atom::Atom;
use crate::builtins::{configure_ctor, install_accessor};
use crate::ledger::TrackedBox;
use crate::object::{ErrorData, ErrorKind, ObjectData, ObjectFlags};
use crate::realm::Realm;
use crate::value::{ObjId, ValueKind};

use super::*;

const CTORS: &[(u16, ErrorKind, u32)] = &[
    (0x60, ErrorKind::Error, 1),
    (0x61, ErrorKind::Eval, 1),
    (0x62, ErrorKind::Range, 1),
    (0x63, ErrorKind::Reference, 1),
    (0x64, ErrorKind::Syntax, 1),
    (0x65, ErrorKind::Type, 1),
    (0x66, ErrorKind::Uri, 1),
    (0x67, ErrorKind::Internal, 1),
    (0x68, ErrorKind::Aggregate, 2),
    (0x69, ErrorKind::Suppressed, 3),
];
const IS_ERROR: u16 = 0x6a;
const TO_STRING: u16 = 0x6b;
const STACK_GET: u16 = 0x6c;
const STACK_SET: u16 = 0x6d;

fn ctor_id(realm: &Realm, kind: ErrorKind) -> ObjId {
    match kind {
        ErrorKind::Error => realm.error_ctor,
        ErrorKind::Eval => realm.eval_error_ctor,
        ErrorKind::Range => realm.range_error_ctor,
        ErrorKind::Reference => realm.reference_error_ctor,
        ErrorKind::Syntax => realm.syntax_error_ctor,
        ErrorKind::Type => realm.type_error_ctor,
        ErrorKind::Uri => realm.uri_error_ctor,
        ErrorKind::Internal => realm.internal_error_ctor,
        ErrorKind::Aggregate => realm.aggregate_error_ctor,
        ErrorKind::Suppressed => realm.suppressed_error_ctor,
    }
}

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    for &(op, kind, length) in CTORS {
        ctx.with_scope(|ctx| {
            let ctor =
                configure_ctor(ctx, ctor_id(ctx.realm(), kind), id(op), kind.name(), length)?;
            if kind != ErrorKind::Error {
                let error_ctor = ctx.root(Value::object(ctx.realm().error_ctor))?;
                if !property::set_prototype_of(ctx, ctor, Some(error_ctor))? {
                    return Err(ctx.throw_type_error("cannot set error constructor prototype"));
                }
            }
            define_global(ctx, kind.name(), ctor)
        })?;
    }
    let ctor = ctx.root(Value::object(ctx.realm().error_ctor))?;
    methods(ctx, ctor, &[(IS_ERROR, "isError", 1)])?;
    let proto = ctx.root(Value::object(ctx.realm().error_proto))?;
    methods(ctx, proto, &[(TO_STRING, "toString", 0)])?;
    install_accessor(
        ctx,
        proto,
        Atom::get("stack"),
        Some((id(STACK_GET), "get stack")),
        Some((id(STACK_SET), "set stack")),
        true,
    )
}

pub(super) fn dispatch<'s>(
    op: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match op {
        0x60..=0x69 => error_constructor(ctx, CTORS[(op - 0x60) as usize].1, args),
        IS_ERROR => {
            let arg = arg(ctx, args, 0)?;
            let is_error = ctx
                .get(arg)
                .as_object()
                .is_some_and(|id| matches!(ctx.heap().object(id).data, ObjectData::Error(_)));
            Ok(Value::bool(is_error))
        }
        TO_STRING => error_to_string(ctx, this),
        STACK_GET => {
            let id = ctx.object_id(this)?;
            Ok(match &ctx.heap().object(id).data {
                ObjectData::Error(error) => error.stack,
                _ => Value::UNDEFINED,
            })
        }
        STACK_SET => {
            let id = ctx.object_id(this)?;
            let value = arg(ctx, args, 0)?;
            if !matches!(ctx.get(value).decode(), ValueKind::String(_)) {
                return Err(ctx.throw_type_error("Error.prototype.stack setter expects a string"));
            }
            if id == ctx.realm().error_proto {
                return Err(ctx.throw_type_error("cannot set stack on Error.prototype"));
            }
            let key = Atom::get("stack");
            if property::get_own_property_descriptor(ctx, this, key)?.is_some() {
                set_or_throw(ctx, this, key, value)?;
            } else {
                put(ctx, this, key, value)?;
            }
            Ok(Value::UNDEFINED)
        }
        _ => Err(ctx.throw_type_error("unknown Error builtin")),
    }
}

fn error_constructor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    kind: ErrorKind,
    args: Args<'s>,
) -> JsResult<Value> {
    let target = ctx
        .new_target()
        .unwrap_or_else(|| ctor_id(ctx.realm(), kind));
    let error = ctx.ordinary_create_from_constructor(
        target,
        |realm| realm.error_prototype(kind),
        ObjectFlags::EXTENSIBLE,
        |heap| {
            Ok(ObjectData::Error(TrackedBox::new(
                &heap.ledger,
                ErrorData {
                    kind,
                    message: Value::UNDEFINED,
                    stack: Value::UNDEFINED,
                },
            )?))
        },
    )?;
    let index = match kind {
        ErrorKind::Aggregate => 1,
        ErrorKind::Suppressed => 2,
        _ => 0,
    };
    let message = arg(ctx, args, index)?;
    if ctx.get(message) != Value::UNDEFINED {
        let message = convert::to_string(ctx, message)?;
        define_data(
            ctx,
            error,
            PropKey::atom(Atom::MESSAGE),
            message,
            method_flags(),
        )?;
    }
    if let Some(options) = args.get(index + 1)
        && ctx.get(options).as_object().is_some()
    {
        let key = Atom::get("cause");
        if property::has_property(ctx, options, key)? {
            let cause = get(ctx, options, key)?;
            define_data(ctx, error, key, cause, method_flags())?;
        }
    }
    match kind {
        ErrorKind::Aggregate => {
            let iterable = arg(ctx, args, 0)?;
            let errors = ctx.new_array()?;
            let value = ctx.get(errors);
            for_each_iterator(ctx, iterable, |ctx, item, index| {
                let array = ctx.root(value)?;
                push_array(ctx, array, index, item)
            })?;
            define_data(ctx, error, Atom::get("errors"), errors, method_flags())?;
        }
        ErrorKind::Suppressed => {
            let primary = arg(ctx, args, 0)?;
            let suppressed = arg(ctx, args, 1)?;
            define_data(ctx, error, Atom::get("error"), primary, method_flags())?;
            let key = ctx.intern_str("suppressed")?;
            define_data(ctx, error, key, suppressed, method_flags())?;
        }
        _ => {}
    }
    Ok(ctx.get(error))
}

fn error_to_string<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    ctx.object_id(this)?;
    let name = string_property(ctx, this, Atom::NAME, "Error")?;
    let message = string_property(ctx, this, Atom::MESSAGE, "")?;
    let name_id = match ctx.get(name).decode() {
        ValueKind::String(id) => id,
        _ => unreachable!(),
    };
    let message_id = match ctx.get(message).decode() {
        ValueKind::String(id) => id,
        _ => unreachable!(),
    };
    if ctx.heap().string(name_id).is_empty() {
        return Ok(ctx.get(message));
    }
    if ctx.heap().string(message_id).is_empty() {
        return Ok(ctx.get(name));
    }
    let separator = ctx.new_string(": ")?;
    let head = ctx.concat_strings(name, separator)?;
    let text = ctx.concat_strings(head, message)?;
    Ok(ctx.get(text))
}

fn string_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: Atom,
    default: &str,
) -> JsResult<Handle<'s>> {
    let value = get(ctx, object, PropKey::atom(key))?;
    if ctx.get(value) == Value::UNDEFINED {
        ctx.new_string(default)
    } else {
        convert::to_string(ctx, value)
    }
}
