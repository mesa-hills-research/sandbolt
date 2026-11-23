use qjs_bytecode::{BuiltinAtom, Instruction, Opcode, Operands};

use crate::atom::{Atom, KeyKind, PropKey};
use crate::object::{ObjectData, ObjectFlags};
use crate::ops::{call, convert, property};
use crate::root::{ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::symbol::WellKnown;
use crate::value::*;
use crate::vm::CallFrame;

use super::{bindings, compiled, define_default, define_flags, key, replace, well_known};

pub(super) fn regexp_literal(
    ctx: &mut ExecCtx<'_, '_>,
    pattern: Value,
    flags: u16,
) -> JsResult<Value> {
    use qjs_regexp::RegExpFlags as F;
    let pattern = ctx.root(pattern)?;
    let mut text = [0; 8];
    let mut length = 0;
    for (bit, ch) in [
        (F::INDICES, b'd'),
        (F::GLOBAL, b'g'),
        (F::IGNORECASE, b'i'),
        (F::MULTILINE, b'm'),
        (F::DOTALL, b's'),
        (F::UNICODE, b'u'),
        (F::UNICODE_SETS, b'v'),
        (F::STICKY, b'y'),
    ] {
        if flags & bit != 0 {
            text[length] = ch;
            length += 1;
        }
    }
    let flags = ctx.new_string(std::str::from_utf8(&text[..length]).unwrap())?;
    regexp(ctx, pattern, flags)
}

fn regexp<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    pattern: Handle<'s>,
    flags: Handle<'s>,
) -> JsResult<Value> {
    crate::builtins::string_re::regexp_literal(ctx, pattern, flags)
}

fn strict_set<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
    strict: bool,
) -> JsResult<()> {
    let boxed = convert::to_object(ctx, object)?;
    if !property::set_property_with_receiver(ctx, boxed, key, value, object)? && strict {
        return Err(ctx.throw_type_error("cannot assign property"));
    }
    Ok(())
}

fn get<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>, key: PropKey) -> JsResult<Value> {
    let boxed = convert::to_object(ctx, value)?;
    property::get_property_with_receiver(ctx, boxed, key, value)
}

fn set_home(ctx: &mut ExecCtx<'_, '_>, function: Value, home: ObjId) {
    let Some(function) = function.as_object() else {
        return;
    };
    if let ObjectData::BytecodeFunction(data) =
        &mut ctx.runtime.heap.objects.get_mut(function).unwrap().data
    {
        data.home_object = Some(home);
    }
}

fn set_name<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    function: Handle<'s>,
    key: PropKey,
    prefix: Option<&str>,
) -> JsResult<()> {
    if ctx.get(function).as_object().is_none() {
        return Ok(());
    }
    let name = match key.decode() {
        KeyKind::Symbol(_) => {
            let value = convert::key_value(ctx, key)?;
            let ValueKind::Symbol(id) = ctx.get(value).decode() else {
                unreachable!()
            };
            match ctx.heap().symbols.get(id).unwrap().description {
                crate::symbol::SymbolDescription::None => ctx.new_string("")?,
                description => {
                    let description = match description {
                        crate::symbol::SymbolDescription::String(id) => {
                            ctx.root(Value::string(id))?
                        }
                        crate::symbol::SymbolDescription::WellKnown(s) => {
                            ctx.new_string(s.description())?
                        }
                        _ => unreachable!(),
                    };
                    let open = ctx.new_string("[")?;
                    let close = ctx.new_string("]")?;
                    let name = ctx.concat_strings(open, description)?;
                    ctx.concat_strings(name, close)?
                }
            }
        }
        _ => convert::key_value(ctx, key)?,
    };
    let name = if let Some(prefix) = prefix {
        let prefix = ctx.new_string(prefix)?;
        ctx.concat_strings(prefix, name)?
    } else {
        name
    };
    define_flags(
        ctx,
        function,
        Atom::get("name"),
        name,
        PropFlags::CONFIGURABLE,
    )
}

