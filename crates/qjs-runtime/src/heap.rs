use crate::arena::{Arena, ArenaItem};
use crate::atom::{AtomTable, KeyKind, PropKey};
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocResult, HeapLedger, TrackedVec};
use crate::object::{Accessor, JsObject, ObjectData, ObjectFlags, WeakKey};
use crate::realm::Realm;
use crate::shape::Shape;
use crate::string::{JsString, StringUnits};
use crate::symbol::{SymbolData, SymbolDescription, SymbolRegistry, WellKnown};
use crate::value::*;
use crate::vm::{BytecodeFunc, CoroutineState, ModuleRecord, VarRef};

#[derive(Debug, Default)]
pub struct BigIntData {
    pub negative: bool,
    pub limbs: TrackedVec<u32>,
}
impl ArenaItem for BigIntData {
    type Id = BigIntId;
}
impl Trace for BigIntData {
    fn trace(&self, _: &Tracer<'_>) {}
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
}

impl Heap {
    pub fn new(limit: usize) -> AllocResult<Self> {
        let mut heap = Self {
            ledger: HeapLedger::new(limit, (limit / 32).min(4096))?,
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
        let shared = self.shapes.iter().find_map(|(id, s)| {
            (s.proto == proto && !s.dictionary && s.props.is_empty()).then_some(id)
        });
        if let Some(id) = shared {
            return Ok(id);
        }
        self.shapes.insert(&self.ledger, Shape::empty(proto))
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

    pub(crate) fn dynamic_key_value(&self, key: PropKey) -> Option<Value> {
        match key.decode() {
            KeyKind::Index(_) => None,
            KeyKind::String(atom) => self.atoms.dynamic_string(atom).map(Value::string),
            KeyKind::Symbol(token) => self.symbol_registry.from_token(token).map(Value::symbol),
        }
    }
}
