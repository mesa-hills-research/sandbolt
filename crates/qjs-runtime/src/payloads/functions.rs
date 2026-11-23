use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::realm::NativeFnId;
use crate::value::{BcId, ObjId, RealmId, Value, VarRefId};

#[derive(Debug)]
pub struct BytecodeFunctionData {
    pub code: BcId,
    pub realm: RealmId,
    pub closure: TrackedVec<Value>,
}
impl Trace for BytecodeFunctionData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.code.trace(tracer);
        self.realm.trace(tracer);
        self.closure.trace(tracer);
    }
}

#[derive(Debug)]
pub struct NativeFunctionData {
    pub function: NativeFnId,
    pub realm: RealmId,
}
impl Trace for NativeFunctionData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.realm.trace(tracer);
    }
}

#[derive(Debug)]
pub struct BoundFunctionData {
    pub target: ObjId,
    pub this_value: Value,
    pub args: TrackedVec<Value>,
}
impl Trace for BoundFunctionData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.target.trace(tracer);
        self.this_value.trace(tracer);
        self.args.trace(tracer);
    }
}

payloads!(
    ArgumentsData { elements: TrackedVec<Value> },
    MappedArgumentsData { parameters: TrackedVec<Option<VarRefId>> },
);