pub(super) fn object_op(
    ctx: &mut ExecCtx<'_, '_>,
    frame: CallFrame,
    code: BcId,
    inst: Instruction,
) -> JsResult<()> {
    use Opcode::*;
    let op = inst.opcode;
    let strict = compiled(ctx, code).strict;
    let tail = ctx.runtime.vm.stack.len();
    let atom = match inst.operands {
        Operands::Atom(atom) | Operands::AtomU8(atom, _) => Some(key(ctx, code, atom)),
        _ => None,
    };
    let (drop, values) = ctx.with_scope(|ctx| {
        let h = |n: usize| Handle::at(tail - n);
        let none = |n| (n, [Value::EMPTY; 2]);
        let one = |n, value| (n, [value, Value::EMPTY]);
        Ok(match op {
            GetField | GetField2 | GetLength => one(
                usize::from(op != GetField2),
                get(ctx, h(1), atom.unwrap_or(Atom::get("length")))?,
            ),
            PutField => {
                strict_set(ctx, h(2), atom.unwrap(), h(1), strict)?;
                none(2)
            }
            GetArrayEl | GetArrayEl2 => {
                let key = convert::to_property_key(ctx, h(1))?;
                one(if op == GetArrayEl { 2 } else { 1 }, get(ctx, h(2), key)?)
            }
            PutArrayEl => {
                let key = convert::to_property_key(ctx, h(2))?;
                strict_set(ctx, h(3), key, h(1), strict)?;
                none(3)
            }
            DefineField => {
                define_default(ctx, h(2), atom.unwrap(), h(1))?;
                none(1)
            }
            DefineArrayEl => {
                let key = convert::to_property_key(ctx, h(2))?;
                define_default(ctx, h(3), key, h(1))?;
                none(1)
            }
            GetSuper => one(
                1,
                property::get_prototype_of(ctx, h(1))?.map_or(Value::NULL, Value::object),
            ),
            GetSuperValue => {
                let key = convert::to_property_key(ctx, h(1))?;
                one(
                    3,
                    property::get_property_with_receiver(ctx, h(2), key, h(3))?,
                )
            }
            PutSuperValue => {
                let key = convert::to_property_key(ctx, h(2))?;
                if !property::set_property_with_receiver(ctx, h(3), key, h(1), h(4))? && strict {
                    return Err(ctx.throw_type_error("cannot assign super property"));
                }
                none(4)
            }
            CheckObject => {
                ctx.object_id(h(1))?;
                none(0)
            }
            ToObject => {
                let object = convert::to_object(ctx, h(1))?;
                one(1, ctx.get(object))
            }
            ToPropkey | ToPropkey2 => {
                if op == ToPropkey2
                    && (ctx.get(h(2)) == Value::NULL || ctx.get(h(2)) == Value::UNDEFINED)
                {
                    return Err(ctx.throw_type_error("cannot access property of null or undefined"));
                }
                let key = convert::to_property_key(ctx, h(1))?;
                let value = convert::key_value(ctx, key)?;
                one(1, ctx.get(value))
            }
            In => {
                ctx.object_id(h(1))?;
                let key = convert::to_property_key(ctx, h(2))?;
                one(2, Value::bool(property::has_property(ctx, h(1), key)?))
            }
            Instanceof => one(2, Value::bool(instance_of(ctx, h(2), h(1))?)),
            Delete => {
                let key = convert::to_property_key(ctx, h(1))?;
                let object = convert::to_object(ctx, h(2))?;
                let deleted = property::delete_property(ctx, object, key)?;
                if !deleted && strict {
                    return Err(ctx.throw_type_error("cannot delete property"));
                }
                one(2, Value::bool(deleted))
            }
            GetPrivateField | PutPrivateField | DefinePrivateField => {
                let index = if op == DefinePrivateField { 2 } else { 1 };
                let name = private_key(ctx, h(index))?;
                let object = if op == GetPrivateField { h(2) } else { h(3) };
                let id = ctx.object_id(object)?;
                let existing = property::private_own(ctx, id, name);
                match (op, existing) {
                    (GetPrivateField, Some(value)) => one(2, value),
                    (PutPrivateField, Some(_)) => {
                        property::private_write(ctx, id, name, ctx.get(h(2)));
                        none(3)
                    }
                    (DefinePrivateField, None) => {
                        property::private_define(ctx, id, name, ctx.get(h(1)))?;
                        none(2)
                    }
                    (DefinePrivateField, Some(_)) => {
                        return Err(ctx.throw_type_error("private field already exists"));
                    }
                    _ => return Err(ctx.throw_type_error("object does not have the private field")),
                }
            }
            PrivateIn | CheckBrand => {
                let id = ctx.object_id(h(2))?;
                let has_brand = if op == CheckBrand || ctx.get(h(1)).as_object().is_some() {
                    has_brand(ctx, id, h(1))?
                } else {
                    let name = private_key(ctx, h(1))?;
                    property::private_own(ctx, id, name).is_some()
                };
                if op == CheckBrand {
                    if !has_brand {
                        return Err(ctx.throw_type_error("invalid brand on object"));
                    }
                    none(0)
                } else {
                    one(2, Value::bool(has_brand))
                }
            }
            AddBrand => {
                let home = ctx.object_id(h(1))?;
                let brand = if let Some(brand) = property::private_own(ctx, home, brand_key()) {
                    brand
                } else {
                    let name = ctx.new_string("<brand>")?;
                    let symbol = ctx.new_symbol(Some(name), true)?;
                    let brand = ctx.get(symbol);
                    property::private_define(ctx, home, brand_key(), brand)?;
                    brand
                };
                if let Some(id) = ctx.get(h(2)).as_object() {
                    let brand = ctx.root(brand)?;
                    let key = private_key(ctx, brand)?;
                    if property::private_own(ctx, id, key).is_some() {
                        return Err(ctx.throw_type_error("private brand already exists"));
                    }
                    property::private_define(ctx, id, key, Value::UNDEFINED)?;
                }
                none(2)
            }
            SetName | SetNameComputed => {
                let key = if let Some(key) = atom {
                    key
                } else {
                    convert::to_property_key(ctx, h(2))?
                };
                set_name(ctx, h(1), key, None)?;
                none(0)
            }
            SetProto => {
                let proto = ctx.get(h(1));
                if (proto == Value::NULL || proto.as_object().is_some())
                    && !property::set_prototype_of(ctx, h(2), proto.as_object().map(|_| h(1)))?
                {
                    return Err(ctx.throw_type_error("cannot set prototype"));
                }
                none(1)
            }
            SetHomeObject => {
                let home = ctx.object_id(h(2))?;
                set_home(ctx, ctx.get(h(1)), home);
                none(0)
            }
            DefineMethod | DefineMethodComputed => {
                let (flags, key, object, drop) = match inst.operands {
                    Operands::AtomU8(_, flags) => (flags, atom.unwrap(), h(2), 1),
                    Operands::Unsigned(flags) => {
                        (flags as u8, convert::to_property_key(ctx, h(2))?, h(3), 2)
                    }
                    _ => unreachable!(),
                };
                let kind = flags & 3;
                set_name(
                    ctx,
                    h(1),
                    key,
                    match kind {
                        1 => Some("get "),
                        2 => Some("set "),
                        _ => None,
                    },
                )?;
                let id = ctx.object_id(object)?;
                set_home(ctx, ctx.get(h(1)), id);
                let descriptor = property::PropertyDescriptor {
                    configurable: Some(true),
                    enumerable: Some(flags & 4 != 0),
                    value: (kind == 0).then(|| h(1)),
                    writable: (kind == 0).then_some(true),
                    get: (kind == 1).then(|| h(1)),
                    set: (kind == 2).then(|| h(1)),
                };
                if !property::define_own_property(ctx, object, key, descriptor)? {
                    return Err(ctx.throw_type_error("cannot define method"));
                }
                none(drop)
            }
            DefineClass | DefineClassComputed => {
                let Operands::AtomU8(_, flags) = inst.operands else {
                    unreachable!()
                };
                let key = if op == DefineClassComputed {
                    convert::to_property_key(ctx, h(3))?
                } else {
                    atom.unwrap()
                };
                let [ctor, proto] = define_class(ctx, frame, key, h(2), h(1), flags & 1 != 0)?;
                (2, [ctor, proto])
            }
            CopyDataProperties => {
                let Operands::Unsigned(mask) = inst.operands else {
                    unreachable!()
                };
                copy_data(
                    ctx,
                    h(1 + (mask & 3) as usize),
                    h(1 + ((mask >> 2) & 7) as usize),
                    h(1 + ((mask >> 5) & 7) as usize),
                )?;
                none(0)
            }
            Append => {
                let start = convert::to_length(ctx, h(2))?;
                let end = super::iterators::append(ctx, h(3), start, h(1))?;
                one(2, Value::number(end as f64))
            }
            Regexp => one(2, regexp(ctx, h(2), h(1))?),
            _ => unreachable!(),
        })
    })?;
    let count = values
        .iter()
        .position(|value| *value == Value::EMPTY)
        .unwrap_or(values.len());
    replace(ctx, drop, &values[..count]);
    Ok(())
}

