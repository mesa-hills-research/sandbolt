use crate::atom::{Atom, KeyKind, PropKey};
use crate::heap::Heap;
use crate::ledger::{TrackedBox, TrackedVec};
use crate::object::{Accessor, ObjectData, ObjectFlags, ProxyData};
use crate::root::{Args, ExecCtx, Handle};
use crate::shape::{PropFlags, Shape, ShapeProp};
use crate::value::*;

#[derive(Clone, Copy, Debug)]
pub struct PropertyDescriptor<T = Value> {
    pub value: Option<T>,
    pub writable: Option<bool>,
    pub get: Option<T>,
    pub set: Option<T>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl<T> Default for PropertyDescriptor<T> {
    fn default() -> Self {
        Self {
            value: None,
            writable: None,
            get: None,
            set: None,
            enumerable: None,
            configurable: None,
        }
    }
}

impl<T> PropertyDescriptor<T> {
    pub fn data(value: T) -> Self {
        Self {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Self::default()
        }
    }
    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }
    pub fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }
    fn map<U>(self, mut f: impl FnMut(T) -> U) -> PropertyDescriptor<U> {
        PropertyDescriptor {
            value: self.value.map(&mut f),
            writable: self.writable,
            get: self.get.map(&mut f),
            set: self.set.map(f),
            enumerable: self.enumerable,
            configurable: self.configurable,
        }
    }
}

const LENGTH: PropKey = PropKey::atom(Atom::LENGTH);

fn own(heap: &Heap, id: ObjId, key: PropKey) -> Option<(Value, PropFlags)> {
    let object = heap.object(id);
    if let ObjectData::Array(array) = &object.data {
        if key == LENGTH {
            let flags = if array.length_writable {
                PropFlags::WRITABLE
            } else {
                PropFlags::default()
            };
            return Some((Value::number(f64::from(array.length)), flags));
        }
        if object.flags.contains(ObjectFlags::FAST_ARRAY)
            && let Some(i) = heap.array_index(key)
        {
            return array
                .elements
                .get(i as usize)
                .copied()
                .filter(|value| *value != Value::EMPTY)
                .map(|value| (value, PropFlags::DEFAULT_DATA));
        }
    }
    let (index, flags) = heap.shape(object.shape).find(key)?;
    Some((object.props[index], flags))
}

fn describe(heap: &Heap, value: Value, flags: PropFlags) -> PropertyDescriptor {
    let mut desc = PropertyDescriptor {
        enumerable: Some(flags.contains(PropFlags::ENUMERABLE)),
        configurable: Some(flags.contains(PropFlags::CONFIGURABLE)),
        ..PropertyDescriptor::default()
    };
    if let ValueKind::Accessor(id) = value.decode() {
        let accessor = heap.accessors.get(id).unwrap();
        desc.get = Some(accessor.getter.map_or(Value::UNDEFINED, Value::object));
        desc.set = Some(accessor.setter.map_or(Value::UNDEFINED, Value::object));
    } else {
        desc.value = Some(value);
        desc.writable = Some(flags.contains(PropFlags::WRITABLE));
    }
    desc
}

fn string_property(heap: &Heap, id: ObjId, key: PropKey) -> Option<(StrId, Option<u32>)> {
    let ObjectData::StringObject(data) = &heap.object(id).data else {
        return None;
    };
    let ValueKind::String(string) = data.value.decode() else {
        unreachable!()
    };
    if key == LENGTH {
        return Some((string, None));
    }
    let index = heap.array_index(key)?;
    (index < heap.string(string).len()).then_some((string, Some(index)))
}

fn mapped_parameter(heap: &Heap, id: ObjId, key: PropKey) -> Option<VarRefId> {
    let ObjectData::MappedArguments(data) = &heap.object(id).data else {
        return None;
    };
    let index = heap.array_index(key)?;
    data.parameters.get(index as usize).copied().flatten()
}

fn detach_parameter(ctx: &mut ExecCtx<'_, '_>, id: ObjId, key: PropKey) {
    let Some(index) = ctx.heap().array_index(key) else {
        return;
    };
    if let ObjectData::MappedArguments(data) =
        &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
        && let Some(parameter) = data.parameters.get_mut(index as usize)
    {
        *parameter = None;
    }
}

// Outer None denotes an ordinary property. Inner None denotes an invalid numeric index.
fn typed_index(heap: &Heap, id: ObjId, key: PropKey) -> Option<Option<usize>> {
    if !matches!(heap.object(id).data, ObjectData::TypedArray(_)) {
        return None;
    }
    let atom = match key.decode() {
        KeyKind::Index(index) => return Some(Some(index as usize)),
        KeyKind::Symbol(_) => return None,
        KeyKind::String(atom) => atom,
    };
    let mut buffer = [0; 32];
    let text = if let Some(text) = crate::atom::AtomTable::builtin(atom) {
        text
    } else {
        let id = heap.atoms.dynamic_string(atom)?;
        let len = heap.string(id).len() as usize;
        if len > buffer.len() {
            return None;
        }
        for (out, unit) in buffer.iter_mut().zip(heap.string_units(id)) {
            *out = u8::try_from(unit).ok()?;
        }
        std::str::from_utf8(&buffer[..len]).ok()?
    };
    if matches!(text, "-0" | "NaN" | "Infinity" | "-Infinity") {
        return Some(None);
    }
    if !text
        .as_bytes()
        .first()
        .is_some_and(|ch| matches!(ch, b'-' | b'0'..=b'9'))
    {
        return None;
    }
    let (number, end) = qjs_dtoa::atod(text, 10, Default::default());
    if end != text.len() || qjs_dtoa::dtoa(number, 10, 0, Default::default()) != text {
        return None;
    }
    Some(
        (number >= 0.0 && number.fract() == 0.0 && number < usize::MAX as f64)
            .then_some(number as usize),
    )
}

fn typed_has(heap: &Heap, id: ObjId, index: Option<usize>) -> bool {
    index.is_some_and(|index| {
        crate::builtins::typed_atomics::typed_array_length(heap, id).is_some_and(|len| index < len)
    })
}

fn module_namespace(heap: &Heap, id: ObjId) -> Option<&crate::vm::ModuleRecord> {
    let ObjectData::ModuleNamespace(data) = &heap.object(id).data else {
        return None;
    };
    data.module.and_then(|id| heap.modules.get(id))
}

fn namespace_binding(heap: &Heap, id: ObjId, key: PropKey) -> Option<VarRefId> {
    module_namespace(heap, id)?
        .namespace_bindings
        .iter()
        .find_map(|&(name, cell)| (name == key).then_some(cell))
}

fn namespace_value(ctx: &mut ExecCtx<'_, '_>, cell: VarRefId) -> JsResult<Value> {
    let value = crate::vm::exec::read_ref(ctx, cell);
    if value == Value::UNINITIALIZED {
        return Err(ctx.throw_error(
            crate::object::ErrorKind::Reference,
            "module binding is not initialized",
        ));
    }
    Ok(value)
}

