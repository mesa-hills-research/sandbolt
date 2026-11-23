use std::marker::PhantomData;

use crate::atom::{AtomTable, PropKey};
use crate::heap::{Heap, PersistentRoot};
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedBox, TrackedVec};
use crate::object::*;
use crate::realm::{NativeFnId, Realm, Runtime, bootstrap_error};
use crate::string::{JsString, MAX_ROPE_DEPTH};
use crate::symbol::SymbolDescription;
use crate::value::*;
use crate::vm::{BytecodeFunc, Job, MiniOp};

type Brand<'s, T> = PhantomData<fn(&'s T) -> &'s T>;

/// An index into the operand stack. It cannot leave the scope that made it.
///
/// ```compile_fail
/// use qjs_runtime::{EngineLimits, Runtime, Value};
/// let mut vm = Runtime::new(EngineLimits::default()).unwrap();
/// let root = vm.with_scope(vm.default_realm(), |ctx| ctx.root(Value::UNDEFINED).unwrap());
/// # let _ = root;
/// ```
#[derive(Debug)]
pub struct Handle<'s, T = Value> {
    index: u32,
    brand: Brand<'s, T>,
}
impl<T> Copy for Handle<'_, T> {}
impl<T> Clone for Handle<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'s, T> Handle<'s, T> {
    pub(crate) fn at(index: usize) -> Self {
        Self {
            index: u32::try_from(index).expect("root stack index"),
            brand: PhantomData,
        }
    }
    pub(crate) fn index(self) -> usize {
        self.index as usize
    }
}

pub struct MutRoot<'s, T = Value>(Handle<'s, T>);
impl<'s, T> MutRoot<'s, T> {
    pub fn handle(&self) -> Handle<'s, T> {
        self.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Args<'s> {
    pub(crate) start: u32,
    pub(crate) len: u32,
    brand: Brand<'s, Value>,
}

impl<'s> Args<'s> {
    pub(crate) fn new(start: usize, len: usize) -> Self {
        Self {
            start: u32::try_from(start).expect("args start"),
            len: u32::try_from(len).expect("args length"),
            brand: PhantomData,
        }
    }
    pub const fn empty() -> Self {
        Self {
            start: 0,
            len: 0,
            brand: PhantomData,
        }
    }
    pub const fn len(self) -> usize {
        self.len as usize
    }
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
    pub fn get(self, index: usize) -> Option<Handle<'s>> {
        (index < self.len()).then(|| Handle::at(self.start as usize + index))
    }
    pub fn as_slice<'a>(self, ctx: &'a ExecCtx<'_, 's>) -> &'a [Value] {
        let start = self.start as usize;
        &ctx.runtime.vm.stack[start..start + self.len()]
    }
}

/// A host root. Release it through the runtime when the host no longer needs the value.
pub struct Persistent {
    id: PersistentId,
    ledger: HeapLedger,
}
impl Persistent {
    pub(crate) fn id(&self) -> PersistentId {
        self.id
    }
    pub(crate) fn belongs_to(&self, ledger: &HeapLedger) -> bool {
        self.ledger.same_account(ledger)
    }
}

pub struct RootScope<'rt> {
    runtime: &'rt mut Runtime,
    base: usize,
    frames: usize,
    reentry_depth: usize,
    realm: RealmId,
}

impl<'rt> RootScope<'rt> {
    pub(crate) fn new(runtime: &'rt mut Runtime, realm: RealmId) -> Self {
        Self {
            base: runtime.vm.stack.len(),
            frames: runtime.vm.frames.len(),
            reentry_depth: runtime.vm.reentry_depth,
            runtime,
            realm,
        }
    }

    pub(crate) fn run<R>(&mut self, f: impl for<'s> FnOnce(&mut ExecCtx<'_, 's>) -> R) -> R {
        f(&mut ExecCtx {
            runtime: self.runtime,
            realm: self.realm,
            brand: PhantomData,
        })
    }
}

impl Drop for RootScope<'_> {
    fn drop(&mut self) {
        self.runtime.vm.stack.truncate(self.base);
        self.runtime.vm.frames.truncate(self.frames);
        self.runtime.vm.reentry_depth = self.reentry_depth;
    }
}

pub struct ExecCtx<'rt, 's> {
    pub(crate) runtime: &'rt mut Runtime,
    pub(crate) realm: RealmId,
    brand: Brand<'s, Value>,
}

