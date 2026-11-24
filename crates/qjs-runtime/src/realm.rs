use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::EngineLimits;
use crate::arena::ArenaItem;
use crate::atom::{Atom, PropKey};
use crate::gc::{Trace, Tracer};
use crate::heap::Heap;
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedBox, TrackedVec};
use crate::object::{ArrayData, ErrorData, ErrorKind, NativeFunctionData, ObjectData, ObjectFlags, PropSlots};
use crate::root::{Args, ExecCtx, Handle, RootScope};
use crate::shape::{PropFlags, Shape, ShapeProp};
use crate::string::JsString;
use crate::value::*;
use crate::vm::{Job, VmState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeFnId(pub u16);
impl NativeFnId {
    pub const UNIMPLEMENTED: Self = Self(u16::MAX);
    pub(crate) const OBJECT_VALUE_OF: Self = Self(0xfff0);
    pub(crate) const OBJECT_TO_STRING: Self = Self(0xfff1);
    pub(crate) const WRAPPER_VALUE_OF: Self = Self(0xfff2);
    pub(crate) const WRAPPER_TO_STRING: Self = Self(0xfff3);
    pub(crate) const EMPTY: Self = Self(0xfff4);
}

pub type NativeFn = for<'rt, 's> fn(&mut ExecCtx<'rt, 's>, Handle<'s>, Args<'s>) -> JsResult<Value>;

#[derive(Clone, Copy, Debug)]
pub(crate) struct EmergencyError {
    pub object: ObjId,
    shape: ShapeId,
}

impl EmergencyError {
    fn new(heap: &mut Heap, proto: ObjId, kind: ErrorKind, text: &str) -> AllocResult<Self> {
        let message = JsString::from_str(&heap.ledger, text)?;
        let message = Value::string(heap.strings.insert(&heap.ledger, message)?);
        let data = TrackedBox::new(
            &heap.ledger,
            ErrorData {
                kind,
                message,
                stack: Value::UNDEFINED,
                frames: TrackedVec::new(),
            },
        )?;
        let shape = heap.shapes.insert(
            &heap.ledger,
            Shape::build(
                &heap.ledger,
                Some(proto),
                1,
                [ShapeProp {
                    key: PropKey::atom(Atom::MESSAGE),
                    flags: PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
                }]
                .into_iter(),
                0,
                false,
            )?,
        )?;
        let props = PropSlots::one(message);
        let object = heap.objects.insert(
            &heap.ledger,
            crate::object::JsObject {
                shape,
                props,
                flags: ObjectFlags::EXTENSIBLE,
                data: ObjectData::Error(data),
            },
        )?;
        Ok(Self { object, shape })
    }

    pub(crate) fn prepare(self, heap: &mut Heap) -> Value {
        let object = heap.objects.get_mut(self.object).unwrap();
        let ObjectData::Error(error) = &mut object.data else {
            unreachable!()
        };
        let message = error.message;
        error.stack = Value::UNDEFINED;
        error.frames = TrackedVec::new();
        // Each realm retains one emergency object with its original shape and message.
        object.props.clear();
        object.props.push_reserved(message);
        object.shape = self.shape;
        object.flags = ObjectFlags::EXTENSIBLE;
        Value::object(self.object)
    }
}

impl Trace for EmergencyError {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.object.trace(tracer);
        self.shape.trace(tracer);
    }
}

