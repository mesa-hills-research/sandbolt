use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::{ObjId, RealmId, Value};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Reaction {
    pub callback: ObjId,
    pub realm: RealmId,
}

impl Trace for Reaction {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.callback.trace(tracer);
        self.realm.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub struct PromiseData {
    pub(crate) result: Option<Value>,
    pub(crate) reactions: TrackedVec<Reaction>,
    pub(crate) reject_reactions: TrackedVec<Reaction>,
    pub(crate) rejected: bool,
    pub(crate) is_handled: bool,
}

impl Trace for PromiseData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.result.trace(tracer);
        self.reactions.trace(tracer);
        self.reject_reactions.trace(tracer);
    }
}
