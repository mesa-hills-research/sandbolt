use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::{ObjId, RealmId, SymId, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WeakKey {
    Object(ObjId),
    Symbol(SymId),
}

impl WeakKey {
    pub const fn value(self) -> Value {
        match self {
            Self::Object(id) => Value::object(id),
            Self::Symbol(id) => Value::symbol(id),
        }
    }
}

#[derive(Debug, Default)]
pub struct WeakMapData {
    pub(crate) entries: TrackedVec<(WeakKey, Value)>,
}
impl Trace for WeakMapData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Debug, Default)]
pub struct WeakSetData {
    pub(crate) entries: TrackedVec<WeakKey>,
}
impl Trace for WeakSetData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Debug)]
pub struct WeakRefData {
    pub(crate) target: Option<WeakKey>,
}
impl Trace for WeakRefData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Clone, Copy, Debug)]
pub struct Registration {
    pub target: WeakKey,
    pub held: Value,
    pub token: Option<WeakKey>,
}

#[derive(Debug)]
pub struct FinalizationRegistryData {
    pub(crate) cleanup: ObjId,
    pub(crate) realm: RealmId,
    pub(crate) entries: TrackedVec<Registration>,
}
impl Trace for FinalizationRegistryData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.cleanup.trace(tracer);
        self.realm.trace(tracer);
        for entry in self.entries.iter() {
            entry.held.trace(tracer);
        }
    }
}