macro_rules! intrinsics {
    ($object:ident, $internal_error:ident, $range_error:ident, $array:ident, $iterator:ident, $async_iterator:ident; $(($proto:ident, $ctor:ident, $parent:ident)),+ $(,)?) => {
        #[derive(Debug)]
        pub struct Realm {
            pub global_object: ObjId,
            pub object_proto: ObjId,
            pub object_ctor: ObjId,
            pub function_proto: ObjId,
            pub function_ctor: ObjId,
            pub throw_type_error: ObjId,
            $( pub $proto: ObjId, pub $ctor: ObjId, )+
            pub array_iterator_proto: ObjId,
            pub string_iterator_proto: ObjId,
            pub regexp_string_iterator_proto: ObjId,
            pub map_iterator_proto: ObjId,
            pub set_iterator_proto: ObjId,
            pub iterator_helper_proto: ObjId,
            pub iterator_wrap_proto: ObjId,
            pub async_from_sync_iterator_proto: ObjId,
            pub for_in_iterator_proto: ObjId,
            pub typed_array_concrete: [Option<(ObjId, ObjId)>; 12],
            pub std_array_prototype_intact: bool,
            pub(crate) callsite_proto: ObjId,
            pub(crate) error_stack_limit: Value,
            pub(crate) error_prepare_stack: Value,
            pub time_origin_instant: Instant,
            pub time_origin_epoch_ms: f64,
            pub(crate) oom_error: EmergencyError,
            pub(crate) stack_error: EmergencyError,
            pub(crate) native_fn_shape: ShapeId,
            pub(crate) ctor_proto_shape: Option<ShapeId>,
            pub(crate) ctor_shape: ShapeId,
            pub(crate) regexp_shape: Option<ShapeId>,
            pub(crate) regexp_result_shape: Option<ShapeId>,
            pub(crate) null_proto_shape: Option<ShapeId>,
            uri_error: Option<(ShapeId, StrId)>,
        }

        impl Trace for Realm {
            fn trace(&self, tracer: &Tracer<'_>) {
                self.global_object.trace(tracer);
                self.object_proto.trace(tracer);
                self.object_ctor.trace(tracer);
                self.function_proto.trace(tracer);
                self.function_ctor.trace(tracer);
                self.throw_type_error.trace(tracer);
                $( self.$proto.trace(tracer); self.$ctor.trace(tracer); )+
                self.array_iterator_proto.trace(tracer);
                self.string_iterator_proto.trace(tracer);
                self.regexp_string_iterator_proto.trace(tracer);
                self.map_iterator_proto.trace(tracer);
                self.set_iterator_proto.trace(tracer);
                self.iterator_helper_proto.trace(tracer);
                self.iterator_wrap_proto.trace(tracer);
                self.async_from_sync_iterator_proto.trace(tracer);
                self.for_in_iterator_proto.trace(tracer);
                for pair in self.typed_array_concrete.into_iter().flatten() {
                    pair.0.trace(tracer);
                    pair.1.trace(tracer);
                }
                self.callsite_proto.trace(tracer);
                self.error_stack_limit.trace(tracer);
                self.error_prepare_stack.trace(tracer);
                self.oom_error.trace(tracer);
                self.stack_error.trace(tracer);
                self.native_fn_shape.trace(tracer);
                self.ctor_proto_shape.trace(tracer);
                self.ctor_shape.trace(tracer);
                self.regexp_shape.trace(tracer);
                self.regexp_result_shape.trace(tracer);
                self.null_proto_shape.trace(tracer);
                if let Some((shape, message)) = self.uri_error {
                    shape.trace(tracer);
                    message.trace(tracer);
                }
            }
        }

        fn bootstrap(heap: &mut Heap, keep_building: bool) -> AllocResult<RealmId> {
            heap.realms.try_reserve_slot(&heap.ledger)?;
            let ordinary = ObjectFlags::EXTENSIBLE;
            let proto = ordinary | ObjectFlags::IS_PROTOTYPE;
            let $object = heap.insert_object(None, proto | ObjectFlags::IMMUTABLE_PROTO, ObjectData::Ordinary)?;
            let function_proto = heap.insert_object(Some($object), proto, ObjectData::Ordinary)?;
            let native_fn_shape = heap.shapes.insert(
                &heap.ledger,
                Shape::build(
                    &heap.ledger,
                    Some(function_proto),
                    2,
                    [
                        ShapeProp { key: PropKey::atom(Atom::LENGTH), flags: PropFlags::CONFIGURABLE },
                        ShapeProp { key: PropKey::atom(Atom::NAME), flags: PropFlags::CONFIGURABLE },
                    ].into_iter(),
                    0,
                    false,
                )?,
            )?;
            let ctor_proto_shape = if keep_building {
                None
            } else {
                Some(heap.shapes.insert(
                    &heap.ledger,
                    Shape::build(
                        &heap.ledger,
                        Some(function_proto),
                        1,
                        [ShapeProp { key: PropKey::atom(Atom::PROTOTYPE), flags: PropFlags::default() }].into_iter(),
                        0,
                        false,
                    )?,
                )?)
            };
            let ctor_shape = heap.shapes.insert(
                &heap.ledger,
                Shape::build(
                    &heap.ledger,
                    Some(function_proto),
                    3,
                    [
                        ShapeProp { key: PropKey::atom(Atom::PROTOTYPE), flags: PropFlags::default() },
                        ShapeProp { key: PropKey::atom(Atom::LENGTH), flags: PropFlags::CONFIGURABLE },
                        ShapeProp { key: PropKey::atom(Atom::NAME), flags: PropFlags::CONFIGURABLE },
                    ].into_iter(),
                    0,
                    false,
                )?,
            )?;
            let throw_type_error = heap.insert_object(Some(function_proto), ObjectFlags::default(), ObjectData::Ordinary)?;
            let global_object = heap.insert_object(Some($object), ordinary, ObjectData::Ordinary)?;
            $( let $proto = heap.insert_object(Some($parent), proto, ObjectData::Ordinary)?; )+
            let array_iterator_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let string_iterator_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let regexp_string_iterator_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let map_iterator_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let set_iterator_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let iterator_helper_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let iterator_wrap_proto = heap.insert_object(Some($iterator), proto, ObjectData::Ordinary)?;
            let async_from_sync_iterator_proto = heap.insert_object(Some($async_iterator), proto, ObjectData::Ordinary)?;
            let for_in_iterator_proto = heap.insert_object(Some($object), proto, ObjectData::Ordinary)?;
            let callsite_proto = heap.insert_object(Some($object), proto, ObjectData::Ordinary)?;
            let oom_error = EmergencyError::new(heap, $internal_error, ErrorKind::Internal, "out of memory")?;
            let stack_error = EmergencyError::new(heap, $range_error, ErrorKind::Range, "Maximum call stack size exceeded")?;
            let time_origin_instant = Instant::now();
            let time_origin_epoch_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0);

            // Constructor identity is installed after the realm has an ID.
            let realm = heap.realms.insert_reserved(Realm {
                global_object, object_proto: $object, object_ctor: function_proto, function_proto, function_ctor: function_proto,
                throw_type_error,
                $( $proto, $ctor: function_proto, )+
                array_iterator_proto,
                string_iterator_proto,
                regexp_string_iterator_proto,
                map_iterator_proto,
                set_iterator_proto,
                iterator_helper_proto,
                iterator_wrap_proto,
                async_from_sync_iterator_proto,
                for_in_iterator_proto,
                typed_array_concrete: [None; 12],
                std_array_prototype_intact: true,
                callsite_proto,
                error_stack_limit: Value::int32(10),
                error_prepare_stack: Value::UNDEFINED,
                time_origin_instant,
                time_origin_epoch_ms,
                oom_error, stack_error,
                native_fn_shape, ctor_proto_shape, ctor_shape,
                regexp_shape: None, regexp_result_shape: None, null_proto_shape: None,
                uri_error: None,
            });
            let prev_building = heap.building_shapes;
            heap.building_shapes = true;
            let constructors = (|| {
                let data = native_payload(realm, NativeFnId::EMPTY);
                heap.objects.get_mut(function_proto).unwrap().data = data;
                let data = native_payload(realm, crate::builtins::core::THROW_TYPE_ERROR);
                heap.objects.get_mut(throw_type_error).unwrap().data = data;
                let empty = JsString::from_str(&heap.ledger, "")?;
                let empty = Value::string(heap.strings.insert(&heap.ledger, empty)?);
                install_property(heap, throw_type_error, PropKey::atom(Atom::LENGTH), PropFlags::default(), Value::int32(0))?;
                install_property(heap, throw_type_error, PropKey::atom(Atom::NAME), PropFlags::default(), empty)?;
                let data = TrackedBox::new(&heap.ledger, ArrayData { elements: TrackedVec::new(), length: 0, length_writable: true, realm })?;
                let array = heap.objects.get_mut($array).unwrap();
                array.flags |= ObjectFlags::IS_EXOTIC | ObjectFlags::FAST_ARRAY;
                array.data = ObjectData::Array(data);
                let ctor_init_shape = ctor_proto_shape.unwrap_or(ctor_shape);
                let init_empty = ctor_proto_shape.is_none().then_some(empty);
                let object_ctor = bootstrap_ctor(heap, realm, ctor_init_shape, init_empty, $object)?;
                let function_ctor = bootstrap_ctor(heap, realm, ctor_init_shape, init_empty, function_proto)?;
                $( let $ctor = bootstrap_ctor(heap, realm, ctor_init_shape, init_empty, $proto)?; )+
                let record = heap.realms.get_mut(realm).unwrap();
                record.object_ctor = object_ctor;
                record.function_ctor = function_ctor;
                $( record.$ctor = $ctor; )+
                bootstrap_methods(heap, realm)?;
                Ok(())
            })();
            if !keep_building && !prev_building {
                heap.finish_bootstrap_shapes();
            }
            if let Err(error) = constructors {
                if keep_building && !prev_building {
                    heap.finish_bootstrap_shapes();
                }
                heap.realms.remove(realm);
                return Err(error);
            }
            if !keep_building && !prev_building && let Err(error) = heap.ensure_bootstrap_lookups() {
                heap.realms.remove(realm);
                return Err(error);
            }
            Ok(realm)
        }
    };
}

