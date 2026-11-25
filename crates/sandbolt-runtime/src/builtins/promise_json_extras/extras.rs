use super::*;
use crate::ledger::TrackedBox;
use crate::object::{DomExceptionData, PerformanceData};
use crate::ops::property::{PropertyDescriptor, define_own_property, delete_property};

const DOM_EXCEPTION: NativeFnId = NativeFnId(BASE + 0xa0);
const DOM_NAME: NativeFnId = NativeFnId(BASE + 0xa1);
const DOM_MESSAGE: NativeFnId = NativeFnId(BASE + 0xa2);
const DOM_CODE: NativeFnId = NativeFnId(BASE + 0xa3);
const NOW: NativeFnId = NativeFnId(BASE + 0xa4);
const ATOB: NativeFnId = NativeFnId(BASE + 0xa5);
const BTOA: NativeFnId = NativeFnId(BASE + 0xa6);
const QUEUE_MICROTASK: NativeFnId = NativeFnId(BASE + 0xa7);
const MICROTASK_JOB: NativeFnId = NativeFnId(BASE + 0xa8);

const CODES: [(Option<&str>, &str); 25] = [
    (Some("IndexSizeError"), "INDEX_SIZE_ERR"),
    (None, "DOMSTRING_SIZE_ERR"),
    (Some("HierarchyRequestError"), "HIERARCHY_REQUEST_ERR"),
    (Some("WrongDocumentError"), "WRONG_DOCUMENT_ERR"),
    (Some("InvalidCharacterError"), "INVALID_CHARACTER_ERR"),
    (None, "NO_DATA_ALLOWED_ERR"),
    (
        Some("NoModificationAllowedError"),
        "NO_MODIFICATION_ALLOWED_ERR",
    ),
    (Some("NotFoundError"), "NOT_FOUND_ERR"),
    (Some("NotSupportedError"), "NOT_SUPPORTED_ERR"),
    (Some("InUseAttributeError"), "INUSE_ATTRIBUTE_ERR"),
    (Some("InvalidStateError"), "INVALID_STATE_ERR"),
    (Some("SyntaxError"), "SYNTAX_ERR"),
    (Some("InvalidModificationError"), "INVALID_MODIFICATION_ERR"),
    (Some("NamespaceError"), "NAMESPACE_ERR"),
    (Some("InvalidAccessError"), "INVALID_ACCESS_ERR"),
    (None, "VALIDATION_ERR"),
    (Some("TypeMismatchError"), "TYPE_MISMATCH_ERR"),
    (Some("SecurityError"), "SECURITY_ERR"),
    (Some("NetworkError"), "NETWORK_ERR"),
    (Some("AbortError"), "ABORT_ERR"),
    (Some("URLMismatchError"), "URL_MISMATCH_ERR"),
    (Some("QuotaExceededError"), "QUOTA_EXCEEDED_ERR"),
    (Some("TimeoutError"), "TIMEOUT_ERR"),
    (Some("InvalidNodeTypeError"), "INVALID_NODE_TYPE_ERR"),
    (Some("DataCloneError"), "DATA_CLONE_ERR"),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let global = global(ctx)?;
    let ctor = super::super::configure_ctor(
        ctx,
        ctx.realm().dom_exception_ctor,
        DOM_EXCEPTION,
        "DOMException",
        0,
    )?;
    let proto = ctx.root(Value::object(ctx.realm().dom_exception_proto))?;
    for (name, getter, id) in [
        ("name", "get name", DOM_NAME),
        ("message", "get message", DOM_MESSAGE),
        ("code", "get code", DOM_CODE),
    ] {
        let key = ctx.intern_str(name)?;
        let getter = super::super::make_native_fn(ctx, id, getter, 0, false)?;
        define_own_property(
            ctx,
            proto,
            key,
            PropertyDescriptor {
                get: Some(getter),
                enumerable: Some(true),
                configurable: Some(true),
                ..PropertyDescriptor::default()
            },
        )?;
    }
    tag(ctx, proto, "DOMException")?;
    for (index, (_, name)) in CODES.iter().enumerate() {
        ctx.with_scope(|ctx| {
            let ctor = ctx.root(Value::object(ctx.realm().dom_exception_ctor))?;
            let proto = ctx.root(Value::object(ctx.realm().dom_exception_proto))?;
            let number = ctx.root(Value::int32(index as i32 + 1))?;
            data(ctx, ctor, name, number, PropFlags::ENUMERABLE)?;
            data(ctx, proto, name, number, PropFlags::ENUMERABLE)
        })?;
    }
    data(ctx, global, "DOMException", ctor, mutable())?;

    let performance_id = ctx.realm().performance_proto;
    let performance = ctx.root(Value::object(performance_id))?;
    delete_property(ctx, performance, Atom::get("constructor"))?;
    let start = ctx.realm().time_origin_instant;
    let payload =
        ctx.allocate(|runtime| TrackedBox::new(&runtime.heap.ledger, PerformanceData { start }))?;
    ctx.runtime
        .heap
        .objects
        .get_mut(performance_id)
        .unwrap()
        .data = ObjectData::Performance(payload);
    let now = super::super::make_native_fn(ctx, NOW, "now", 0, false)?;
    data(ctx, performance, "now", now, PropFlags::ENUMERABLE)?;
    let millis = ctx.root(Value::number(ctx.realm().time_origin_epoch_ms))?;
    data(
        ctx,
        performance,
        "timeOrigin",
        millis,
        PropFlags::ENUMERABLE,
    )?;
    data(ctx, global, "performance", performance, DATA)?;
    define_method(ctx, global, "atob", ATOB, 1)?;
    define_method(ctx, global, "btoa", BTOA, 1)?;
    define_method(ctx, global, "queueMicrotask", QUEUE_MICROTASK, 1)
}