fn own_descriptor(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    key: PropKey,
) -> JsResult<Option<PropertyDescriptor>> {
    if let Some(cell) = namespace_binding(ctx.heap(), id, key) {
        let value = namespace_value(ctx, cell)?;
        return Ok(Some(PropertyDescriptor {
            configurable: Some(false),
            ..PropertyDescriptor::data(value)
        }));
    }
    if let Some(index) = typed_index(ctx.heap(), id, key) {
        if !typed_has(ctx.heap(), id, index) {
            return Ok(None);
        }
        let value = crate::builtins::typed_atomics::typed_array_get_index(ctx, id, index.unwrap())?;
        return Ok(Some(PropertyDescriptor::data(value)));
    }
    if let Some((value, flags)) = own(ctx.heap(), id, key) {
        let value = mapped_parameter(ctx.heap(), id, key)
            .map_or(value, |id| crate::vm::exec::read_ref(ctx, id));
        return Ok(Some(describe(ctx.heap(), value, flags)));
    }
    let Some((string, index)) = string_property(ctx.heap(), id, key) else {
        return Ok(None);
    };
    let (value, flags) = if let Some(index) = index {
        let unit = ctx.heap().string_units(string).nth(index as usize).unwrap();
        let value = ctx.new_utf16(&[unit])?;
        (ctx.get(value), PropFlags::ENUMERABLE)
    } else {
        (
            Value::number(f64::from(ctx.heap().string(string).len())),
            PropFlags::default(),
        )
    };
    Ok(Some(describe(ctx.heap(), value, flags)))
}

fn compatible(
    heap: &Heap,
    extensible: bool,
    desc: PropertyDescriptor,
    previous: Option<PropertyDescriptor>,
) -> bool {
    let Some(previous) = previous else {
        return extensible;
    };
    if previous.configurable != Some(false) {
        return true;
    }
    if desc.configurable == Some(true)
        || desc
            .enumerable
            .is_some_and(|new| Some(new) != previous.enumerable)
    {
        return false;
    }
    if desc.is_data() && previous.is_accessor() || desc.is_accessor() && previous.is_data() {
        return false;
    }
    if previous.writable == Some(false)
        && (desc.writable == Some(true)
            || desc.value.is_some_and(|new| {
                !super::compare::same_value_raw(heap, new, previous.value.unwrap())
            }))
    {
        return false;
    }
    if desc.get.is_some_and(|get| Some(get) != previous.get)
        || desc.set.is_some_and(|set| Some(set) != previous.set)
    {
        return false;
    }
    true
}

pub fn new_proxy<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    handler: Handle<'s>,
) -> JsResult<Handle<'s>> {
    let target = ctx.object_id(target)?;
    let handler = ctx.object_id(handler)?;
    // Private elements use the proxy's own storage. Ordinary queries use its traps.
    let mut flags = ObjectFlags::IS_EXOTIC | ObjectFlags::EXTENSIBLE;
    flags.set(
        ObjectFlags::IS_CONSTRUCTOR,
        ctx.heap()
            .object(target)
            .flags
            .contains(ObjectFlags::IS_CONSTRUCTOR),
    );
    let callable = super::call::is_callable(ctx.heap(), target);
    ctx.make_object(None, flags, |ledger| {
        Ok(ObjectData::Proxy(TrackedBox::new(
            ledger,
            ProxyData {
                target: Some(target),
                handler: Some(handler),
                callable,
            },
        )?))
    })
}

pub fn revoke_proxy<'s>(ctx: &mut ExecCtx<'_, 's>, proxy: Handle<'s>) -> JsResult<()> {
    let id = ctx.object_id(proxy)?;
    let ObjectData::Proxy(proxy) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        return Err(ctx.throw_type_error("expected a Proxy"));
    };
    proxy.target = None;
    proxy.handler = None;
    Ok(())
}

fn proxy_operation<T>(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    trap_name: &str,
    run: impl for<'s> FnOnce(
        &mut ExecCtx<'_, 's>,
        Handle<'s>,
        Handle<'s>,
        Option<Handle<'s>>,
    ) -> JsResult<T>,
) -> JsResult<T> {
    let (target, handler) = proxy_parts(ctx, id)?;
    ctx.with_scope(|ctx| {
        if !ctx.runtime.vm.check_budget(0, 0, 1) {
            return Err(ctx.stack_overflow());
        }
        ctx.runtime.vm.reentry_depth += 1;
        let target = ctx.root(Value::object(target))?;
        let handler = ctx.root(Value::object(handler))?;
        let trap = lookup_proxy_trap(ctx, handler, Atom::get(trap_name))?;
        run(ctx, target, handler, trap)
    })
}

fn is_proxy(ctx: &ExecCtx<'_, '_>, id: ObjId) -> bool {
    matches!(ctx.heap().object(id).data, ObjectData::Proxy(_))
}

pub fn get_own_property_descriptor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<Option<PropertyDescriptor>> {
    let id = ctx.object_id(object)?;
    root_key(ctx, key)?;
    if is_proxy(ctx, id) && !is_private(ctx.heap(), key) {
        return proxy_get_own(ctx, id, key);
    }
    own_descriptor(ctx, id, key)
}

pub fn get_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<Value> {
    get_property_with_receiver(ctx, object, key, object)
}

pub fn get_property_with_receiver<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    receiver: Handle<'s>,
) -> JsResult<Value> {
    let object = ctx.get(object);
    let receiver = ctx.get(receiver);
    ctx.with_scope(|ctx| {
        let object = ctx.root(object)?;
        let object = super::convert::to_object(ctx, object)?;
        let id = ctx.object_id(object)?;
        let receiver = ctx.root(receiver)?;
        get_from(ctx, id, key, receiver)
    })
}

fn get_from<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    mut id: ObjId,
    key: PropKey,
    receiver: Handle<'s>,
) -> JsResult<Value> {
    loop {
        if let ObjectData::Proxy(proxy) = &ctx.heap().object(id).data {
            let (target, handler) = (proxy.target, proxy.handler);
            let target = target.ok_or_else(|| ctx.throw_type_error("proxy is revoked"))?;
            if let Some(handler) = handler {
                let target = ctx.root(Value::object(target))?;
                let handler = ctx.root(Value::object(handler))?;
                root_key(ctx, key)?;
                if let Some(trap) = lookup_proxy_trap(ctx, handler, Atom::get("get"))? {
                    let key_value = super::convert::key_value(ctx, key)?;
                    let args = ctx.args(&[target, key_value, receiver])?;
                    let value = super::call::call(ctx, trap, handler, args)?;
                    let value = ctx.root(value)?;
                    if let Some(desc) = get_own_property_descriptor(ctx, target, key)? {
                        check_proxy_get_set(ctx, desc, ctx.get(value), false)?;
                    }
                    return Ok(ctx.get(value));
                }
            }
            id = target;
            continue;
        }
        if let Some(cell) = namespace_binding(ctx.heap(), id, key) {
            return namespace_value(ctx, cell);
        }
        if let Some(index) = typed_index(ctx.heap(), id, key) {
            return if let Some(index) = index {
                crate::builtins::typed_atomics::typed_array_get_index(ctx, id, index)
            } else {
                Ok(Value::UNDEFINED)
            };
        }
        if let Some((string, index)) = string_property(ctx.heap(), id, key) {
            return if let Some(index) = index {
                let unit = ctx.heap().string_units(string).nth(index as usize).unwrap();
                let value = ctx.new_utf16(&[unit])?;
                Ok(ctx.get(value))
            } else {
                Ok(Value::number(f64::from(ctx.heap().string(string).len())))
            };
        }
        if let Some((value, _)) = own(ctx.heap(), id, key) {
            if let ValueKind::Accessor(acc) = value.decode() {
                if let Some(getter) = ctx.heap().accessors.get(acc).unwrap().getter {
                    let callee = ctx.root(Value::object(getter))?;
                    return super::call::call(ctx, callee, receiver, Args::empty());
                }
                return Ok(Value::UNDEFINED);
            }
            return Ok(mapped_parameter(ctx.heap(), id, key)
                .map_or(value, |id| crate::vm::exec::read_ref(ctx, id)));
        }
        let Some(proto) = ctx.heap().shape(ctx.heap().object(id).shape).proto else {
            return Ok(Value::UNDEFINED);
        };
        id = proto;
    }
}