intrinsics!(object_proto, internal_error_proto, range_error_proto, array_proto, iterator_proto, async_iterator_proto;
    (array_proto, array_ctor, object_proto), (error_proto, error_ctor, object_proto),
    (type_error_proto, type_error_ctor, error_proto), (range_error_proto, range_error_ctor, error_proto),
    (reference_error_proto, reference_error_ctor, error_proto), (syntax_error_proto, syntax_error_ctor, error_proto),
    (uri_error_proto, uri_error_ctor, error_proto), (eval_error_proto, eval_error_ctor, error_proto),
    (internal_error_proto, internal_error_ctor, error_proto), (aggregate_error_proto, aggregate_error_ctor, error_proto),
    (suppressed_error_proto, suppressed_error_ctor, error_proto), (promise_proto, promise_ctor, object_proto),
    (symbol_proto, symbol_ctor, object_proto), (string_proto, string_ctor, object_proto),
    (number_proto, number_ctor, object_proto), (boolean_proto, boolean_ctor, object_proto),
    (bigint_proto, bigint_ctor, object_proto), (date_proto, date_ctor, object_proto),
    (regexp_proto, regexp_ctor, object_proto), (map_proto, map_ctor, object_proto), (set_proto, set_ctor, object_proto),
    (weakmap_proto, weakmap_ctor, object_proto), (weakset_proto, weakset_ctor, object_proto),
    (weakref_proto, weakref_ctor, object_proto), (finalization_registry_proto, finalization_registry_ctor, object_proto),
    (array_buffer_proto, array_buffer_ctor, object_proto), (shared_array_buffer_proto, shared_array_buffer_ctor, object_proto),
    (typed_array_proto, typed_array_ctor, object_proto), (data_view_proto, data_view_ctor, object_proto),
    (iterator_proto, iterator_ctor, object_proto), (async_iterator_proto, async_iterator_ctor, object_proto),
    (generator_proto, generator_ctor, iterator_proto), (async_generator_proto, async_generator_ctor, async_iterator_proto),
    (disposable_stack_proto, disposable_stack_ctor, object_proto), (async_disposable_stack_proto, async_disposable_stack_ctor, object_proto),
    (dom_exception_proto, dom_exception_ctor, error_proto), (performance_proto, performance_ctor, object_proto),
);

