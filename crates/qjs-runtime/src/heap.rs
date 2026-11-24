use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use crate::arena::{Arena, ArenaItem};
use crate::atom::{AtomTable, KeyKind, PropKey};
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocResult, HeapLedger, TrackedVec};
use crate::object::{Accessor, HashIndex, JsObject, ObjectData, ObjectFlags, WeakKey};
use crate::realm::Realm;
use crate::shape::Shape;
use crate::string::{JsString, StringUnits};
use crate::symbol::{SymbolData, SymbolDescription, SymbolRegistry, WellKnown};
use crate::value::*;
use crate::vm::{BytecodeFunc, CoroutineState, ModuleRecord, VarRef};

#[derive(Debug, Default)]
pub struct BigIntData {
    pub negative: bool,
    pub limbs: BigIntLimbs,
}
impl ArenaItem for BigIntData {
    type Id = BigIntId;
}
impl Trace for BigIntData {
    fn trace(&self, _: &Tracer<'_>) {}
}

pub enum BigIntLimbs {
    Inline {
        len: u8,
        words: [u32; Self::INLINE_LEN],
    },
    Heap(TrackedVec<u32>),
}

impl BigIntLimbs {
    pub(crate) const INLINE_LEN: usize = 8;

    pub(crate) fn inline(words: &[u32]) -> Self {
        let mut inline = [0; Self::INLINE_LEN];
        inline[..words.len()].copy_from_slice(words);
        Self::Inline {
            len: words.len() as u8,
            words: inline,
        }
    }
}

impl Default for BigIntLimbs {
    fn default() -> Self {
        Self::inline(&[])
    }
}

impl std::fmt::Debug for BigIntLimbs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl std::ops::Deref for BigIntLimbs {
    type Target = [u32];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Inline { len, words } => &words[..usize::from(*len)],
            Self::Heap(words) => words,
        }
    }
}

impl From<TrackedVec<u32>> for BigIntLimbs {
    fn from(words: TrackedVec<u32>) -> Self {
        if words.len() <= Self::INLINE_LEN {
            Self::inline(&words)
        } else {
            Self::Heap(words)
        }
    }
}

pub(crate) struct PersistentRoot(pub Value);
impl ArenaItem for PersistentRoot {
    type Id = PersistentId;
}
impl Trace for PersistentRoot {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.0.trace(tracer);
    }
}

#[derive(Clone, Copy)]
struct EmptyShape {
    proto: ObjId,
    id: ShapeId,
    hash: u64,
}

#[derive(Default)]
struct EmptyShapeCache {
    null: Option<ShapeId>,
    entries: TrackedVec<EmptyShape>,
    index: HashIndex,
    seed: RandomState,
}

pub struct Heap {
    pub ledger: HeapLedger,
    pub objects: Arena<JsObject>,
    pub strings: Arena<JsString>,
    pub shapes: Arena<Shape>,
    pub symbols: Arena<SymbolData>,
    pub bigints: Arena<BigIntData>,
    pub bytecode: Arena<BytecodeFunc>,
    pub modules: Arena<ModuleRecord>,
    pub var_refs: Arena<VarRef>,
    pub accessors: Arena<Accessor>,
    pub coroutines: Arena<CoroutineState>,
    pub realms: Arena<Realm>,
    pub atoms: AtomTable,
    pub symbol_registry: SymbolRegistry,
    pub(crate) persistent_roots: Arena<PersistentRoot>,
    pub(crate) kept_objects: TrackedVec<WeakKey>,
    pub(crate) collections: usize,
    empty_shapes: EmptyShapeCache,
    pub(crate) building_shapes: bool,
}

impl Heap {
    pub fn new(limit: usize) -> AllocResult<Self> {
        let ledger = HeapLedger::new(limit, (limit / 32).min(4096))?;
        Self::with_ledger(ledger)
    }

    pub fn with_ledger(ledger: HeapLedger) -> AllocResult<Self> {
        let mut heap = Self {
            ledger,
            objects: Arena::new(),
            strings: Arena::new(),
            shapes: Arena::new(),
            symbols: Arena::new(),
            bigints: Arena::new(),
            bytecode: Arena::new(),
            modules: Arena::new(),
            var_refs: Arena::new(),
            accessors: Arena::new(),
            coroutines: Arena::new(),
            realms: Arena::new(),
            atoms: AtomTable::default(),
            symbol_registry: SymbolRegistry::default(),
            persistent_roots: Arena::new(),
            kept_objects: TrackedVec::new(),
            collections: 0,
            empty_shapes: EmptyShapeCache::default(),
            building_shapes: false,
        };
        for symbol in WellKnown::ALL {
            heap.symbol_registry.insert(
                &mut heap.symbols,
                &heap.ledger,
                SymbolDescription::WellKnown(symbol),
                false,
                false,
            )?;
        }
        Ok(heap)
    }