pub fn has_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<bool> {
    let mut id = ctx.object_id(object)?;
    loop {
        if is_proxy(ctx, id) {
            root_key(ctx, key)?;
            return proxy_operation(ctx, id, "has", |ctx, target, handler, trap| {
                let Some(trap) = trap else {
                    return has_property(ctx, target, key);
                };
                let key_value = super::convert::key_value(ctx, key)?;
                let args = ctx.args(&[target, key_value])?;
                let result = super::call::call(ctx, trap, handler, args)?;
                let found = super::convert::to_boolean_value(ctx.heap(), result);
                if !found
                    && let Some(desc) = get_own_property_descriptor(ctx, target, key)?
                    && (desc.configurable == Some(false) || !is_extensible(ctx, target)?)
                {
                    return Err(ctx.throw_type_error("proxy has trap hid a protected property"));
                }
                Ok(found)
            });
        }
        if let Some(index) = typed_index(ctx.heap(), id, key) {
            return Ok(typed_has(ctx.heap(), id, index));
        }
        if own(ctx.heap(), id, key).is_some()
            || namespace_binding(ctx.heap(), id, key).is_some()
            || string_property(ctx.heap(), id, key).is_some()
        {
            return Ok(true);
        }
        let Some(proto) = ctx.heap().shape(ctx.heap().object(id).shape).proto else {
            return Ok(false);
        };
        id = proto;
    }
}

pub fn set_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
) -> JsResult<bool> {
    set_property_with_receiver(ctx, object, key, value, object)
}

pub fn set_property_with_receiver<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    value: Handle<'s>,
    receiver: Handle<'s>,
) -> JsResult<bool> {
    let object = ctx.object_id(object)?;
    let value = ctx.get(value);
    let receiver = ctx.get(receiver);
    ctx.with_scope(|ctx| {
        let receiver = ctx.root(receiver)?;
        let value = ctx.root(value)?;
        set_from(ctx, object, key, receiver, value)
    })
}

fn set_from<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    mut id: ObjId,
    key: PropKey,
    receiver: Handle<'s>,
    value: Handle<'s>,
) -> JsResult<bool> {
    loop {
        if let ObjectData::Proxy(proxy) = &ctx.heap().object(id).data {
            let (target, handler) = (proxy.target, proxy.handler);
            let target = target.ok_or_else(|| ctx.throw_type_error("proxy is revoked"))?;
            let target_root = ctx.root(Value::object(target))?;
            if let Some(handler) = handler {
                let handler = ctx.root(Value::object(handler))?;
                root_key(ctx, key)?;
                if let Some(trap) = lookup_proxy_trap(ctx, handler, Atom::get("set"))? {
                    let key_value = super::convert::key_value(ctx, key)?;
                    let args = ctx.args(&[target_root, key_value, value, receiver])?;
                    let result = super::call::call(ctx, trap, handler, args)?;
                    if !super::convert::to_boolean_value(ctx.heap(), result) {
                        return Ok(false);
                    }
                    if let Some(desc) = get_own_property_descriptor(ctx, target_root, key)? {
                        check_proxy_get_set(ctx, desc, ctx.get(value), true)?;
                    }
                    return Ok(true);
                }
            }
            id = target;
            continue;
        }
        if module_namespace(ctx.heap(), id).is_some() {
            return Ok(false);
        }
        if let Some(index) = typed_index(ctx.heap(), id, key) {
            let same_receiver = ctx.get(receiver) == Value::object(id);
            if same_receiver {
                return crate::builtins::typed_atomics::typed_array_set_index(
                    ctx, id, index, value,
                );
            }
            if !typed_has(ctx.heap(), id, index) {
                return Ok(true);
            }
            break;
        }
        if string_property(ctx.heap(), id, key).is_some() {
            return Ok(false);
        }
        if let Some((old, flags)) = own(ctx.heap(), id, key) {
            if let ValueKind::Accessor(acc) = old.decode() {
                let Some(setter) = ctx.heap().accessors.get(acc).unwrap().setter else {
                    return Ok(false);
                };
                let callee = ctx.root(Value::object(setter))?;
                let args = ctx.args(&[value])?;
                super::call::call(ctx, callee, receiver, args)?;
                return Ok(true);
            }
            if !flags.contains(PropFlags::WRITABLE) {
                return Ok(false);
            }
            break;
        }
        let Some(proto) = ctx.heap().shape(ctx.heap().object(id).shape).proto else {
            break;
        };
        id = proto;
    }
    if ctx.get(receiver).as_object().is_none() {
        return Ok(false);
    }
    if let Some(desc) = get_own_property_descriptor(ctx, receiver, key)? {
        if desc.is_accessor() || desc.writable == Some(false) {
            return Ok(false);
        }
        define_own_property(
            ctx,
            receiver,
            key,
            PropertyDescriptor {
                value: Some(value),
                ..PropertyDescriptor::default()
            },
        )
    } else {
        define_own_property(ctx, receiver, key, PropertyDescriptor::data(value))
    }
}

pub(crate) fn proxy_parts(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<(ObjId, ObjId)> {
    let ObjectData::Proxy(proxy) = &ctx.heap().object(id).data else {
        unreachable!()
    };
    proxy
        .target
        .zip(proxy.handler)
        .ok_or_else(|| ctx.throw_type_error("proxy is revoked"))
}

pub(crate) fn lookup_proxy_trap<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    handler: Handle<'s>,
    trap: PropKey,
) -> JsResult<Option<Handle<'s>>> {
    let handler = ctx.get(handler);
    let method = ctx.with_scope(|ctx| {
        ctx.check_interrupt()?;
        if !ctx.runtime.vm.check_budget(0, 0, 1) {
            return Err(ctx.stack_overflow());
        }
        ctx.runtime.vm.reentry_depth += 1;
        let handler = ctx.root(handler)?;
        get_property(ctx, handler, trap)
    })?;
    if method == Value::UNDEFINED || method == Value::NULL {
        return Ok(None);
    }
    if !method
        .as_object()
        .is_some_and(|id| super::call::is_callable(ctx.heap(), id))
    {
        return Err(ctx.throw_type_error("proxy trap is not callable"));
    }
    ctx.root(method).map(Some)
}

