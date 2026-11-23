use crate::arena::{Arena, ArenaItem};
use crate::atom::PropKey;
#[cfg(feature = "gc_stress")]
use crate::atom::{AtomTable, KeyKind};
use crate::heap::Heap;
use crate::ledger::TrackedVec;
use crate::object::{ObjectData, WeakKey};
use crate::realm::Runtime;
use crate::value::*;
use crate::vm::Job;

pub trait Trace {
    fn trace(&self, tracer: &Tracer<'_>);
}

pub struct Tracer<'h> {
    heap: &'h Heap,
}

impl Tracer<'_> {
    pub fn key(&self, key: PropKey) {
        if let Some(value) = self.heap.dynamic_key_value(key) {
            value.trace(self);
        } else {
            #[cfg(feature = "gc_stress")]
            match key.decode() {
                KeyKind::String(atom) if AtomTable::builtin(atom).is_none() => {
                    panic!("USE-AFTER-GC: unrooted or stale handle")
                }
                KeyKind::Symbol(_) => panic!("USE-AFTER-GC: unrooted or stale handle"),
                _ => {}
            }
        }
    }
}

impl<T: Trace> Trace for Option<T> {
    fn trace(&self, tracer: &Tracer<'_>) {
        if let Some(value) = self {
            value.trace(tracer);
        }
    }
}

impl<T: Trace> Trace for [T] {
    fn trace(&self, tracer: &Tracer<'_>) {
        for entry in self {
            entry.trace(tracer);
        }
    }
}

impl<T: Trace> Trace for TrackedVec<T> {
    fn trace(&self, tracer: &Tracer<'_>) {
        (**self).trace(tracer);
    }
}

impl<T: Trace, U: Trace> Trace for (T, U) {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.0.trace(tracer);
        self.1.trace(tracer);
    }
}

macro_rules! trace_ids {
    ($($id:ident => $arena:ident),+ $(,)?) => {$(
        impl Trace for $id {
            fn trace(&self, tracer: &Tracer<'_>) { tracer.heap.$arena.mark(*self); }
        }
    )+};
}

trace_ids!(ObjId => objects, StrId => strings, ShapeId => shapes, SymId => symbols, BigIntId => bigints,
    BcId => bytecode, ModId => modules, VarRefId => var_refs, AccId => accessors, CoroId => coroutines, RealmId => realms);

impl Trace for Value {
    fn trace(&self, tracer: &Tracer<'_>) {
        match self.decode() {
            ValueKind::Object(id) => id.trace(tracer),
            ValueKind::String(id) => id.trace(tracer),
            ValueKind::Symbol(id) => id.trace(tracer),
            ValueKind::BigInt(id) => id.trace(tracer),
            ValueKind::Bytecode(id) => id.trace(tracer),
            ValueKind::Module(id) => id.trace(tracer),
            ValueKind::VarRef(id) => id.trace(tracer),
            ValueKind::Accessor(id) => id.trace(tracer),
            ValueKind::Float64(_)
            | ValueKind::Int32(_)
            | ValueKind::Bool(_)
            | ValueKind::Null
            | ValueKind::Undefined
            | ValueKind::Uninitialized
            | ValueKind::Empty
            | ValueKind::ShortBigInt(_)
            | ValueKind::CatchOffset(_) => {}
        }
    }
}

fn marked(
    objects: &Arena<crate::object::JsObject>,
    symbols: &Arena<crate::symbol::SymbolData>,
    key: WeakKey,
) -> bool {
    match key {
        WeakKey::Object(id) => objects.is_marked(id),
        WeakKey::Symbol(id) => symbols.is_marked(id),
    }
}

impl Runtime {
    pub fn collect(&mut self) {
        let heap = &mut self.heap;
        {
            let tracer = Tracer { heap };
            self.vm.stack.trace(&tracer);
            self.vm.frames.trace(&tracer);
            self.vm.exception.trace(&tracer);
            self.vm.stack_capture.trace(&tracer);
            self.realms.trace(&tracer);
            self.modules.trace(&tracer);
            self.active_coroutines.trace(&tracer);
            self.job_queue.trace(&tracer);
            heap.symbol_registry.trace(&tracer);
            for (_, record) in heap.persistent_roots.iter() {
                record.trace(&tracer);
            }
            for key in heap.kept_objects.iter() {
                key.value().trace(&tracer);
            }

            fn scan<T: ArenaItem + Trace>(arena: &Arena<T>, tracer: &Tracer<'_>) -> bool {
                arena.scan_gray(|value| value.trace(tracer))
            }
            loop {
                let mut advanced = false;
                macro_rules! scan_all {
                    ($($arena:ident),+ $(,)?) => { $( advanced |= scan(&heap.$arena, &tracer); )+ };
                }
                for (id, object) in heap.objects.iter() {
                    if heap.objects.is_marked(id)
                        && let ObjectData::WeakMap(map) = &object.data
                    {
                        for &(key, value) in map.entries.iter() {
                            if marked(&heap.objects, &heap.symbols, key) {
                                value.trace(&tracer);
                            }
                        }
                    }
                }
                scan_all!(
                    objects, strings, shapes, symbols, bigints, bytecode, modules, var_refs,
                    accessors, coroutines, realms
                );
                if !advanced {
                    break;
                }
            }
        }

        heap.atoms.prune(&heap.strings);
        heap.symbol_registry.prune(&heap.symbols);
        for (id, object) in heap.objects.iter() {
            if !heap.objects.is_marked(id) {
                continue;
            }
            if let ObjectData::FinalizationRegistry(registry) = &object.data {
                for entry in registry.entries.iter() {
                    if !marked(&heap.objects, &heap.symbols, entry.target) {
                        self.job_queue.push_reserved(Job {
                            callback: registry.cleanup,
                            argument: entry.held,
                            realm: registry.realm,
                        });
                    }
                }
            }
        }
        heap.objects.sweep();
        heap.symbols.sweep();
        heap.shapes.sweep();
        heap.strings.sweep();
        heap.bigints.sweep();
        heap.bytecode.sweep();
        heap.modules.sweep();
        heap.var_refs.sweep();
        heap.accessors.sweep();
        heap.coroutines.sweep();
        heap.realms.sweep();

        heap.objects.for_each_live_mut(|object, objects| {
            let alive = |key| match key {
                WeakKey::Object(id) => objects(id),
                WeakKey::Symbol(id) => heap.symbols.get_weak(id).is_some(),
            };
            match &mut object.data {
                ObjectData::WeakMap(map) => map.entries.retain(|(key, _)| alive(*key)),
                ObjectData::WeakSet(set) => set.entries.retain(|key| alive(*key)),
                ObjectData::WeakRef(reference)
                    if reference.target.is_some_and(|key| !alive(key)) =>
                {
                    reference.target = None
                }
                ObjectData::FinalizationRegistry(registry) => {
                    registry.entries.retain(|entry| alive(entry.target));
                    for entry in registry.entries.iter_mut() {
                        if entry.token.is_some_and(|token| !alive(token)) {
                            entry.token = None;
                        }
                    }
                }
                _ => {}
            }
        });
        heap.shapes
            .for_each_live_mut(|shape, shapes| shape.transitions.retain(|(_, id)| shapes(*id)));
        heap.collections += 1;
        let ceiling = heap.ledger.limit_bytes() - heap.ledger.reserve_bytes();
        heap.ledger
            .set_gc_threshold(heap.ledger.used_bytes().saturating_mul(2).min(ceiling));
    }

    pub fn clear_kept_objects(&mut self) {
        self.heap.kept_objects.clear();
    }
}