impl ArenaItem for Realm {
    type Id = RealmId;
}

impl Realm {
    pub fn error_prototype(&self, kind: ErrorKind) -> ObjId {
        match kind {
            ErrorKind::Error => self.error_proto,
            ErrorKind::Type => self.type_error_proto,
            ErrorKind::Range => self.range_error_proto,
            ErrorKind::Reference => self.reference_error_proto,
            ErrorKind::Syntax => self.syntax_error_proto,
            ErrorKind::Uri => self.uri_error_proto,
            ErrorKind::Eval => self.eval_error_proto,
            ErrorKind::Internal => self.internal_error_proto,
            ErrorKind::Aggregate => self.aggregate_error_proto,
            ErrorKind::Suppressed => self.suppressed_error_proto,
        }
    }
}

pub(crate) fn ensure_regexp_shape(ctx: &mut ExecCtx<'_, '_>) -> JsResult<ShapeId> {
    let realm = ctx.realm_id();
    if let Some(shape) = ctx.heap().realm(realm).regexp_shape {
        return Ok(shape);
    }
    let proto = ctx.heap().realm(realm).regexp_proto;
    let shape = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let shape = Shape::build(
            &heap.ledger,
            Some(proto),
            1,
            [ShapeProp {
                key: PropKey::atom(Atom(qjs_bytecode::BuiltinAtom::LastIndex as u32)),
                flags: PropFlags::WRITABLE,
            }]
            .into_iter(),
            0,
            false,
        )?;
        heap.shapes.insert(&heap.ledger, shape)
    })?;
    ctx.runtime.heap.realms.get_mut(realm).unwrap().regexp_shape = Some(shape);
    Ok(shape)
}