    pub fn collection_count(&self) -> usize {
        self.collections
    }
    pub fn object(&self, id: ObjId) -> &JsObject {
        self.objects.get(id).expect("missing object")
    }
    pub fn string(&self, id: StrId) -> &JsString {
        self.strings.get(id).expect("missing string")
    }
    pub fn shape(&self, id: ShapeId) -> &Shape {
        self.shapes.get(id).expect("missing shape")
    }
    pub fn realm(&self, id: RealmId) -> &Realm {
        self.realms.get(id).expect("missing realm")
    }
    pub fn string_units(&self, id: StrId) -> StringUnits<'_> {
        StringUnits::new(&self.strings, id)
    }
    pub fn array_index(&self, key: PropKey) -> Option<u32> {
        self.atoms.array_index(&self.strings, key)
    }

    pub fn weak_key(&self, value: Value) -> Option<WeakKey> {
        match value.decode() {
            ValueKind::Object(id) => {
                self.object(id);
                Some(WeakKey::Object(id))
            }
            ValueKind::Symbol(id) => {
                (!self.symbols.get(id)?.is_registered).then_some(WeakKey::Symbol(id))
            }
            _ => None,
        }
    }

    pub(crate) fn empty_shape(&mut self, proto: Option<ObjId>) -> AllocResult<ShapeId> {
        let cache = &mut self.empty_shapes;
        let Some(proto) = proto else {
            if let Some(id) = cache.null {
                return Ok(id);
            }
            let id = self.shapes.insert(&self.ledger, Shape::empty(None))?;
            cache.null = Some(id);
            return Ok(id);
        };
        let hash = cache.seed.hash_one(proto);
        if let Some(row) = cache
            .index
            .find(hash, |row| cache.entries[row].proto == proto)
        {
            return Ok(cache.entries[row].id);
        }
        cache.entries.try_reserve(&self.ledger, 1)?;
        cache.index.reserve(&self.ledger)?;
        let id = self
            .shapes
            .insert(&self.ledger, Shape::empty(Some(proto)))?;
        cache.index.insert(hash, cache.entries.len());
        cache.entries.push_reserved(EmptyShape { proto, id, hash });
        Ok(id)
    }

    pub(crate) fn prune_empty_shapes(&mut self) {
        let cache = &mut self.empty_shapes;
        if cache.null.is_some_and(|id| !self.shapes.is_marked(id)) {
            cache.null = None;
        }
        let mut to = 0;
        for from in 0..cache.entries.len() {
            let entry = cache.entries[from];
            if self.shapes.is_marked(entry.id) && self.objects.is_marked(entry.proto) {
                if from != to {
                    cache.index.relocate(entry.hash, from, to);
                    cache.entries[to] = entry;
                }
                to += 1;
            } else {
                cache.index.remove(entry.hash, |row| row == from);
            }
        }
        cache.entries.truncate(to);
    }

    pub(crate) fn insert_object(
        &mut self,
        proto: Option<ObjId>,
        flags: ObjectFlags,
        data: ObjectData,
    ) -> AllocResult<ObjId> {
        let shape = self.empty_shape(proto)?;
        self.objects.insert(
            &self.ledger,
            JsObject {
                shape,
                props: TrackedVec::new(),
                flags,
                data,
            },
        )
    }

    pub(crate) fn finish_bootstrap_shapes(&mut self) {
        self.building_shapes = false;
        let ledger = self.ledger.clone();
        self.shapes
            .for_each_live_mut(|shape, _| shape.clear_building(&ledger));
        self.objects.for_each_live_mut(|object, _| {
            object.props.shrink_to(&ledger, object.props.len());
        });
    }

    pub(crate) fn ensure_bootstrap_lookups(&mut self) -> AllocResult<()> {
        let ledger = self.ledger.clone();
        let mut error = None;
        self.shapes.for_each_live_mut(|shape, _| {
            if error.is_none()
                && let Err(err) = shape.ensure_lookup(&ledger)
            {
                error = Some(err);
            }
        });
        match error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    pub(crate) fn dynamic_key_value(&self, key: PropKey) -> Option<Value> {
        match key.decode() {
            KeyKind::Index(_) => None,
            KeyKind::String(atom) => self.atoms.dynamic_string(atom).map(Value::string),
            KeyKind::Symbol(token) => self.symbol_registry.from_token(token).map(Value::symbol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineLimits, Runtime};

    #[test]
    fn empty_shape_cache_shares_live_shapes_without_rooting_them() {
        let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
        let ordinary = ObjectFlags::EXTENSIBLE;
        let proto = runtime
            .heap
            .insert_object(None, ordinary, ObjectData::Ordinary)
            .unwrap();
        runtime.vm.stack.push_reserved(Value::object(proto));
        let first = runtime.heap.empty_shape(Some(proto)).unwrap();
        assert_eq!(runtime.heap.empty_shape(Some(proto)), Ok(first));
        runtime.collect();
        assert!(runtime.heap.objects.get_weak(proto).is_some());
        assert!(runtime.heap.shapes.get_weak(first).is_none());

        let child = runtime
            .heap
            .insert_object(Some(proto), ordinary, ObjectData::Ordinary)
            .unwrap();
        let next = runtime.heap.object(child).shape;
        assert_ne!(first, next);
        runtime.vm.stack.push_reserved(Value::object(child));
        runtime.collect();
        assert_eq!(runtime.heap.empty_shape(Some(proto)), Ok(next));
        runtime.vm.stack.clear();
        runtime.collect();
        assert!(runtime.heap.shapes.get_weak(next).is_none());
        assert!(runtime.heap.objects.get_weak(proto).is_none());
        assert!(
            runtime
                .heap
                .empty_shapes
                .entries
                .iter()
                .all(|entry| entry.proto != proto)
        );
    }
}