fn private_key<'s>(ctx: &mut ExecCtx<'_, 's>, key: Handle<'s>) -> JsResult<PropKey> {
    let ValueKind::Symbol(id) = ctx.get(key).decode() else {
        return Err(ctx.throw_type_error("invalid private name"));
    };
    Ok(ctx.heap().symbols.get(id).unwrap().key())
}

pub(crate) fn brand_key() -> PropKey {
    PropKey::atom(Atom(BuiltinAtom::PrivateBrand as u32))
}

fn has_brand<'s>(ctx: &mut ExecCtx<'_, 's>, target: ObjId, method: Handle<'s>) -> JsResult<bool> {
    let id = ctx.object_id(method)?;
    let ObjectData::BytecodeFunction(data) = &ctx.heap().object(id).data else {
        return Err(ctx.throw_type_error("invalid private method"));
    };
    let Some(brand) = data
        .home_object
        .and_then(|home| property::private_own(ctx, home, brand_key()))
    else {
        return Err(ctx.throw_type_error("missing private brand"));
    };
    let brand = ctx.root(brand)?;
    let key = private_key(ctx, brand)?;
    Ok(property::private_own(ctx, target, key).is_some())
}

fn define_class<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    frame: CallFrame,
    name: PropKey,
    parent: Handle<'s>,
    function: Handle<'s>,
    extends: bool,
) -> JsResult<[Value; 2]> {
    let parent_value = ctx.get(parent);
    let (parent_ctor, parent_proto) = if !extends || parent_value == Value::NULL {
        let constructor = ctx.root(Value::object(ctx.realm().function_proto))?;
        let proto = if extends {
            None
        } else {
            Some(ctx.root(Value::object(ctx.realm().object_proto))?)
        };
        (constructor, proto)
    } else {
        let id = ctx.object_id(parent)?;
        if !ctx
            .heap()
            .object(id)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR)
        {
            return Err(ctx.throw_type_error("parent class must be a constructor"));
        }
        let proto = property::get_property(ctx, parent, Atom::get("prototype"))?;
        let proto = if proto.as_object().is_some() {
            Some(ctx.root(proto)?)
        } else if proto == Value::NULL {
            None
        } else {
            return Err(ctx.throw_type_error("parent prototype must be an object or null"));
        };
        (parent, proto)
    };
    let proto = ctx.new_object_with_proto(parent_proto)?;
    let function = match ctx.get(function).decode() {
        ValueKind::Bytecode(child) => {
            let value = bindings::closure(ctx, frame, child)?;
            ctx.root(value)?
        }
        _ => function,
    };
    let id = ctx.object_id(function)?;
    let home = ctx.object_id(proto)?;
    let derived = if let ObjectData::BytecodeFunction(f) = &ctx.heap().object(id).data {
        compiled(ctx, f.code).flags.is_derived_class_constructor
    } else {
        false
    };
    let object = ctx.runtime.heap.objects.get_mut(id).unwrap();
    object.flags |= ObjectFlags::IS_CONSTRUCTOR;
    let ObjectData::BytecodeFunction(f) = &mut object.data else {
        return Err(ctx.throw_type_error("invalid class constructor"));
    };
    f.home_object = Some(home);
    f.is_class_constructor = true;
    f.is_derived_constructor = derived;
    f.is_arrow = false;
    if !property::set_prototype_of(ctx, function, Some(parent_ctor))? {
        return Err(ctx.throw_type_error("cannot set class prototype"));
    }
    set_name(ctx, function, name, None)?;
    define_flags(
        ctx,
        proto,
        Atom::get("constructor"),
        function,
        PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
    )?;
    define_flags(
        ctx,
        function,
        Atom::get("prototype"),
        proto,
        PropFlags::default(),
    )?;
    Ok([ctx.get(function), ctx.get(proto)])
}

