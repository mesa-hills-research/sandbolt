use crate::ledger::TrackedVec;
use crate::value::{CoroId, Value};

payloads!(
    GeneratorData { state: Option<CoroId> },
    AsyncFunctionData { state: Option<CoroId> }, AsyncGeneratorData { state: Option<CoroId> },
    IteratorData { target: Value },
    DisposableStackData { resources: TrackedVec<Value> },
    AsyncDisposableStackData { resources: TrackedVec<Value> },
);