pub(super) fn dispatch<'s>(
    id: NativeFnId,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match id {
        DOM_EXCEPTION => construct(ctx, args),
        DOM_NAME | DOM_MESSAGE | DOM_CODE => {
            let object = ctx.object_id(this)?;
            let ObjectData::DomException(exception) = &ctx.heap().object(object).data else {
                return Err(ctx.throw_type_error("expected a DOMException"));
            };
            Ok(match id {
                DOM_NAME => exception.name,
                DOM_MESSAGE => exception.message,
                _ => Value::int32(exception.code),
            })
        }
        NOW => {
            let ObjectData::Performance(performance) =
                &ctx.heap().object(ctx.realm().performance_proto).data
            else {
                unreachable!()
            };
            Ok(Value::number(
                performance.start.elapsed().as_secs_f64() * 1e3,
            ))
        }
        ATOB | BTOA => base64(ctx, args, id == BTOA),
        QUEUE_MICROTASK => {
            let callback = arg(ctx, args, 0)?;
            if !callable(ctx, ctx.get(callback)) {
                return Err(ctx.throw_type_error("expected a callable function"));
            }
            let job = closure(ctx, MICROTASK_JOB, callback, 0)?;
            let undefined = ctx.root(Value::UNDEFINED)?;
            ctx.enqueue_job(job, undefined)?;
            Ok(Value::UNDEFINED)
        }
        MICROTASK_JOB => {
            let global = global(ctx)?;
            call(ctx, this, global, Args::empty())
        }
        _ => Err(ctx.throw_type_error("unknown builtin function")),
    }
}

fn construct<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let target = ctx
        .new_target()
        .ok_or_else(|| ctx.throw_type_error("DOMException must be called with new"))?;
    let object = ctx.ordinary_create_from_constructor(
        target,
        |realm| realm.dom_exception_proto,
        ObjectFlags::EXTENSIBLE,
        |heap| {
            let slots = DomExceptionData {
                name: Value::UNDEFINED,
                message: Value::UNDEFINED,
                code: 0,
            };
            Ok(ObjectData::DomException(TrackedBox::new(
                &heap.ledger,
                slots,
            )?))
        },
    )?;
    let message = arg(ctx, args, 0)?;
    let name = arg(ctx, args, 1)?;
    let message = string_or(ctx, message, "")?;
    let name = string_or(ctx, name, "Error")?;
    initialize_dom_exception(ctx, object, message, name)?;
    Ok(ctx.get(object))
}

fn string_or<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    value: Handle<'s>,
    default: &str,
) -> JsResult<Handle<'s>> {
    if ctx.get(value) == Value::UNDEFINED {
        ctx.new_string(default)
    } else {
        crate::ops::convert::to_string(ctx, value)
    }
}