fn check_proxy_get_set(
    ctx: &mut ExecCtx<'_, '_>,
    desc: PropertyDescriptor,
    value: Value,
    set: bool,
) -> JsResult<()> {
    if desc.configurable == Some(false) {
        if desc.writable == Some(false)
            && !super::compare::same_value_raw(ctx.heap(), desc.value.unwrap(), value)
        {
            return Err(ctx.throw_type_error("proxy trap changed a frozen data property"));
        }
        if set && desc.set == Some(Value::UNDEFINED) {
            return Err(ctx.throw_type_error("proxy trap accepted a property without a setter"));
        }
        if !set && desc.get == Some(Value::UNDEFINED) && value != Value::UNDEFINED {
            return Err(
                ctx.throw_type_error("proxy trap returned a value for a property without a getter")
            );
        }
    }
    Ok(())
}

fn root_descriptor(
    ctx: &mut ExecCtx<'_, '_>,
    descriptor: Option<PropertyDescriptor>,
) -> JsResult<()> {
    if let Some(desc) = descriptor {
        for value in [desc.value, desc.get, desc.set].into_iter().flatten() {
            ctx.root(value)?;
        }
    }
    Ok(())
}

fn descriptor_object<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    desc: PropertyDescriptor,
) -> JsResult<Handle<'s>> {
    let object = ctx.new_object()?;
    for (name, value) in [
        ("value", desc.value),
        ("writable", desc.writable.map(Value::bool)),
        ("get", desc.get),
        ("set", desc.set),
        ("enumerable", desc.enumerable.map(Value::bool)),
        ("configurable", desc.configurable.map(Value::bool)),
    ] {
        if let Some(value) = value {
            let value = ctx.root(value)?;
            define_own_property(
                ctx,
                object,
                Atom::get(name),
                PropertyDescriptor::data(value),
            )?;
        }
    }
    Ok(object)
}

fn to_complete_descriptor<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
) -> JsResult<PropertyDescriptor> {
    let mut desc = PropertyDescriptor::default();
    for name in [
        "enumerable",
        "configurable",
        "value",
        "writable",
        "get",
        "set",
    ] {
        let key = Atom::get(name);
        if !has_property(ctx, object, key)? {
            continue;
        }
        let value = get_property(ctx, object, key)?;
        ctx.root(value)?;
        match name {
            "enumerable" => {
                desc.enumerable = Some(super::convert::to_boolean_value(ctx.heap(), value))
            }
            "configurable" => {
                desc.configurable = Some(super::convert::to_boolean_value(ctx.heap(), value))
            }
            "writable" => desc.writable = Some(super::convert::to_boolean_value(ctx.heap(), value)),
            "value" => desc.value = Some(value),
            "get" | "set" => {
                if value != Value::UNDEFINED
                    && !value
                        .as_object()
                        .is_some_and(|id| super::call::is_callable(ctx.heap(), id))
                {
                    return Err(ctx.throw_type_error("accessor is not callable"));
                }
                if name == "get" {
                    desc.get = Some(value);
                } else {
                    desc.set = Some(value);
                }
            }
            _ => unreachable!(),
        }
    }
    if desc.is_accessor() {
        if desc.is_data() {
            return Err(ctx.throw_type_error("invalid property descriptor"));
        }
        desc.get.get_or_insert(Value::UNDEFINED);
        desc.set.get_or_insert(Value::UNDEFINED);
    } else {
        desc.value.get_or_insert(Value::UNDEFINED);
        desc.writable.get_or_insert(false);
    }
    desc.enumerable.get_or_insert(false);
    desc.configurable.get_or_insert(false);
    Ok(desc)
}

fn proxy_get_own(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    key: PropKey,
) -> JsResult<Option<PropertyDescriptor>> {
    proxy_operation(
        ctx,
        id,
        "getOwnPropertyDescriptor",
        |ctx, target, handler, trap| {
            let Some(trap) = trap else {
                return get_own_property_descriptor(ctx, target, key);
            };
            let key_value = super::convert::key_value(ctx, key)?;
            let args = ctx.args(&[target, key_value])?;
            let result = super::call::call(ctx, trap, handler, args)?;
            if result != Value::UNDEFINED && result.as_object().is_none() {
                return Err(ctx.throw_type_error(
                    "proxy getOwnPropertyDescriptor trap must return an object or undefined",
                ));
            }
            let result = ctx.root(result)?;
            let actual = get_own_property_descriptor(ctx, target, key)?;
            root_descriptor(ctx, actual)?;
            if ctx.get(result) == Value::UNDEFINED {
                if let Some(actual) = actual
                    && (actual.configurable == Some(false) || !is_extensible(ctx, target)?)
                {
                    return Err(ctx.throw_type_error(
                        "proxy getOwnPropertyDescriptor trap hid a protected property",
                    ));
                }
                return Ok(None);
            }
            let extensible = is_extensible(ctx, target)?;
            let reported = to_complete_descriptor(ctx, result)?;
            if !compatible(ctx.heap(), extensible, reported, actual)
                || reported.configurable == Some(false)
                    && (actual.is_none_or(|d| d.configurable == Some(true))
                        || reported.writable == Some(false)
                            && actual.is_some_and(|d| d.writable == Some(true)))
            {
                return Err(ctx.throw_type_error(
                    "proxy getOwnPropertyDescriptor trap reported an incompatible descriptor",
                ));
            }
            Ok(Some(reported))
        },
    )
}

fn proxy_define(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    key: PropKey,
    desc: PropertyDescriptor,
) -> JsResult<bool> {
    proxy_operation(ctx, id, "defineProperty", |ctx, target, handler, trap| {
        let Some(trap) = trap else {
            return define_raw(ctx, ctx.get(target).as_object().unwrap(), key, desc);
        };
        let key_value = super::convert::key_value(ctx, key)?;
        let descriptor = descriptor_object(ctx, desc)?;
        let args = ctx.args(&[target, key_value, descriptor])?;
        let result = super::call::call(ctx, trap, handler, args)?;
        if !super::convert::to_boolean_value(ctx.heap(), result) {
            return Ok(false);
        }
        let actual = get_own_property_descriptor(ctx, target, key)?;
        root_descriptor(ctx, actual)?;
        let extensible = is_extensible(ctx, target)?;
        if !compatible(ctx.heap(), extensible, desc, actual)
            || desc.configurable == Some(false)
                && actual.is_none_or(|d| d.configurable == Some(true))
            || desc.writable == Some(false)
                && actual.is_some_and(|d| d.configurable == Some(false) && d.writable == Some(true))
        {
            return Err(ctx.throw_type_error(
                "proxy defineProperty trap accepted an incompatible descriptor",
            ));
        }
        Ok(true)
    })
}

fn pin_keys(ctx: &mut ExecCtx<'_, '_>, keys: &[PropKey]) -> JsResult<()> {
    let count = keys
        .iter()
        .filter(|&&key| ctx.heap().dynamic_key_value(key).is_some())
        .count();
    ctx.reserve_stack(count)?;
    push_keys(ctx, keys);
    Ok(())
}

