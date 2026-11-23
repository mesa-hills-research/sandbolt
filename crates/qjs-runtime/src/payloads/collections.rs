use crate::ledger::TrackedVec;
use crate::value::Value;

payloads!(
    MapData { entries: TrackedVec<(Value, Value)> },
    SetData { entries: TrackedVec<Value> },
);