impl<'s> ExecCtx<'_, 's> {
    pub fn heap(&self) -> &Heap {
        &self.runtime.heap
    }
    pub fn realm(&self) -> &Realm {
        self.heap().realm(self.realm)
    }
    pub fn realm_id(&self) -> RealmId {
        self.realm
    }
    pub fn new_target(&self) -> Option<ObjId> {
        self.runtime.vm.frames.last().and_then(|frame| frame.new_target)
    }
    pub fn can_block(&self) -> bool {
        self.runtime.vm.can_block
    }
    pub fn ordinary_create_from_constructor(
        &mut self,
        new_target: ObjId,
        default_proto: impl FnOnce(&Realm) -> ObjId,
        flags: ObjectFlags,
        mut make_data: impl FnMut(&Heap) -> crate::ledger::AllocResult<ObjectData>,
    ) -> JsResult<Handle<'s>> {
        let rooted_nt = self.root(Value::object(new_target))?;
        let proto = crate::ops::property::get_property(
            self,
            rooted_nt,
            PropKey::atom(crate::atom::Atom::PROTOTYPE),
        )?;
        let proto_id = if let Some(obj) = proto.as_object() {
            obj
        } else {
            let realm_id = crate::ops::call::get_function_realm(self, new_target)?;
            default_proto(self.heap().realm(realm_id))
        };
        let rooted_proto = self.root(Value::object(proto_id))?;
        let id = self.allocate(|runtime| {
            let proto = runtime.vm.stack[rooted_proto.index()].as_object();
            let payload = make_data(&runtime.heap)?;
            runtime.heap.insert_object(proto, flags, payload)
        })?;
        self.root(Value::object(id))
    }

    pub fn with_scope<R>(&mut self, f: impl for<'a> FnOnce(&mut ExecCtx<'_, 'a>) -> R) -> R {
        RootScope::new(self.runtime, self.realm).run(f)
    }

    pub fn root(&mut self, value: Value) -> JsResult<Handle<'s>> {
        let index = self.runtime.vm.stack.len();
        self.check_stack(1)?;
        self.runtime.vm.stack.push_reserved(value);
        // Use the spare slot to keep the value visible if stack growth collects.
        if let Err(error) = self.grow_stack(1) {
            self.runtime.vm.stack.pop();
            return Err(error);
        }
        Ok(Handle::at(index))
    }

    pub fn mut_root(&mut self, value: Value) -> JsResult<MutRoot<'s>> {
        self.root(value).map(MutRoot)
    }
    pub fn get(&self, handle: Handle<'s>) -> Value {
        self.runtime.vm.stack[handle.index()]
    }
    pub fn set(&mut self, handle: &MutRoot<'s>, value: Value) {
        self.runtime.vm.stack[handle.0.index()] = value;
    }

    pub fn args(&mut self, handles: &[Handle<'s>]) -> JsResult<Args<'s>> {
        let start = self.runtime.vm.stack.len();
        self.reserve_stack(handles.len())?;
        for handle in handles {
            let value = self.get(*handle);
            self.runtime.vm.stack.push_reserved(value);
        }
        Ok(Args::new(start, handles.len()))
    }

    pub(crate) fn reserve_stack(&mut self, additional: usize) -> JsResult<()> {
        self.check_stack(additional)?;
        self.grow_stack(additional + 1)
    }

    fn check_stack(&mut self, additional: usize) -> JsResult<()> {
        if !self.runtime.vm.check_budget(additional, 0, 0)
            || self.runtime.vm.stack.len().saturating_add(additional) >= u32::MAX as usize
        {
            return Err(self.stack_overflow());
        }
        Ok(())
    }

    fn grow_stack(&mut self, additional: usize) -> JsResult<()> {
        let stack = &self.runtime.vm.stack;
        if stack.capacity() - stack.len() >= additional {
            return Ok(());
        }
        self.allocate(|runtime| {
            runtime
                .vm
                .stack
                .try_reserve(&runtime.heap.ledger, additional)
        })
    }

    pub fn collect(&mut self) {
        self.runtime.collect();
    }
    pub fn clear_kept_objects(&mut self) {
        self.runtime.clear_kept_objects();
    }

    pub fn check_interrupt(&mut self) -> JsResult<()> {
        if self
            .runtime
            .vm
            .interrupt_handler
            .as_mut()
            .is_some_and(|handler| handler())
        {
            self.take_exception();
            Err(Abrupt::Interrupted)
        } else {
            Ok(())
        }
    }

    pub fn throw(&mut self, value: Handle<'s>) -> Abrupt {
        self.throw_value(self.get(value), false)
    }
    pub fn throw_oom(&mut self) -> Abrupt {
        let value = self.realm().oom_error.prepare(&mut self.runtime.heap);
        self.throw_value(value, true)
    }

    pub(crate) fn stack_overflow(&mut self) -> Abrupt {
        let value = self.realm().stack_error.prepare(&mut self.runtime.heap);
        self.throw_value(value, false)
    }

    pub fn throw_type_error(&mut self, text: &str) -> Abrupt {
        self.throw_error(ErrorKind::Type, text)
    }
    pub fn throw_range_error(&mut self, text: &str) -> Abrupt {
        self.throw_error(ErrorKind::Range, text)
    }

    pub fn throw_error(&mut self, kind: ErrorKind, text: &str) -> Abrupt {
        let proto = self.realm().error_prototype(kind);
        match self
            .runtime
            .allocate(|runtime| bootstrap_error(&mut runtime.heap, proto, kind, text))
        {
            Ok(id) => self.throw_value(Value::object(id), false),
            Err(_) => self.throw_oom(),
        }
    }

    pub(crate) fn throw_value(&mut self, value: Value, oom: bool) -> Abrupt {
        self.runtime.vm.exception = Some(value);
        self.runtime.vm.oom_thrown = oom
            || value.as_object().is_some_and(|object| {
                self.runtime
                    .realms
                    .iter()
                    .any(|realm| self.runtime.heap.realm(*realm).oom_error.object == object)
            });
        Abrupt::Thrown
    }

    pub fn take_exception(&mut self) -> Option<(Value, bool)> {
        let oom = std::mem::take(&mut self.runtime.vm.oom_thrown);
        self.runtime.vm.exception.take().map(|value| (value, oom))
    }

    pub fn pending_exception(&self) -> Option<Value> {
        self.runtime.vm.exception
    }
    pub fn is_oom(&self) -> bool {
        self.runtime.vm.oom_thrown
    }

    pub(crate) fn allocate<T>(
        &mut self,
        f: impl FnMut(&mut Runtime) -> AllocResult<T>,
    ) -> JsResult<T> {
        self.runtime.allocate(f).map_err(|_| self.throw_oom())
    }

    pub fn persist(&mut self, root: Handle<'s>) -> JsResult<Persistent> {
        let value = self.get(root);
        let id = self.allocate(|runtime| {
            runtime
                .heap
                .persistent_roots
                .insert(&runtime.heap.ledger, PersistentRoot(value))
        })?;
        Ok(Persistent {
            id,
            ledger: self.heap().ledger.clone(),
        })
    }

    pub fn get_persistent(&mut self, root: &Persistent) -> JsResult<Option<Handle<'s>>> {
        if !root.belongs_to(&self.heap().ledger) {
            return Ok(None);
        }
        self.heap()
            .persistent_roots
            .get_weak(root.id)
            .map(|value| value.0)
            .map(|value| self.root(value))
            .transpose()
    }

    pub fn new_string(&mut self, text: &str) -> JsResult<Handle<'s>> {
        self.make_string(|ledger| JsString::from_str(ledger, text))
    }

    pub fn new_utf16(&mut self, units: &[u16]) -> JsResult<Handle<'s>> {
        self.make_string(|ledger| JsString::from_utf16(ledger, units))
    }

    fn make_string(
        &mut self,
        f: impl Fn(&HeapLedger) -> AllocResult<JsString>,
    ) -> JsResult<Handle<'s>> {
        let id = self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let string = f(&heap.ledger)?;
            heap.strings.insert(&heap.ledger, string)
        })?;
        self.root(Value::string(id))
    }

    pub fn concat_strings(&mut self, left: Handle<'s>, right: Handle<'s>) -> JsResult<Handle<'s>> {
        let (ValueKind::String(left), ValueKind::String(right)) =
            (self.get(left).decode(), self.get(right).decode())
        else {
            return Err(self.throw_type_error("expected strings"));
        };
        let id = self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let (a, b) = (heap.string(left), heap.string(right));
            let len = a.len().checked_add(b.len()).ok_or(AllocError::Limit)?;
            let is_wide = a.is_wide() || b.is_wide();
            let depth = a.depth().max(b.depth()) + 1;
            let string = if depth > MAX_ROPE_DEPTH {
                JsString::from_units(
                    &heap.ledger,
                    len as usize,
                    is_wide,
                    heap.string_units(left).chain(heap.string_units(right)),
                )?
            } else {
                JsString::Rope {
                    left,
                    right,
                    len,
                    is_wide,
                    depth,
                }
            };
            heap.strings.insert(&heap.ledger, string)
        })?;
        self.root(Value::string(id))
    }

    pub fn flatten_string(&mut self, value: Handle<'s>) -> JsResult<()> {
        let ValueKind::String(id) = self.get(value).decode() else {
            return Err(self.throw_type_error("expected a string"));
        };
        if self.heap().string(id).depth() == 0 {
            return Ok(());
        }
        self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let string = heap.string(id);
            let flat = JsString::from_units(
                &heap.ledger,
                string.len() as usize,
                string.is_wide(),
                heap.string_units(id),
            )?;
            *heap.strings.get_mut(id).unwrap() = flat;
            Ok(())
        })
    }

    pub fn intern(&mut self, value: Handle<'s>) -> JsResult<PropKey> {
        let ValueKind::String(id) = self.get(value).decode() else {
            return Err(self.throw_type_error("expected a string"));
        };
        let heap = self.heap();
        if let Some(key) = heap.atoms.find(&heap.strings, id) {
            if let Some(backing) = heap.dynamic_key_value(key) {
                self.root(backing)?;
            }
            return Ok(key);
        }
        self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            heap.atoms.insert(&heap.ledger, &heap.strings, id)
        })
    }

    pub fn intern_str(&mut self, text: &str) -> JsResult<PropKey> {
        if let Some(key) = AtomTable::builtin_key(text) {
            return Ok(key);
        }
        let string = self.new_string(text)?;
        self.intern(string)
    }

    pub fn new_symbol(
        &mut self,
        description: Option<Handle<'s>>,
        private: bool,
    ) -> JsResult<Handle<'s>> {
        let desc = match description.map(|s| self.get(s).decode()) {
            None => SymbolDescription::None,
            Some(ValueKind::String(id)) => SymbolDescription::String(id),
            _ => return Err(self.throw_type_error("expected a symbol description")),
        };
        let id = self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            heap.symbol_registry
                .insert(&mut heap.symbols, &heap.ledger, desc, false, private)
        })?;
        self.root(Value::symbol(id))
    }

    pub fn symbol_for(&mut self, name: Handle<'s>) -> JsResult<Handle<'s>> {
        let ValueKind::String(id) = self.get(name).decode() else {
            return Err(self.throw_type_error("expected a string"));
        };
        let heap = self.heap();
        let symbol = if let Some(existing) = heap.symbol_registry.find_registered(&heap.strings, id)
        {
            existing
        } else {
            self.allocate(|runtime| {
                let heap = &mut runtime.heap;
                heap.symbol_registry.insert(
                    &mut heap.symbols,
                    &heap.ledger,
                    SymbolDescription::String(id),
                    true,
                    false,
                )
            })?
        };
        self.root(Value::symbol(symbol))
    }

    pub fn new_object(&mut self) -> JsResult<Handle<'s>> {
        let proto = self.realm().object_proto;
        self.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |_| {
            Ok(ObjectData::Ordinary)
        })
    }

    pub fn new_object_with_proto(&mut self, proto: Option<Handle<'s>>) -> JsResult<Handle<'s>> {
        let proto = match proto {
            Some(handle) => Some(self.object_id(handle)?),
            None => None,
        };
        self.make_object(proto, ObjectFlags::EXTENSIBLE, |_| Ok(ObjectData::Ordinary))
    }

    pub(crate) fn make_object(
        &mut self,
        proto: Option<ObjId>,
        flags: ObjectFlags,
        f: impl Fn(&HeapLedger) -> AllocResult<ObjectData>,
    ) -> JsResult<Handle<'s>> {
        let id = self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let data = f(&heap.ledger)?;
            heap.insert_object(proto, flags, data)
        })?;
        self.root(Value::object(id))
    }

    pub fn object_id(&mut self, handle: Handle<'s>) -> JsResult<ObjId> {
        self.get(handle)
            .as_object()
            .ok_or_else(|| self.throw_type_error("expected an object"))
    }

    pub fn new_array(&mut self) -> JsResult<Handle<'s>> {
        let proto = self.realm().array_proto;
        let realm = self.realm;
        self.make_object(
            Some(proto),
            ObjectFlags::EXTENSIBLE | ObjectFlags::FAST_ARRAY | ObjectFlags::IS_EXOTIC,
            |ledger| {
                Ok(ObjectData::Array(TrackedBox::new(
                    ledger,
                    ArrayData {
                        elements: TrackedVec::new(),
                        length: 0,
                        length_writable: true,
                        realm,
                    },
                )?))
            },
        )
    }

    pub fn new_native(&mut self, function: NativeFnId, constructor: bool) -> JsResult<Handle<'s>> {
        let proto = self.realm().function_proto;
        let realm = self.realm;
        let mut flags = ObjectFlags::EXTENSIBLE;
        flags.set(ObjectFlags::IS_CONSTRUCTOR, constructor);
        self.make_object(Some(proto), flags, |ledger| {
            Ok(ObjectData::NativeFunction(TrackedBox::new(
                ledger,
                NativeFunctionData { function, realm },
            )?))
        })
    }

    pub fn new_bytecode(
        &mut self,
        ops: &[MiniOp],
        constants: Args<'s>,
        closure: Args<'s>,
        constructor: bool,
    ) -> JsResult<Handle<'s>> {
        assert!(
            ops.iter()
                .all(|op| !matches!(op, MiniOp::Push(value) if value.is_gc())),
            "use the constant pool for GC values"
        );
        let realm = self.realm;
        let proto = self.realm().function_proto;
        let mut flags = ObjectFlags::EXTENSIBLE;
        flags.set(ObjectFlags::IS_CONSTRUCTOR, constructor);
        let id = self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let stack = &runtime.vm.stack;
            let copy_args = |args: Args<'_>| {
                TrackedVec::from_slice(
                    &heap.ledger,
                    &stack[args.start as usize..args.start as usize + args.len()],
                )
            };
            let function = BytecodeFunc {
                code: TrackedVec::from_slice(&heap.ledger, ops)?,
                constants: copy_args(constants)?,
                compiled: None,
            };
            let code = heap.bytecode.insert(&heap.ledger, function)?;
            let data = TrackedBox::new(
                &heap.ledger,
                BytecodeFunctionData {
                    code,
                    realm,
                    closure: copy_args(closure)?,
                    home_object: None,
                    is_class_constructor: false,
                    is_arrow: false,
                    is_derived_constructor: false,
                },
            )?;
            heap.insert_object(Some(proto), flags, ObjectData::BytecodeFunction(data))
        })?;
        self.root(Value::object(id))
    }

    pub fn bind_function(
        &mut self,
        target: Handle<'s>,
        this: Handle<'s>,
        args: Args<'s>,
    ) -> JsResult<Handle<'s>> {
        let target = self.object_id(target)?;
        if !crate::ops::call::is_callable(self.heap(), target) {
            return Err(self.throw_type_error("expected a callable function"));
        }
        let value = self.get(this);
        let proto = self.heap().shape(self.heap().object(target).shape).proto;
        let mut flags = ObjectFlags::EXTENSIBLE;
        flags.set(
            ObjectFlags::IS_CONSTRUCTOR,
            self.heap()
                .object(target)
                .flags
                .contains(ObjectFlags::IS_CONSTRUCTOR),
        );
        let id = self.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let args = TrackedVec::from_slice(
                &heap.ledger,
                &runtime.vm.stack[args.start as usize..args.start as usize + args.len()],
            )?;
            let bound = TrackedBox::new(
                &heap.ledger,
                BoundFunctionData {
                    target,
                    this_value: value,
                    args,
                },
            )?;
            heap.insert_object(proto, flags, ObjectData::BoundFunction(bound))
        })?;
        self.root(Value::object(id))
    }

    pub fn enqueue_job(&mut self, callback: Handle<'s>, argument: Handle<'s>) -> JsResult<()> {
        let callback = self.object_id(callback)?;
        if !crate::ops::call::is_callable(self.heap(), callback) {
            return Err(self.throw_type_error("job callback is not callable"));
        }
        let job = Job {
            callback,
            argument: self.get(argument),
            realm: self.realm,
        };
        self.allocate(|runtime| {
            runtime.reserve_jobs(1)?;
            runtime.job_queue.push_reserved(job);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_are_four_bytes() {
        assert_eq!(size_of::<Handle<'_>>(), 4);
    }
}