fn push_keys(ctx: &mut ExecCtx<'_, '_>, keys: &[PropKey]) {
    for &key in keys {
        if let Some(value) = ctx.heap().dynamic_key_value(key) {
            ctx.runtime.vm.stack.push_reserved(value);
        }
    }
}

fn proxy_own_keys(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<TrackedVec<PropKey>> {
    let keys = proxy_operation(ctx, id, "ownKeys", |ctx, target, handler, trap| {
        let keys = if let Some(trap) = trap {
            let args = ctx.args(&[target])?;
            let result = super::call::call(ctx, trap, handler, args)?;
            let result = ctx.root(result)?;
            ctx.object_id(result)?;
            let length = get_property(ctx, result, LENGTH)?;
            let length = ctx.root(length)?;
            let length = super::convert::to_length(ctx, length)?;
            let size = usize::try_from(length).map_err(|_| ctx.throw_oom())?;
            let mut keys = TrackedVec::new();
            ctx.allocate(|rt| keys.try_reserve_exact(&rt.heap.ledger, size))?;
            for i in 0..length {
                ctx.check_interrupt()?;
                let index = if let Some(key) = u32::try_from(i).ok().and_then(PropKey::index) {
                    key
                } else {
                    ctx.intern_str(&i.to_string())?
                };
                let value = get_property(ctx, result, index)?;
                let value = ctx.root(value)?;
                let key = match ctx.get(value).decode() {
                    ValueKind::String(_) => ctx.intern(value)?,
                    ValueKind::Symbol(id) => ctx.heap().symbols.get(id).unwrap().key(),
                    _ => {
                        return Err(ctx.throw_type_error(
                            "proxy ownKeys trap must return only strings and symbols",
                        ));
                    }
                };
                if keys.contains(&key) {
                    return Err(ctx.throw_type_error("proxy ownKeys trap returned duplicate keys"));
                }
                keys.push_reserved(key);
            }
            let extensible = is_extensible(ctx, target)?;
            let actual = own_property_keys(ctx, target)?;
            for &key in actual.iter() {
                let desc = get_own_property_descriptor(ctx, target, key)?;
                if (!extensible || desc.is_some_and(|d| d.configurable == Some(false)))
                    && !keys.contains(&key)
                {
                    return Err(ctx.throw_type_error("proxy ownKeys trap omitted a protected key"));
                }
            }
            if !extensible && keys.len() != actual.len() {
                return Err(ctx.throw_type_error(
                    "proxy ownKeys trap added a key to a non-extensible target",
                ));
            }
            keys
        } else {
            own_property_keys(ctx, target)?
        };
        ctx.reserve_stack(keys.len())?;
        Ok(keys)
    })?;
    // The trap's keys may not exist on the target. Keep their backing values alive.
    push_keys(ctx, &keys);
    Ok(keys)
}

fn root_key(ctx: &mut ExecCtx<'_, '_>, key: PropKey) -> JsResult<()> {
    if let Some(value) = ctx.heap().dynamic_key_value(key) {
        ctx.root(value)?;
    }
    Ok(())
}

pub fn define_own_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
    desc: PropertyDescriptor<Handle<'s>>,
) -> JsResult<bool> {
    let id = ctx.object_id(object)?;
    let desc = desc.map(|handle| ctx.get(handle));
    ctx.with_scope(|ctx| define_raw(ctx, id, key, desc))
}

fn define_raw(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    key: PropKey,
    desc: PropertyDescriptor,
) -> JsResult<bool> {
    root_key(ctx, key)?;
    if desc.is_data() && desc.is_accessor() {
        return Err(ctx.throw_type_error("invalid property descriptor"));
    }
    for value in [desc.get, desc.set].into_iter().flatten() {
        if value != Value::UNDEFINED
            && !value
                .as_object()
                .is_some_and(|id| super::call::is_callable(ctx.heap(), id))
        {
            return Err(ctx.throw_type_error("accessor is not callable"));
        }
    }
    if is_proxy(ctx, id) && !is_private(ctx.heap(), key) {
        return proxy_define(ctx, id, key, desc);
    }
    if let Some(cell) = namespace_binding(ctx.heap(), id, key) {
        let value = namespace_value(ctx, cell)?;
        return Ok(!desc.is_accessor()
            && desc.configurable != Some(true)
            && desc.enumerable != Some(false)
            && desc.writable != Some(false)
            && desc
                .value
                .is_none_or(|other| super::compare::same_value_raw(ctx.heap(), value, other)));
    }
    if let Some(index) = typed_index(ctx.heap(), id, key) {
        if !typed_has(ctx.heap(), id, index)
            || desc.is_accessor()
            || desc.configurable == Some(false)
            || desc.enumerable == Some(false)
            || desc.writable == Some(false)
        {
            return Ok(false);
        }
        if let Some(value) = desc.value {
            let value = ctx.root(value)?;
            return crate::builtins::typed_atomics::typed_array_set_index(ctx, id, index, value);
        }
        return Ok(true);
    }
    if string_property(ctx.heap(), id, key).is_some() {
        let current = own_descriptor(ctx, id, key)?;
        return Ok(compatible(ctx.heap(), false, desc, current));
    }
    let object = ctx.heap().object(id);
    let array_index = matches!(object.data, ObjectData::Array(_))
        .then(|| ctx.heap().array_index(key))
        .flatten();
    if let ObjectData::Array(array) = &object.data {
        if key == LENGTH {
            return set_length(ctx, id, desc);
        }
        if array_index.is_some_and(|index| index >= array.length && !array.length_writable) {
            return Ok(false);
        }
    }
    let old = own(ctx.heap(), id, key);
    if old.is_none()
        && !is_private(ctx.heap(), key)
        && !object.flags.contains(ObjectFlags::EXTENSIBLE)
    {
        return Ok(false);
    }
    let parameter = mapped_parameter(ctx.heap(), id, key);
    let old_desc = old.map(|(val, flags)| {
        describe(
            ctx.heap(),
            parameter.map_or(val, |reference| crate::vm::exec::read_ref(ctx, reference)),
            flags,
        )
    });
    if !compatible(
        ctx.heap(),
        is_private(ctx.heap(), key) || object.flags.contains(ObjectFlags::EXTENSIBLE),
        desc,
        old_desc,
    ) {
        return Ok(false);
    }
    let previous = old_desc.unwrap_or_default();
    let accessor = desc.is_accessor() || (!desc.is_data() && previous.is_accessor());
    let mut flags = PropFlags::default();
    flags.set(
        PropFlags::ENUMERABLE,
        desc.enumerable.or(previous.enumerable).unwrap_or(false),
    );
    flags.set(
        PropFlags::CONFIGURABLE,
        desc.configurable.or(previous.configurable).unwrap_or(false),
    );
    flags.set(PropFlags::ACCESSOR, accessor);
    flags.set(
        PropFlags::WRITABLE,
        !accessor && desc.writable.or(previous.writable).unwrap_or(false),
    );
    let value = if accessor {
        let getter = desc.get.or(previous.get).and_then(Value::as_object);
        let setter = desc.set.or(previous.set).and_then(Value::as_object);
        let existing = old.and_then(|(value, _)| {
            let ValueKind::Accessor(id) = value.decode() else {
                return None;
            };
            let data = ctx.heap().accessors.get(id).unwrap();
            (data.getter == getter && data.setter == setter).then_some(value)
        });
        if let Some(value) = existing {
            value
        } else {
            let id = ctx.allocate(|runtime| {
                runtime
                    .heap
                    .accessors
                    .insert(&runtime.heap.ledger, Accessor { getter, setter })
            })?;
            let value = Value::accessor(id);
            ctx.root(value)?;
            value
        }
    } else {
        desc.value.or(previous.value).unwrap_or(Value::UNDEFINED)
    };
    if let Some(index) = array_index
        && ctx
            .heap()
            .object(id)
            .flags
            .contains(ObjectFlags::FAST_ARRAY)
    {
        let ObjectData::Array(array) = &ctx.heap().object(id).data else {
            unreachable!()
        };
        if flags == PropFlags::DEFAULT_DATA
            && (index < 1024 || index as usize <= array.elements.len().saturating_add(64))
        {
            ctx.allocate(|runtime| {
                let heap = &mut runtime.heap;
                let ObjectData::Array(array) = &mut heap.objects.get_mut(id).unwrap().data else {
                    unreachable!()
                };
                let needed = (index as usize + 1).saturating_sub(array.elements.len());
                array.elements.try_reserve(&heap.ledger, needed)?;
                for _ in 0..needed {
                    array.elements.push_reserved(Value::EMPTY);
                }
                array.elements[index as usize] = value;
                array.length = array.length.max(index + 1);
                Ok(())
            })?;
            invalidate_array_prototypes(ctx, id);
            return Ok(true);
        }
        downgrade(ctx, id)?;
    }
    define_slot(ctx, id, ShapeProp { key, flags }, value)?;
    if let Some(reference) = parameter {
        if let Some(value) = desc.value {
            crate::vm::exec::write_ref(ctx, reference, value);
        }
        if desc.is_accessor() || desc.writable == Some(false) {
            detach_parameter(ctx, id, key);
        }
    }
    if let Some(index) = array_index {
        let ObjectData::Array(array) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
        else {
            unreachable!()
        };
        array.length = array.length.max(index + 1);
    }
    invalidate_array_prototypes(ctx, id);
    Ok(true)
}

