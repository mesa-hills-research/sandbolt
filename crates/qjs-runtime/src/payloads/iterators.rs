use crate::atom::PropKey;
use crate::gc::{Trace, Tracer};
use crate::ledger::{AllocResult, HeapLedger, TrackedVec};
use crate::object::HashIndex;
use crate::realm::NativeFnId;
use crate::value::{CoroId, ObjId, Value};

pub(crate) const ARRAY_NEXT_OFFSET: u16 = 22;
pub(crate) const ARRAY_ITERATOR_NEXT: NativeFnId =
    NativeFnId(crate::builtins::ARRAY_ITER_BASE + 128 + ARRAY_NEXT_OFFSET);

payloads!(
    GeneratorData { state: Option<CoroId> },
    AsyncFunctionData { state: Option<CoroId> }, AsyncGeneratorData { state: Option<CoroId> },
    IteratorData { kind: IteratorKind },
);

#[derive(Debug, Default)]
pub struct ForInIteratorData {
    pub(crate) target: Option<ObjId>,
    pub(crate) keys: Option<TrackedVec<PropKey>>,
    pub(crate) index: usize,
    pub(crate) dense: Option<DenseForIn>,
    visited: TrackedVec<PropKey>,
    by_key: HashIndex,
}

#[derive(Debug)]
pub(crate) struct DenseForIn {
    pub(crate) length: u32,
    pub(crate) active: bool,
    pub(crate) absent: TrackedVec<u32>,
}

impl ForInIteratorData {
    pub(crate) fn has_seen(&self, key: PropKey) -> bool {
        if let Some(dense) = &self.dense {
            if key == PropKey::atom(crate::atom::Atom::LENGTH) {
                return true;
            }
            if let crate::atom::KeyKind::Index(i) = key.decode()
                && i < dense.length
                && dense.absent.binary_search(&i).is_err()
            {
                return true;
            }
        }
        self.by_key
            .find(key_hash(key), |i| self.visited[i] == key)
            .is_some()
    }

    pub(crate) fn visit(&mut self, ledger: &HeapLedger, key: PropKey) -> AllocResult<()> {
        self.visited.try_reserve(ledger, 1)?;
        self.by_key.reserve(ledger)?;
        self.by_key.insert(key_hash(key), self.visited.len());
        self.visited.push_reserved(key);
        Ok(())
    }
}

fn key_hash(key: PropKey) -> u64 {
    let bits = key.bits();
    u64::from(bits ^ (bits >> 16))
}

impl Trace for ForInIteratorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.target.trace(tracer);
        self.keys.trace(tracer);
        self.visited.trace(tracer);
    }
}

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
    Chunks { size: usize },
    Windows { size: usize, allow_partial: bool },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperState {
    SuspendedStart,
    SuspendedYield,
    Executing,
    Completed,
}

#[derive(Debug)]
pub struct HelperData {
    pub op: HelperOp,
    pub source: Option<IteratorRecordData>,
    pub inner: Option<IteratorRecordData>,
    pub callback: Value,
    pub position: u64,
    pub limit: f64,
    pub state: HelperState,
    pub sequence: TrackedVec<IteratorRecordData>,
    pub zip: TrackedVec<ZipInput>,
    pub buffer: TrackedVec<Value>,
    pub buffer_start: usize,
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
            state: HelperState::SuspendedStart,
            sequence: TrackedVec::new(),
            zip: TrackedVec::new(),
            buffer: TrackedVec::new(),
            buffer_start: 0,
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
        self.buffer.trace(tracer);
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