pub(super) fn get_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<Option<Handle<'s>>> {
    let value = get(ctx, object, key)?;
    if value == Value::UNDEFINED || value == Value::NULL {
        return Ok(None);
    }
    if !value
        .as_object()
        .is_some_and(|id| call::is_callable(ctx.heap(), id))
    {
        return Err(ctx.throw_type_error("property is not callable"));
    }
    ctx.root(value).map(Some)
}

fn instance_of<'s>(ctx: &mut ExecCtx<'_, 's>, lhs: Handle<'s>, rhs: Handle<'s>) -> JsResult<bool> {
    let mut rhs = rhs;
    loop {
        let id = ctx.object_id(rhs)?;
        if let Some(method) = get_method(ctx, rhs, well_known(ctx, WellKnown::HasInstance))? {
            let args = ctx.args(&[lhs])?;
            let result = call::call(ctx, method, rhs, args)?;
            return Ok(convert::to_boolean_value(ctx.heap(), result));
        }
        if let ObjectData::BoundFunction(f) = &ctx.heap().object(id).data {
            rhs = ctx.root(Value::object(f.target))?;
            continue;
        }
        if !call::is_callable(ctx.heap(), id) {
            return Err(ctx.throw_type_error("right operand is not callable"));
        }
        if ctx.get(lhs).as_object().is_none() {
            return Ok(false);
        }
        let prototype = property::get_property(ctx, rhs, Atom::get("prototype"))?;
        let target = prototype
            .as_object()
            .ok_or_else(|| ctx.throw_type_error("prototype is not an object"))?;
        let current = ctx.mut_root(ctx.get(lhs))?;
        loop {
            ctx.check_interrupt()?;
            let Some(id) = property::get_prototype_of(ctx, current.handle())? else {
                return Ok(false);
            };
            if id == target {
                return Ok(true);
            }
            ctx.set(&current, Value::object(id));
        }
    }
}

fn copy_data<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    source: Handle<'s>,
    exclude: Handle<'s>,
) -> JsResult<()> {
    let raw = ctx.get(source);
    if raw == Value::NULL || raw == Value::UNDEFINED {
        return Ok(());
    }
    let source = convert::to_object(ctx, source)?;
    let keys = property::own_property_keys(ctx, source)?;
    for &key in keys.iter() {
        if ctx.get(exclude).as_object().is_some()
            && property::get_own_property_descriptor(ctx, exclude, key)?.is_some()
        {
            continue;
        }
        if property::get_own_property_descriptor(ctx, source, key)?
            .is_some_and(|d| d.enumerable == Some(true))
        {
            let value = property::get_property(ctx, source, key)?;
            let value = ctx.root(value)?;
            define_default(ctx, target, key, value)?;
        }
    }
    Ok(())
}