fn define_slot(
    ctx: &mut ExecCtx<'_, '_>,
    id: ObjId,
    change: ShapeProp,
    value: Value,
) -> JsResult<()> {
    let old_shape = ctx.heap().object(id).shape;
    let old = ctx.heap().shape(old_shape).find(change.key);
    if let Some((index, flags)) = old
        && flags == change.flags
    {
        ctx.runtime.heap.objects.get_mut(id).unwrap().props[index] = value;
        return Ok(());
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let shape = heap.shape(old_shape);
        let shared = !shape.dictionary && old.is_none() && shape.props.len() < 1024;
        let existing = if shared {
            shape.transitions.iter().find_map(|&(p, target)| {
                (p == change && heap.shapes.get_weak(target).is_some()).then_some(target)
            })
        } else {
            None
        };
        let next = if let Some(existing) = existing {
            existing
        } else {
            let next = match old {
                Some((index, _)) => Shape::build(
                    &heap.ledger,
                    shape.proto,
                    shape.props.len(),
                    shape
                        .props
                        .iter()
                        .enumerate()
                        .map(|(i, prop)| if i == index { change } else { *prop }),
                    shape.churn.saturating_add(1),
                    shape.dictionary,
                )?,
                None => Shape::build(
                    &heap.ledger,
                    shape.proto,
                    shape.props.len() + 1,
                    shape.props.iter().copied().chain([change]),
                    shape.churn,
                    shape.dictionary,
                )?,
            };
            let next = heap.shapes.insert(&heap.ledger, next)?;
            if shared {
                heap.shapes
                    .get_mut(old_shape)
                    .unwrap()
                    .transitions
                    .push(&heap.ledger, (change, next))?;
            }
            next
        };
        let object = heap.objects.get_mut(id).unwrap();
        match old {
            Some((index, _)) => object.props[index] = value,
            None => object.props.push(&heap.ledger, value)?,
        }
        object.shape = next;
        Ok(())
    })
}

fn downgrade(ctx: &mut ExecCtx<'_, '_>, id: ObjId) -> JsResult<()> {
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let object = heap.object(id);
        let ObjectData::Array(array) = &object.data else {
            unreachable!()
        };
        let shape = heap.shape(object.shape);
        let indices = || {
            array
                .elements
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, val)| *val != Value::EMPTY)
        };
        let count = indices().count() + shape.props.len();
        let new_shape = Shape::build(
            &heap.ledger,
            shape.proto,
            count,
            shape
                .props
                .iter()
                .copied()
                .chain(indices().map(|(i, _)| ShapeProp {
                    key: PropKey::index(i as u32).unwrap(),
                    flags: PropFlags::DEFAULT_DATA,
                })),
            shape.churn,
            true,
        )?;
        let mut props = TrackedVec::new();
        props.try_reserve_exact(&heap.ledger, count)?;
        for value in object
            .props
            .iter()
            .copied()
            .chain(indices().map(|(_, val)| val))
        {
            props.push_reserved(value);
        }
        let new_shape = heap.shapes.insert(&heap.ledger, new_shape)?;
        let object = heap.objects.get_mut(id).unwrap();
        object.shape = new_shape;
        object.props = props;
        object.flags.set(ObjectFlags::FAST_ARRAY, false);
        let ObjectData::Array(array) = &mut object.data else {
            unreachable!()
        };
        array.elements = TrackedVec::new();
        Ok(())
    })
}

fn set_length(ctx: &mut ExecCtx<'_, '_>, id: ObjId, desc: PropertyDescriptor) -> JsResult<bool> {
    let length = if let Some(value) = desc.value {
        ctx.root(Value::object(id))?;
        let value = ctx.root(value)?;
        let length = super::convert::to_uint32(ctx, value)?;
        let n = super::convert::to_number(ctx, value)?;
        if n != f64::from(length) {
            return Err(ctx.throw_range_error("invalid array length"));
        }
        Some(length)
    } else {
        None
    };
    if desc.configurable == Some(true) || desc.enumerable == Some(true) || desc.is_accessor() {
        return Ok(false);
    }
    let object = ctx.heap().object(id);
    let ObjectData::Array(array) = &object.data else {
        unreachable!()
    };
    if !array.length_writable
        && (desc.writable == Some(true) || length.is_some_and(|length| length != array.length))
    {
        return Ok(false);
    }
    let mut target = length.unwrap_or(array.length);
    let mut ok = true;
    if target < array.length && !object.flags.contains(ObjectFlags::FAST_ARRAY) {
        let old_shape = object.shape;
        let shape = ctx.heap().shape(old_shape);
        for prop in shape.props.iter() {
            if !prop.flags.contains(PropFlags::CONFIGURABLE)
                && let Some(i) = ctx.heap().array_index(prop.key)
                && i >= target
            {
                target = i + 1;
                ok = false;
            }
        }
        ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let shape = heap.shape(old_shape);
            let keep = |prop: &&ShapeProp| {
                heap.array_index(prop.key)
                    .is_none_or(|index| index < target)
            };
            let count = shape.props.iter().filter(keep).count();
            if count == shape.props.len() {
                return Ok(());
            }
            let next = Shape::build(
                &heap.ledger,
                shape.proto,
                count,
                shape.props.iter().filter(keep).copied(),
                shape.churn.saturating_add(1),
                shape.dictionary,
            )?;
            let mut props = TrackedVec::new();
            props.try_reserve_exact(&heap.ledger, count)?;
            for (i, prop) in shape.props.iter().enumerate() {
                if keep(&prop) {
                    props.push_reserved(heap.object(id).props[i]);
                }
            }
            let next = heap.shapes.insert(&heap.ledger, next)?;
            let object = heap.objects.get_mut(id).unwrap();
            object.shape = next;
            object.props = props;
            Ok(())
        })?;
    }
    let ObjectData::Array(array) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        unreachable!()
    };
    array.elements.truncate(target as usize);
    array.length = target;
    if desc.writable == Some(false) {
        array.length_writable = false;
    }
    invalidate_array_prototypes(ctx, id);
    Ok(ok)
}