pub(crate) fn ensure_regexp_result_shape(ctx: &mut ExecCtx<'_, '_>) -> JsResult<ShapeId> {
    let realm = ctx.realm_id();
    if let Some(shape) = ctx.heap().realm(realm).regexp_result_shape {
        return Ok(shape);
    }
    let proto = ctx.heap().realm(realm).array_proto;
    let shape = ctx.allocate(|rt| {
        let heap = &mut rt.heap;
        let shape = Shape::build(
            &heap.ledger,
            Some(proto),
            3,
            [
                ShapeProp {
                    key: PropKey::atom(Atom(qjs_bytecode::BuiltinAtom::Index as u32)),
                    flags: PropFlags::DEFAULT_DATA,
                },
                ShapeProp {
                    key: PropKey::atom(Atom(qjs_bytecode::BuiltinAtom::Input as u32)),
                    flags: PropFlags::DEFAULT_DATA,
                },
                ShapeProp {
                    key: PropKey::atom(Atom(qjs_bytecode::BuiltinAtom::Groups as u32)),
                    flags: PropFlags::DEFAULT_DATA,
                },
            ]
            .into_iter(),
            0,
            false,
        )?;
        heap.shapes.insert(&heap.ledger, shape)
    })?;
    ctx.runtime
        .heap
        .realms
        .get_mut(realm)
        .unwrap()
        .regexp_result_shape = Some(shape);
    Ok(shape)
}

pub(crate) fn ensure_null_proto_shape(ctx: &mut ExecCtx<'_, '_>) -> JsResult<ShapeId> {
    let realm = ctx.realm_id();
    if let Some(shape) = ctx.heap().realm(realm).null_proto_shape {
        return Ok(shape);
    }
    let shape = ctx.allocate(|rt| rt.heap.empty_shape(None))?;
    ctx.runtime
        .heap
        .realms
        .get_mut(realm)
        .unwrap()
        .null_proto_shape = Some(shape);
    Ok(shape)
}

fn bootstrap_ctor(
    heap: &mut Heap,
    realm: RealmId,
    ctor_init_shape: ShapeId,
    empty_name: Option<Value>,
    instance_proto: ObjId,
) -> AllocResult<ObjId> {
    let payload = native_payload(realm, NativeFnId::UNIMPLEMENTED);
    let mut props = PropSlots::new();
    if let Some(empty) = empty_name {
        props.try_reserve_exact(&heap.ledger, 3)?;
        props.push_reserved(Value::object(instance_proto));
        props.push_reserved(Value::int32(0));
        props.push_reserved(empty);
    } else {
        props.try_reserve_exact(&heap.ledger, 1)?;
        props.push_reserved(Value::object(instance_proto));
    }
    let ctor = heap.objects.insert(
        &heap.ledger,
        crate::object::JsObject {
            shape: ctor_init_shape,
            props,
            flags: ObjectFlags::EXTENSIBLE | ObjectFlags::IS_CONSTRUCTOR,
            data: payload,
        },
    )?;
    install_property(
        heap,
        instance_proto,
        Atom::get("constructor"),
        PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
        Value::object(ctor),
    )?;
    Ok(ctor)
}

fn native_payload(realm: RealmId, function: NativeFnId) -> ObjectData {
    ObjectData::NativeFunction(NativeFunctionData { function, realm })
}

