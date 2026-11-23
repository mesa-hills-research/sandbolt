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

macro_rules! heap_arenas {
    ($apply:ident) => {
        $apply!(
            objects, strings, shapes, symbols, bigints, bytecode, modules, var_refs, accessors,
            coroutines, realms
        )
    };
}

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
        let mut weak_maps = None;
        let mut finalizers = None;
        let mut other_weak = None;
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

            fn drain<T: ArenaItem + Trace>(arena: &Arena<T>, tracer: &Tracer<'_>) -> bool {
                let mut advanced = false;
                while let Some((_, value)) = arena.pop_gray() {
                    value.trace(tracer);
                    advanced = true;
                }
                advanced
            }

            loop {
                let mut traced = false;
                loop {
                    let mut advanced = false;
                    while let Some((id, object)) = heap.objects.pop_gray() {
                        let head = match object.data {
                            ObjectData::WeakMap(_) => Some(&mut weak_maps),
                            ObjectData::FinalizationRegistry(_) => Some(&mut finalizers),
                            ObjectData::WeakSet(_) | ObjectData::WeakRef(_) => {
                                Some(&mut other_weak)
                            }
                            _ => None,
                        };
                        if let Some(head) = head {
                            heap.objects.link_black(id, head);
                        }
                        object.trace(&tracer);
                        advanced = true;
                    }
                    macro_rules! drain_all {
                        (objects, $($arena:ident),+ $(,)?) => { $( advanced |= drain(&heap.$arena, &tracer); )+ };
                    }
                    heap_arenas!(drain_all);
                    if !advanced {
                        break;
                    }
                    traced = true;
                }
                if !traced || weak_maps.is_none() {
                    break;
                }
                // An ephemeron value can reach another map or another key.
                for object in heap.objects.linked(weak_maps) {
                    let ObjectData::WeakMap(map) = &object.data else {
                        unreachable!()
                    };
                    for &(key, value) in map.entries.iter() {
                        if marked(&heap.objects, &heap.symbols, key) {
                            value.trace(&tracer);
                        }
                    }
                }
            }
        }

        heap.atoms.prune(&heap.strings);
        heap.symbol_registry.prune(&heap.symbols);
        heap.prune_empty_shapes();
        for object in heap.objects.linked(finalizers) {
            let ObjectData::FinalizationRegistry(registry) = &object.data else {
                unreachable!()
            };
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
        macro_rules! sweep_all {
            ($($arena:ident),+ $(,)?) => { $( heap.$arena.sweep(); )+ };
        }
        heap_arenas!(sweep_all);

        for list in [weak_maps, finalizers, other_weak] {
            heap.objects.for_each_linked_mut(list, |object, objects| {
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
        }
        heap.shapes
            .for_each_live_mut(|shape, shapes| shape.transitions.retain(|(_, id)| shapes(*id)));

        let mut slack = 0;
        macro_rules! shrink_all {
            ($($arena:ident),+ $(,)?) => { $(
                heap.$arena.shrink_after_gc(&heap.ledger);
                slack += heap.$arena.slack_bytes();
            )+ };
        }
        heap_arenas!(shrink_all);
        shrink_all!(persistent_roots);
        heap.collections += 1;
        let ceiling = heap.ledger.limit_bytes() - heap.ledger.reserve_bytes();
        let used = heap.ledger.used_bytes();
        let live = used.saturating_sub(slack);
        heap.ledger
            .set_gc_threshold(used.saturating_add(live.max(512 * 1024)).min(ceiling));
    }

    pub fn clear_kept_objects(&mut self) {
        self.heap.kept_objects.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineLimits;
    use crate::ledger::AllocError;
    use crate::ops::weak::{new_weak_map, weak_map_set};

    #[test]
    fn ephemeron_values_can_reach_another_weak_map() {
        let mut runtime = Runtime::new(EngineLimits::default()).unwrap();
        runtime.with_scope(runtime.default_realm(), |ctx| {
            let first = new_weak_map(ctx).unwrap();
            let key = ctx.new_object().unwrap();
            let (first, key) = (ctx.get(first), ctx.get(key));
            let (map, value) = ctx.with_scope(|ctx| {
                let first = ctx.root(first).unwrap();
                let key = ctx.root(key).unwrap();
                let map = new_weak_map(ctx).unwrap();
                let value = ctx.new_object().unwrap();
                weak_map_set(ctx, first, key, map).unwrap();
                weak_map_set(ctx, map, key, value).unwrap();
                (ctx.object_id(map).unwrap(), ctx.object_id(value).unwrap())
            });
            ctx.collect();
            assert!(ctx.heap().objects.get_weak(map).is_some());
            assert!(ctx.heap().objects.get_weak(value).is_some());
        });
    }

    #[test]
    fn ceiling_defers_collection_until_an_allocation_needs_bytes() {
        let mut runtime = Runtime::new(EngineLimits {
            heap_bytes: 96 * 1024,
            ..Default::default()
        })
        .unwrap();
        #[cfg(feature = "gc_stress")]
        runtime.set_gc_stress_interval(usize::MAX);
        let ledger = runtime.heap.ledger.clone();
        let ceiling = ledger.limit_bytes() - ledger.reserve_bytes();
        let remaining = ceiling - ledger.used_bytes();
        ledger.charge_bytes(remaining).unwrap();
        ledger.set_gc_threshold(ceiling);

        let before = runtime.heap.collections;
        assert_eq!(runtime.allocate(|_| Ok(())), Ok(()));
        assert_eq!(runtime.heap.collections, before);
        let mut attempts = 0;
        let result = runtime.allocate(|_| {
            attempts += 1;
            ledger.charge_bytes(ceiling + 1)
        });
        assert_eq!(result, Err(AllocError::Limit));
        assert_eq!(attempts, 2);
        assert_eq!(runtime.heap.collections, before + 1);
        ledger.release_bytes(remaining);
    }
}