pub fn delete_property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    key: PropKey,
) -> JsResult<bool> {
    let id = ctx.object_id(object)?;
    if is_proxy(ctx, id) && !is_private(ctx.heap(), key) {
        root_key(ctx, key)?;
        return proxy_operation(ctx, id, "deleteProperty", |ctx, target, handler, trap| {
            let Some(trap) = trap else {
                return delete_property(ctx, target, key);
            };
            let key_value = super::convert::key_value(ctx, key)?;
            let args = ctx.args(&[target, key_value])?;
            let result = super::call::call(ctx, trap, handler, args)?;
            if !super::convert::to_boolean_value(ctx.heap(), result) {
                return Ok(false);
            }
            if let Some(desc) = get_own_property_descriptor(ctx, target, key)?
                && (desc.configurable == Some(false) || !is_extensible(ctx, target)?)
            {
                return Err(
                    ctx.throw_type_error("proxy deleteProperty trap hid a protected property")
                );
            }
            Ok(true)
        });
    }
    if namespace_binding(ctx.heap(), id, key).is_some() {
        return Ok(false);
    }
    if let Some(index) = typed_index(ctx.heap(), id, key) {
        return Ok(!typed_has(ctx.heap(), id, index));
    }
    if string_property(ctx.heap(), id, key).is_some() {
        return Ok(false);
    }
    let Some((_, flags)) = own(ctx.heap(), id, key) else {
        return Ok(true);
    };
    if !flags.contains(PropFlags::CONFIGURABLE) {
        return Ok(false);
    }
    let object = ctx.heap().object(id);
    if object.flags.contains(ObjectFlags::FAST_ARRAY)
        && let Some(index) = ctx.heap().array_index(key)
    {
        let ObjectData::Array(array) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data
        else {
            unreachable!()
        };
        array.elements[index as usize] = Value::EMPTY;
    } else {
        let old_shape = object.shape;
        let (index, _) = ctx.heap().shape(old_shape).find(key).unwrap();
        ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let shape = heap.shape(old_shape);
            let next = Shape::build(
                &heap.ledger,
                shape.proto,
                shape.props.len() - 1,
                shape
                    .props
                    .iter()
                    .enumerate()
                    .filter_map(|(i, prop)| (i != index).then_some(*prop)),
                shape.churn.saturating_add(1),
                shape.dictionary,
            )?;
            let next = heap.shapes.insert(&heap.ledger, next)?;
            let object = heap.objects.get_mut(id).unwrap();
            object.props.remove(index);
            object.shape = next;
            Ok(())
        })?;
    }
    detach_parameter(ctx, id, key);
    invalidate_array_prototypes(ctx, id);
    Ok(true)
}

pub fn own_property_keys<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
) -> JsResult<TrackedVec<PropKey>> {
    let id = ctx.object_id(object)?;
    if is_proxy(ctx, id) {
        return proxy_own_keys(ctx, id);
    }
    let keys = ctx.allocate(|runtime| {
        let heap = &runtime.heap;
        let object = heap.object(id);
        let shape = heap.shape(object.shape);
        let mut keys = TrackedVec::new();
        if let Some(module) = module_namespace(heap, id) {
            for &(key, _) in module.namespace_bindings.iter() {
                keys.push(&heap.ledger, key)?;
            }
            for prop in shape.props.iter() {
                keys.push(&heap.ledger, prop.key)?;
            }
            return Ok(keys);
        }
        let entries = || {
            shape
                .props
                .iter()
                .map(|p| p.key)
                .filter(|&key| !is_private(heap, key))
        };
        let string = if let ObjectData::StringObject(data) = &object.data {
            let ValueKind::String(string) = data.value.decode() else {
                unreachable!()
            };
            for i in 0..heap.string(string).len() {
                keys.push(
                    &heap.ledger,
                    PropKey::index(i).ok_or(crate::ledger::AllocError::Limit)?,
                )?;
            }
            true
        } else {
            false
        };
        if let ObjectData::Array(array) = &object.data {
            for (i, val) in array.elements.iter().enumerate() {
                if *val != Value::EMPTY {
                    keys.push(&heap.ledger, PropKey::index(i as u32).unwrap())?;
                }
            }
        }
        if let Some(length) = crate::builtins::typed_atomics::typed_array_length(heap, id) {
            for i in 0..length {
                keys.push(
                    &heap.ledger,
                    u32::try_from(i)
                        .ok()
                        .and_then(PropKey::index)
                        .ok_or(crate::ledger::AllocError::Limit)?,
                )?;
            }
        }
        for key in entries().filter(|&k| heap.array_index(k).is_some()) {
            keys.push(&heap.ledger, key)?;
        }
        keys.sort_unstable_by_key(|key| heap.array_index(*key));
        if string || matches!(object.data, ObjectData::Array(_)) {
            keys.push(&heap.ledger, LENGTH)?;
        }
        for key in entries()
            .filter(|&k| matches!(k.decode(), KeyKind::String(_)) && heap.array_index(k).is_none())
        {
            keys.push(&heap.ledger, key)?;
        }
        for key in entries().filter(|k| matches!(k.decode(), KeyKind::Symbol(_))) {
            keys.push(&heap.ledger, key)?;
        }
        Ok(keys)
    })?;
    pin_keys(ctx, &keys)?;
    Ok(keys)
}

pub fn get_prototype_of<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
) -> JsResult<Option<ObjId>> {
    let id = ctx.object_id(object)?;
    if is_proxy(ctx, id) {
        return proxy_operation(ctx, id, "getPrototypeOf", |ctx, target, handler, trap| {
            let Some(trap) = trap else {
                return get_prototype_of(ctx, target);
            };
            let args = ctx.args(&[target])?;
            let result = super::call::call(ctx, trap, handler, args)?;
            let result = ctx.root(result)?;
            let proto = match ctx.get(result).decode() {
                ValueKind::Null => None,
                ValueKind::Object(id) => Some(id),
                _ => {
                    return Err(ctx.throw_type_error(
                        "proxy getPrototypeOf trap must return an object or null",
                    ));
                }
            };
            if !is_extensible(ctx, target)? && get_prototype_of(ctx, target)? != proto {
                return Err(
                    ctx.throw_type_error("proxy getPrototypeOf trap changed a locked prototype")
                );
            }
            Ok(proto)
        });
    }
    Ok(ctx.heap().shape(ctx.heap().object(id).shape).proto)
}