fn bootstrap_methods(heap: &mut Heap, realm: RealmId) -> AllocResult<()> {
    let setup = heap.realm(realm);
    let global = setup.global_object;
    let object = setup.object_proto;
    let function = setup.function_proto;
    let symbol = setup.symbol_proto;
    let wrappers = [
        setup.string_proto,
        setup.number_proto,
        setup.boolean_proto,
        setup.symbol_proto,
        setup.bigint_proto,
    ];
    let errors = [
        ErrorKind::Error,
        ErrorKind::Type,
        ErrorKind::Range,
        ErrorKind::Reference,
        ErrorKind::Syntax,
        ErrorKind::Uri,
        ErrorKind::Eval,
        ErrorKind::Internal,
        ErrorKind::Aggregate,
        ErrorKind::Suppressed,
    ];
    let flags = PropFlags::WRITABLE | PropFlags::CONFIGURABLE;
    install_property(
        heap,
        global,
        Atom::get("globalThis"),
        flags,
        Value::object(global),
    )?;
    let mut method = |parent, key, id: NativeFnId| {
        let value = if let Some(auto) = Value::auto_native(id.0, 0, realm.index()) {
            auto
        } else {
            let data = native_payload(realm, id);
            let callee = heap.insert_object(Some(function), ObjectFlags::EXTENSIBLE, data)?;
            Value::object(callee)
        };
        install_property(heap, parent, key, flags, value)
    };
    method(
        object,
        PropKey::atom(Atom::VALUE_OF),
        NativeFnId::OBJECT_VALUE_OF,
    )?;
    method(
        object,
        PropKey::atom(Atom::TO_STRING),
        NativeFnId::OBJECT_TO_STRING,
    )?;
    for id in wrappers {
        method(
            id,
            PropKey::atom(Atom::VALUE_OF),
            NativeFnId::WRAPPER_VALUE_OF,
        )?;
        method(
            id,
            PropKey::atom(Atom::TO_STRING),
            NativeFnId::WRAPPER_TO_STRING,
        )?;
    }
    let sym = heap
        .symbol_registry
        .well_known(crate::symbol::WellKnown::ToPrimitive);
    let key = heap.symbols.get(sym).unwrap().key();
    let to_prim_val =
        if let Some(auto) = Value::auto_native(NativeFnId::WRAPPER_VALUE_OF.0, 0, realm.index()) {
            auto
        } else {
            let data = native_payload(realm, NativeFnId::WRAPPER_VALUE_OF);
            let callee = heap.insert_object(Some(function), ObjectFlags::EXTENSIBLE, data)?;
            Value::object(callee)
        };
    install_property(heap, symbol, key, PropFlags::CONFIGURABLE, to_prim_val)?;
    let empty = heap
        .strings
        .insert(&heap.ledger, JsString::from_str(&heap.ledger, "")?)?;
    for kind in errors {
        let id = heap.realm(realm).error_prototype(kind);
        let name = heap
            .strings
            .insert(&heap.ledger, JsString::from_str(&heap.ledger, kind.name())?)?;
        install_property(
            heap,
            id,
            PropKey::atom(Atom::NAME),
            flags,
            Value::string(name),
        )?;
        install_property(
            heap,
            id,
            PropKey::atom(Atom::MESSAGE),
            flags,
            Value::string(empty),
        )?;
    }
    Ok(())
}

fn install_property(
    heap: &mut Heap,
    object: ObjId,
    key: PropKey,
    flags: PropFlags,
    value: Value,
) -> AllocResult<()> {
    let id = heap.object(object).shape;
    let change = ShapeProp { key, flags };
    if heap.shape(id).is_building() {
        let ledger = heap.ledger.clone();
        let shape = heap.shapes.get_mut(id).unwrap();
        let obj = heap.objects.get_mut(object).unwrap();
        return shape.push_building_prop(&ledger, &mut obj.props, change, value);
    }
    if heap.building_shapes {
        let shape = heap.shape(id);
        let next = Shape::build_building(
            &heap.ledger,
            shape.proto,
            &shape.props,
            Some(change),
            shape.churn,
        )?;
        let new_id = heap.shapes.insert(&heap.ledger, next)?;
        let obj = heap.objects.get_mut(object).unwrap();
        obj.props.push(&heap.ledger, value)?;
        obj.shape = new_id;
        return Ok(());
    }
    let shape = heap.shape(id);
    let shared = !shape.is_mutable() && shape.props.len() < crate::shape::MAX_SHARED_SHAPE_PROPS;
    let existing = if shared {
        shape.find_transition(change, |target| heap.shapes.get_weak(target).is_some())
    } else {
        None
    };
    let new_id = if let Some(existing) = existing {
        existing
    } else {
        let next = Shape::build(
            &heap.ledger,
            shape.proto,
            shape.props.len() + 1,
            shape.props.iter().copied().chain([change]),
            shape.churn,
            shape.is_dictionary(),
        )?;
        let new_id = heap.shapes.insert(&heap.ledger, next)?;
        if shared {
            heap.shapes
                .get_mut(id)
                .unwrap()
                .add_transition(&heap.ledger, change, new_id)?;
        }
        new_id
    };
    let obj = heap.objects.get_mut(object).unwrap();
    obj.props.push(&heap.ledger, value)?;
    obj.shape = new_id;
    Ok(())
}

