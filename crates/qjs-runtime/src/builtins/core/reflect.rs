use crate::ops::property::PropertyDescriptor;

use super::*;

const APPLY: u16 = 0x80;
const CONSTRUCT: u16 = 0x81;
const DEFINE: u16 = 0x82;
const DELETE: u16 = 0x83;
const GET: u16 = 0x84;
const GET_DESC: u16 = 0x85;
const GET_PROTO: u16 = 0x86;
const HAS: u16 = 0x87;
const IS_EXTENSIBLE: u16 = 0x88;
const OWN_KEYS: u16 = 0x89;
const PREVENT_EXTENSIONS: u16 = 0x8a;
const SET: u16 = 0x8b;
const SET_PROTO: u16 = 0x8c;

const METHODS: &[(u16, &str, u32)] = &[
    (APPLY, "apply", 3),
    (CONSTRUCT, "construct", 2),
    (DEFINE, "defineProperty", 3),
    (DELETE, "deleteProperty", 2),
    (GET, "get", 2),
    (GET_DESC, "getOwnPropertyDescriptor", 2),
    (GET_PROTO, "getPrototypeOf", 1),
    (HAS, "has", 2),
    (IS_EXTENSIBLE, "isExtensible", 1),
    (OWN_KEYS, "ownKeys", 1),
    (PREVENT_EXTENSIONS, "preventExtensions", 1),
    (SET, "set", 3),
    (SET_PROTO, "setPrototypeOf", 2),
];

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    let reflect = ctx.new_object()?;
    methods(ctx, reflect, METHODS)?;
    let tag = ctx.new_string("Reflect")?;
    define_data(
        ctx,
        reflect,
        symbol_key(ctx, WellKnown::ToStringTag),
        tag,
        PropFlags::CONFIGURABLE,
    )?;
    define_global(ctx, "Reflect", reflect)
}

pub(super) fn dispatch<'s>(op: u16, ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let target = arg(ctx, args, 0)?;
    match op {
        APPLY => {
            require_callable(ctx, ctx.get(target))?;
            let receiver = arg(ctx, args, 1)?;
            let list = arg(ctx, args, 2)?;
            let args = super::function::array_like_arguments(ctx, list)?;
            call::call(ctx, target, receiver, args)
        }
        CONSTRUCT => {
            super::function::require_constructor(ctx, ctx.get(target))?;
            let new_target = args.get(2).unwrap_or(target);
            super::function::require_constructor(ctx, ctx.get(new_target))?;
            let list = arg(ctx, args, 1)?;
            let args = super::function::array_like_arguments(ctx, list)?;
            call::construct(ctx, target, args, new_target)
        }
        DEFINE | DELETE | GET | GET_DESC | HAS | SET => {
            ctx.object_id(target)?;
            let key = arg(ctx, args, 1)?;
            let key = convert::to_property_key(ctx, key)?;
            match op {
                DEFINE => {
                    let desc = arg(ctx, args, 2)?;
                    let desc = super::object::to_descriptor(ctx, desc)?;
                    Ok(Value::bool(property::define_own_property(
                        ctx, target, key, desc,
                    )?))
                }
                DELETE => Ok(Value::bool(property::delete_property(ctx, target, key)?)),
                GET => {
                    let receiver = args.get(2).unwrap_or(target);
                    property::get_property_with_receiver(ctx, target, key, receiver)
                }
                GET_DESC => {
                    let desc = property::get_own_property_descriptor(ctx, target, key)?;
                    super::object::from_descriptor(ctx, desc)
                }
                HAS => Ok(Value::bool(property::has_property(ctx, target, key)?)),
                SET => {
                    let value = arg(ctx, args, 2)?;
                    let receiver = args.get(3).unwrap_or(target);
                    Ok(Value::bool(set(ctx, target, key, value, receiver)?))
                }
                _ => unreachable!(),
            }
        }
        GET_PROTO => Ok(super::object::prototype_value(property::get_prototype_of(
            ctx, target,
        )?)),
        IS_EXTENSIBLE => Ok(Value::bool(property::is_extensible(ctx, target)?)),
        OWN_KEYS => {
            let keys = property::own_property_keys(ctx, target)?;
            root_keys(ctx, &keys)?;
            let out = ctx.new_array()?;
            let raw = ctx.get(out);
            for (index, &key) in keys.iter().enumerate() {
                ctx.with_scope(|ctx| {
                    let out = ctx.root(raw)?;
                    let value = convert::key_value(ctx, key)?;
                    push_array(ctx, out, index as u64, value)
                })?;
            }
            Ok(raw)
        }
        PREVENT_EXTENSIONS => Ok(Value::bool(property::prevent_extensions(ctx, target)?)),
        SET_PROTO => {
            ctx.object_id(target)?;
            let proto = arg(ctx, args, 1)?;
            let proto = prototype_arg(ctx, proto)?;
            Ok(Value::bool(property::set_prototype_of(ctx, target, proto)?))
        }
        _ => Err(ctx.throw_type_error("unknown Reflect builtin")),
    }
}

fn set<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
    receiver: Handle<'s>,
) -> JsResult<bool> {
    if ctx.get(target) == ctx.get(receiver) {
        return property::set_property(ctx, target, key, value);
    }
    let cursor = ctx.mut_root(ctx.get(target))?;
    loop {
        ctx.check_interrupt()?;
        if let Some(desc) = property::get_own_property_descriptor(ctx, cursor.handle(), key)? {
            if desc.is_accessor() {
                let setter = desc.set.unwrap_or(Value::UNDEFINED);
                if setter == Value::UNDEFINED {
                    return Ok(false);
                }
                let setter = ctx.root(setter)?;
                let args = ctx.args(&[value])?;
                call::call(ctx, setter, receiver, args)?;
                return Ok(true);
            }
            if desc.writable != Some(true) {
                return Ok(false);
            }
            break;
        }
        let Some(proto) = property::get_prototype_of(ctx, cursor.handle())? else {
            break;
        };
        ctx.set(&cursor, Value::object(proto));
    }
    if ctx.get(receiver).as_object().is_none() {
        return Ok(false);
    }
    let desc = match property::get_own_property_descriptor(ctx, receiver, key)? {
        Some(desc) if desc.is_accessor() || desc.writable != Some(true) => return Ok(false),
        Some(_) => PropertyDescriptor {
            value: Some(value),
            ..PropertyDescriptor::default()
        },
        None => PropertyDescriptor::data(value),
    };
    property::define_own_property(ctx, receiver, key, desc)
}
