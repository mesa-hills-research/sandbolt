use crate::atom::PropKey;
use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::{CoroId, Value};

payloads!(
    GeneratorData { state: Option<CoroId> },
    AsyncFunctionData { state: Option<CoroId> }, AsyncGeneratorData { state: Option<CoroId> },
    IteratorData { kind: IteratorKind },
);

#[derive(Clone, Copy, Debug)]
pub enum ArrayIteratorKind {
    Key,
    Value,
    Entry,
}

#[derive(Clone, Copy, Debug)]
pub struct IteratorRecordData {
    pub object: Value,
    pub next: Value,
}

impl Trace for IteratorRecordData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.object.trace(tracer);
        self.next.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub enum IteratorKind {
    #[default]
    Complete,
    Array {
        kind: ArrayIteratorKind,
        target: Value,
        index: u64,
    },
    Wrap(IteratorRecordData),
    Helper(HelperData),
    FromAsync(FromAsyncData),
}

impl Trace for IteratorKind {
    fn trace(&self, tracer: &Tracer<'_>) {
        match self {
            Self::Complete => {}
            Self::Array { target, .. } => target.trace(tracer),
            Self::Wrap(record) => record.trace(tracer),
            Self::Helper(data) => data.trace(tracer),
            Self::FromAsync(data) => data.trace(tracer),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HelperOp {
    Map,
    Filter,
    FlatMap,
    Drop,
    Take,
    Concat,
    Zip { keyed: bool, mode: ZipMode },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZipMode {
    Shortest,
    Longest,
    Strict,
}

#[derive(Clone, Copy, Debug)]
pub struct ZipInput {
    pub key: Option<PropKey>,
    pub record: Option<IteratorRecordData>,
    pub pad: Value,
}

impl Trace for ZipInput {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.key.trace(tracer);
        self.record.trace(tracer);
        self.pad.trace(tracer);
    }
}

#[derive(Debug)]
pub struct HelperData {
    pub op: HelperOp,
    pub source: Option<IteratorRecordData>,
    pub inner: Option<IteratorRecordData>,
    pub callback: Value,
    pub position: u64,
    pub limit: f64,
    pub running: bool,
    pub sequence: TrackedVec<IteratorRecordData>,
    pub zip: TrackedVec<ZipInput>,
}

impl HelperData {
    pub fn new(op: HelperOp) -> Self {
        Self {
            op,
            source: None,
            inner: None,
            callback: Value::UNDEFINED,
            position: 0,
            limit: 0.0,
            running: false,
            sequence: TrackedVec::new(),
            zip: TrackedVec::new(),
        }
    }
}

impl Trace for HelperData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.source.trace(tracer);
        self.inner.trace(tracer);
        self.callback.trace(tracer);
        self.sequence.trace(tracer);
        self.zip.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum FromAsyncPhase {
    Next,
    AsyncResult,
    SyncValue { done: bool },
    SyncResult { done: bool },
    MapValue,
    Mapped,
    Close,
}

#[derive(Debug)]
pub struct FromAsyncData {
    pub promise: Value,
    pub resolve: Value,
    pub source: Value,
    pub result: Value,
    pub mapper: Value,
    pub this_arg: Value,
    pub iterator: Option<IteratorRecordData>,
    pub sync: bool,
    pub length: u64,
    pub index: u64,
    pub phase: FromAsyncPhase,
    pub saved: Value,
}

impl Trace for FromAsyncData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.promise.trace(tracer);
        self.resolve.trace(tracer);
        self.source.trace(tracer);
        self.result.trace(tracer);
        self.mapper.trace(tracer);
        self.this_arg.trace(tracer);
        self.iterator.trace(tracer);
        self.saved.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub struct DisposableStackData {
    pub resources: TrackedVec<DisposableResource>,
    pub disposed: bool,
}

impl Trace for DisposableStackData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.resources.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub struct AsyncDisposableStackData {
    pub resources: TrackedVec<DisposableResource>,
    pub disposed: bool,
    pub promise: Option<Value>,
    pub error: Option<Value>,
    pub needs_await: bool,
    pub has_awaited: bool,
}

impl Trace for AsyncDisposableStackData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.resources.trace(tracer);
        self.promise.trace(tracer);
        self.error.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DisposableResource {
    pub value: Value,
    pub method: Value,
    pub adopt: bool,
    pub await_result: bool,
}

impl Trace for DisposableResource {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.value.trace(tracer);
        self.method.trace(tracer);
    }
}