pub(crate) fn new_uri_error(heap: &mut Heap, realm: RealmId) -> AllocResult<ObjId> {
    let (shape, message) = match heap.realm(realm).uri_error {
        Some(cached) => cached,
        None => {
            let proto = heap.realm(realm).error_prototype(ErrorKind::Uri);
            let shape = Shape::build(
                &heap.ledger,
                Some(proto),
                1,
                [ShapeProp {
                    key: PropKey::atom(Atom::MESSAGE),
                    flags: PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
                }]
                .into_iter(),
                0,
                false,
            )?;
            let message = JsString::from_str(&heap.ledger, "malformed URI")?;
            let message = heap.strings.insert(&heap.ledger, message)?;
            let shape = heap.shapes.insert(&heap.ledger, shape)?;
            heap.realms.get_mut(realm).unwrap().uri_error = Some((shape, message));
            (shape, message)
        }
    };
    let message = Value::string(message);
    let data = TrackedBox::new(
        &heap.ledger,
        ErrorData {
            kind: ErrorKind::Uri,
            message,
            stack: Value::UNDEFINED,
            frames: TrackedVec::new(),
        },
    )?;
    heap.objects.insert(
        &heap.ledger,
        crate::object::JsObject {
            shape,
            props: PropSlots::one(message),
            flags: ObjectFlags::EXTENSIBLE,
            data: ObjectData::Error(data),
        },
    )
}

pub(crate) fn bootstrap_error_message(
    heap: &mut Heap,
    proto: ObjId,
    kind: ErrorKind,
    message: JsString,
) -> AllocResult<ObjId> {
    let message = Value::string(heap.strings.insert(&heap.ledger, message)?);
    let data = TrackedBox::new(
        &heap.ledger,
        ErrorData {
            kind,
            message,
            stack: Value::UNDEFINED,
            frames: TrackedVec::new(),
        },
    )?;
    let object = heap.insert_object(
        Some(proto),
        ObjectFlags::EXTENSIBLE,
        ObjectData::Error(data),
    )?;
    let flags = PropFlags::WRITABLE | PropFlags::CONFIGURABLE;
    install_property(heap, object, PropKey::atom(Atom::MESSAGE), flags, message)?;
    Ok(object)
}

pub struct Runtime {
    pub(crate) heap: Heap,
    pub vm: VmState,
    pub(crate) job_queue: TrackedVec<Job>,
    pub(crate) realms: TrackedVec<RealmId>,
    pub(crate) modules: TrackedVec<ModId>,
    pub(crate) active_coroutines: TrackedVec<CoroId>,
    pub(crate) native_dispatch: &'static [NativeFn],
    #[cfg(feature = "gc_stress")]
    stress_interval: usize,
    #[cfg(feature = "gc_stress")]
    stress_countdown: usize,
}

impl Runtime {
    pub fn new(limits: EngineLimits) -> AllocResult<Self> {
        let ledger = HeapLedger::new(limits.heap_bytes, (limits.heap_bytes / 32).min(4096))?;
        Self::with_ledger(limits, ledger)
    }

    pub fn with_ledger(limits: EngineLimits, ledger: HeapLedger) -> AllocResult<Self> {
        Self::with_ledger_inner(limits, ledger, false)
    }

    pub(crate) fn with_ledger_for_builtins(
        limits: EngineLimits,
        ledger: HeapLedger,
    ) -> AllocResult<Self> {
        Self::with_ledger_inner(limits, ledger, true)
    }

    fn with_ledger_inner(
        limits: EngineLimits,
        ledger: HeapLedger,
        keep_building: bool,
    ) -> AllocResult<Self> {
        let mut runtime = Self {
            heap: Heap::with_ledger(ledger)?,
            vm: VmState::new(limits),
            job_queue: TrackedVec::new(),
            realms: TrackedVec::new(),
            modules: TrackedVec::new(),
            active_coroutines: TrackedVec::new(),
            native_dispatch: &[],
            #[cfg(feature = "gc_stress")]
            stress_interval: 1,
            #[cfg(feature = "gc_stress")]
            stress_countdown: 1,
        };
        runtime.create_realm_inner(keep_building)?;
        if let Err(err) = runtime
            .vm
            .stack
            .try_reserve_emergency(&runtime.heap.ledger, 8)
        {
            if keep_building {
                runtime.heap.finish_bootstrap_shapes();
            }
            return Err(err);
        }
        Ok(runtime)
    }