pub fn set_prototype_of<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    proto: Option<Handle<'s>>,
) -> JsResult<bool> {
    let id = ctx.object_id(object)?;
    let proto = proto.map(|proto| ctx.object_id(proto)).transpose()?;
    if is_proxy(ctx, id) {
        return proxy_operation(ctx, id, "setPrototypeOf", |ctx, target, handler, trap| {
            let h = proto
                .map(|proto| ctx.root(Value::object(proto)))
                .transpose()?;
            let Some(trap) = trap else {
                return set_prototype_of(ctx, target, h);
            };
            let arg = match h {
                Some(h) => h,
                None => ctx.root(Value::NULL)?,
            };
            let args = ctx.args(&[target, arg])?;
            let result = super::call::call(ctx, trap, handler, args)?;
            if !super::convert::to_boolean_value(ctx.heap(), result) {
                return Ok(false);
            }
            if !is_extensible(ctx, target)? && get_prototype_of(ctx, target)? != proto {
                return Err(
                    ctx.throw_type_error("proxy setPrototypeOf trap changed a locked prototype")
                );
            }
            Ok(true)
        });
    }
    let object = ctx.heap().object(id);
    let shape_id = object.shape;
    if ctx.heap().shape(shape_id).proto == proto {
        return Ok(true);
    }
    if !object.flags.contains(ObjectFlags::EXTENSIBLE)
        || object.flags.contains(ObjectFlags::IMMUTABLE_PROTO)
    {
        return Ok(false);
    }
    let mut parent = proto;
    while let Some(p) = parent {
        if p == id {
            return Ok(false);
        }
        let object = ctx.heap().object(p);
        if matches!(object.data, ObjectData::Proxy(_)) {
            break;
        }
        parent = ctx.heap().shape(object.shape).proto;
    }
    invalidate_array_prototypes(ctx, id);
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let shape = heap.shape(shape_id);
        let next = if shape.props.is_empty() {
            heap.empty_shape(proto)?
        } else {
            let next = Shape::build(
                &heap.ledger,
                proto,
                shape.props.len(),
                shape.props.iter().copied(),
                shape.churn,
                shape.dictionary,
            )?;
            heap.shapes.insert(&heap.ledger, next)?
        };
        heap.objects.get_mut(id).unwrap().shape = next;
        if let Some(proto) = proto {
            heap.objects.get_mut(proto).unwrap().flags |= ObjectFlags::IS_PROTOTYPE;
        }
        Ok(())
    })?;
    Ok(true)
}

pub fn is_extensible<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<bool> {
    let id = ctx.object_id(object)?;
    if is_proxy(ctx, id) {
        return proxy_operation(ctx, id, "isExtensible", |ctx, target, handler, trap| {
            let Some(trap) = trap else {
                return is_extensible(ctx, target);
            };
            let args = ctx.args(&[target])?;
            let result = super::call::call(ctx, trap, handler, args)?;
            let reported = super::convert::to_boolean_value(ctx.heap(), result);
            if is_extensible(ctx, target)? != reported {
                return Err(
                    ctx.throw_type_error("proxy isExtensible trap reported the wrong value")
                );
            }
            Ok(reported)
        });
    }
    Ok(ctx
        .heap()
        .object(id)
        .flags
        .contains(ObjectFlags::EXTENSIBLE))
}

pub fn prevent_extensions<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>) -> JsResult<bool> {
    let id = ctx.object_id(object)?;
    if is_proxy(ctx, id) {
        return proxy_operation(
            ctx,
            id,
            "preventExtensions",
            |ctx, target, handler, trap| {
                let Some(trap) = trap else {
                    return prevent_extensions(ctx, target);
                };
                let args = ctx.args(&[target])?;
                let result = super::call::call(ctx, trap, handler, args)?;
                let reported = super::convert::to_boolean_value(ctx.heap(), result);
                if reported && is_extensible(ctx, target)? {
                    return Err(ctx.throw_type_error(
                        "proxy preventExtensions trap left the target extensible",
                    ));
                }
                Ok(reported)
            },
        );
    }
    if !crate::builtins::typed_atomics::typed_array_can_prevent_extensions(ctx.heap(), id) {
        return Ok(false);
    }
    ctx.runtime
        .heap
        .objects
        .get_mut(id)
        .unwrap()
        .flags
        .set(ObjectFlags::EXTENSIBLE, false);
    Ok(true)
}

fn invalidate_array_prototypes(ctx: &mut ExecCtx<'_, '_>, object: ObjId) {
    if !ctx
        .heap()
        .object(object)
        .flags
        .contains(ObjectFlags::IS_PROTOTYPE)
    {
        return;
    }
    let runtime = &mut ctx.runtime;
    let heap = &mut runtime.heap;
    for &id in runtime.realms.iter() {
        let realm = heap.realm(id);
        if !realm.std_array_prototype_intact {
            continue;
        }
        let mut ancestor = Some(realm.array_proto);
        while let Some(current) = ancestor {
            if current == object {
                heap.realms.get_mut(id).unwrap().std_array_prototype_intact = false;
                break;
            }
            ancestor = heap.shape(heap.object(current).shape).proto;
        }
    }
}

fn is_private(heap: &Heap, key: PropKey) -> bool {
    match key.decode() {
        KeyKind::Symbol(token) => heap
            .symbol_registry
            .from_token(token)
            .and_then(|id| heap.symbols.get(id))
            .is_some_and(|symbol| symbol.is_private),
        KeyKind::String(atom) => atom.0 == qjs_bytecode::BuiltinAtom::PrivateBrand as u32,
        KeyKind::Index(_) => false,
    }
}

pub(crate) fn private_own(ctx: &ExecCtx<'_, '_>, object: ObjId, key: PropKey) -> Option<Value> {
    own(ctx.heap(), object, key).map(|(value, _)| value)
}

pub(crate) fn private_write(ctx: &mut ExecCtx<'_, '_>, object: ObjId, key: PropKey, value: Value) {
    let id = ctx.heap().object(object).shape;
    let (index, _) = ctx.heap().shape(id).find(key).unwrap();
    ctx.runtime.heap.objects.get_mut(object).unwrap().props[index] = value;
}

pub(crate) fn private_define(
    ctx: &mut ExecCtx<'_, '_>,
    object: ObjId,
    key: PropKey,
    value: Value,
) -> JsResult<()> {
    let object = ctx.root(Value::object(object))?;
    let value = ctx.root(value)?;
    if define_own_property(ctx, object, key, PropertyDescriptor::data(value))? {
        Ok(())
    } else {
        Err(ctx.throw_type_error("cannot define private field"))
    }
}

impl Atom {
    pub(crate) fn get(name: &str) -> PropKey {
        crate::atom::AtomTable::builtin_key(name).expect("missing built-in atom")
    }
}