fn initialize_dom_exception<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    message: Handle<'s>,
    name: Handle<'s>,
) -> JsResult<()> {
    let message = ctx.get(message);
    let name = ctx.get(name);
    let ValueKind::String(name_id) = name.decode() else {
        unreachable!()
    };
    let code = CODES
        .iter()
        .position(|(candidate, _)| {
            candidate.is_some_and(|text| ctx.heap().string_units(name_id).eq(text.encode_utf16()))
        })
        .map_or(0, |index| index as i32 + 1);
    let id = ctx.object_id(object)?;
    let ObjectData::DomException(slots) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
    else {
        unreachable!()
    };
    **slots = DomExceptionData {
        name,
        message,
        code,
    };
    let stack = ctx.new_string("")?;
    data(ctx, object, "stack", stack, mutable())
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, encode: bool) -> JsResult<Value> {
    // Keep the scratch strings charged until the result is rooted.
    let input = arg(ctx, args, 0)?;
    let input = crate::ops::convert::to_string(ctx, input)?;
    let input = units(ctx, ctx.get(input))?;
    let output = if encode {
        encode_base64(ctx, &input)?
    } else {
        decode_base64(ctx, &input)?
    };
    let output = ctx.new_utf16(&output)?;
    Ok(ctx.get(output))
}

fn invalid_character<T>(ctx: &mut ExecCtx<'_, '_>, encode: bool) -> JsResult<T> {
    let message = ctx.new_string(if encode {
        "String contains an invalid character"
    } else {
        "The string to be decoded is not correctly encoded"
    })?;
    let name = ctx.new_string("InvalidCharacterError")?;
    let error = ctx.make_object(
        Some(ctx.realm().dom_exception_proto),
        ObjectFlags::EXTENSIBLE,
        |ledger| {
            Ok(ObjectData::DomException(TrackedBox::new(
                ledger,
                DomExceptionData::default(),
            )?))
        },
    )?;
    initialize_dom_exception(ctx, error, message, name)?;
    Err(ctx.throw(error))
}

fn encode_base64(ctx: &mut ExecCtx<'_, '_>, input: &[u16]) -> JsResult<TrackedVec<u16>> {
    let length = input
        .len()
        .div_ceil(3)
        .checked_mul(4)
        .ok_or_else(|| ctx.throw_oom())?;
    let mut output = TrackedVec::new();
    ctx.allocate(|runtime| output.try_reserve_exact(&runtime.heap.ledger, length))?;
    for (index, group) in input.chunks(3).enumerate() {
        if index % 1024 == 0 {
            ctx.check_interrupt()?;
        }
        if group.iter().any(|&unit| unit > 0xff) {
            return invalid_character(ctx, true);
        }
        let [a, b, c] = [
            group[0],
            *group.get(1).unwrap_or(&0),
            *group.get(2).unwrap_or(&0),
        ];
        let digits = [
            a >> 2,
            (a & 3) << 4 | b >> 4,
            (b & 15) << 2 | c >> 6,
            c & 63,
        ];
        for (i, digit) in digits.into_iter().enumerate() {
            output.push_reserved(u16::from(if i <= group.len() {
                ALPHABET[digit as usize]
            } else {
                b'='
            }));
        }
    }
    Ok(output)
}

fn decode_base64(ctx: &mut ExecCtx<'_, '_>, input: &[u16]) -> JsResult<TrackedVec<u16>> {
    let mut output = TrackedVec::new();
    ctx.allocate(|runtime| {
        output.try_reserve_exact(&runtime.heap.ledger, input.len() / 4 * 3 + 2)
    })?;
    let (mut digits, mut padding, mut pending, mut bits) = (0usize, 0usize, 0u16, 0);
    for (index, &unit) in input.iter().enumerate() {
        if index % 4096 == 0 {
            ctx.check_interrupt()?;
        }
        if matches!(unit, 9 | 10 | 12 | 13 | 32) {
            continue;
        }
        if unit == u16::from(b'=') {
            padding += 1;
            if padding > 2 {
                return invalid_character(ctx, false);
            }
            continue;
        }
        let digit = match unit {
            65..=90 => unit - 65,
            97..=122 => unit - 97 + 26,
            48..=57 => unit - 48 + 52,
            43 => 62,
            47 => 63,
            _ => return invalid_character(ctx, false),
        };
        if padding != 0 {
            return invalid_character(ctx, false);
        }
        digits += 1;
        pending = (pending << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push_reserved((pending >> bits) & 255);
        }
    }
    if digits % 4 == 1 || (padding != 0 && (digits + padding) % 4 != 0) {
        return invalid_character(ctx, false);
    }
    Ok(output)
}