    pub fn heap(&self) -> &Heap {
        &self.heap
    }
    pub fn default_realm(&self) -> RealmId {
        self.realms[0]
    }
    pub fn set_native_dispatch(&mut self, dispatch: &'static [NativeFn]) {
        self.native_dispatch = dispatch;
    }
    pub fn set_interrupt_handler(&mut self, handler: Option<Box<dyn FnMut() -> bool>>) {
        self.vm.interrupt_handler = handler;
    }
    pub fn can_block(&self) -> bool {
        self.vm.can_block
    }
    pub fn set_can_block(&mut self, can_block: bool) {
        self.vm.can_block = can_block;
    }
    pub fn pending_exception(&self) -> Option<Value> {
        self.vm.exception
    }
    pub fn is_oom(&self) -> bool {
        self.vm.oom_thrown
    }
    pub fn queued_jobs(&self) -> usize {
        self.job_queue.len()
    }

    pub fn create_realm(&mut self) -> AllocResult<RealmId> {
        self.create_realm_inner(false)
    }

    pub(crate) fn create_realm_for_builtins(&mut self) -> AllocResult<RealmId> {
        self.create_realm_inner(true)
    }

    fn create_realm_inner(&mut self, keep_building: bool) -> AllocResult<RealmId> {
        self.allocate(|runtime| {
            runtime.realms.try_reserve(&runtime.heap.ledger, 1)?;
            let realm = bootstrap(&mut runtime.heap, keep_building)?;
            runtime.realms.push_reserved(realm);
            Ok(realm)
        })
    }

    pub(crate) fn unroot_realm(&mut self, realm: RealmId) {
        if let Some(pos) = self.realms.iter().position(|&id| id == realm) {
            self.realms.remove(pos);
        }
    }

    pub fn install_builtins(&mut self, realm: RealmId) -> JsResult<()> {
        self.with_scope(realm, crate::builtins::install_all)
    }

    #[cfg(feature = "gc_stress")]
    pub fn set_gc_stress_interval(&mut self, interval: usize) {
        self.stress_interval = interval.max(1);
        self.stress_countdown = self.stress_interval;
    }

    pub(crate) fn allocation_point(&mut self) {
        let ledger = &self.heap.ledger;
        let threshold = ledger.gc_threshold();
        let ceiling = ledger.limit_bytes() - ledger.reserve_bytes();
        // At the ceiling, let a failed allocation request a collection.
        let collect = threshold < ceiling
            && (ledger.used_bytes() >= threshold || ledger.gc_allocated() >= threshold);
        #[cfg(feature = "gc_stress")]
        let collect = {
            self.stress_countdown -= 1;
            if self.stress_countdown == 0 {
                self.stress_countdown = self.stress_interval;
                true
            } else {
                collect
            }
        };
        if collect {
            self.collect();
        }
    }

    pub(crate) fn allocate<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> AllocResult<T>,
    ) -> AllocResult<T> {
        self.allocation_point();
        match f(self) {
            Err(AllocError::Limit | AllocError::Allocator) => {
                self.collect();
                f(self)
            }
            result => result,
        }
    }

    pub fn with_scope<R>(
        &mut self,
        realm: RealmId,
        f: impl for<'s> FnOnce(&mut ExecCtx<'_, 's>) -> R,
    ) -> R {
        self.heap.realm(realm);
        RootScope::new(self, realm).run(f)
    }

    pub fn release_persistent(&mut self, root: crate::root::Persistent) -> Option<Value> {
        if !root.belongs_to(&self.heap.ledger) {
            return None;
        }
        self.heap
            .persistent_roots
            .remove(root.id())
            .map(|entry| entry.0)
    }

    pub(crate) fn reserve_jobs(&mut self, extra: usize) -> AllocResult<()> {
        let mut pending = extra;
        for (_, object) in self.heap.objects.iter() {
            if let ObjectData::FinalizationRegistry(registry) = &object.data {
                pending = pending
                    .checked_add(registry.entries.len())
                    .ok_or(AllocError::Limit)?;
            }
        }
        self.job_queue.try_reserve(&self.heap.ledger, pending)
    }

    pub fn run_one_job(&mut self) -> JsResult<bool> {
        let Some(job) = self.job_queue.first().copied() else {
            return Ok(false);
        };
        let result = self.with_scope(job.realm, |ctx| {
            let callback = ctx.root(Value::object(job.callback))?;
            let this = ctx.root(Value::UNDEFINED)?;
            let arg = ctx.root(job.argument)?;
            let args = ctx.args(&[arg])?;
            ctx.runtime.job_queue.remove(0);
            crate::ops::call::call(ctx, callback, this, args)?;
            Ok(true)
        });
        self.clear_kept_objects();
        result
    }

    pub fn run_jobs(&mut self) -> JsResult<()> {
        while self.run_one_job()? {}
        Ok(())
    }
}
