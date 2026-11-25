use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::{RealmId, Value};

#[derive(Debug)]
pub struct ArrayData {
    pub elements: TrackedVec<Value>,
    pub length: u32,
    pub length_writable: bool,
    pub realm: RealmId,
}
impl Trace for ArrayData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.elements.trace(tracer);
        self.realm.trace(tracer);
    }
}
