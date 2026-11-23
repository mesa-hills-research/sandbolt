use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::realm::NativeFnId;
use crate::value::{BcId, CoroId, ModId, ObjId, RealmId, Value, VarRefId};

mod promise;
mod weak;
pub use promise::*;
pub use weak::*;

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

#[derive(Debug, Default)]
pub struct ArgumentsData {
    pub elements: TrackedVec<Value>,
}
impl Trace for ArgumentsData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.elements.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub struct MappedArgumentsData {
    pub parameters: TrackedVec<Option<VarRefId>>,
}
impl Trace for MappedArgumentsData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.parameters.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Error,
    Type,
    Range,
    Reference,
    Syntax,
    Uri,
    Eval,
    Internal,
    Aggregate,
    Suppressed,
}
impl ErrorKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Type => "TypeError",
            Self::Range => "RangeError",
            Self::Reference => "ReferenceError",
            Self::Syntax => "SyntaxError",
            Self::Uri => "URIError",
            Self::Eval => "EvalError",
            Self::Internal => "InternalError",
            Self::Aggregate => "AggregateError",
            Self::Suppressed => "SuppressedError",
        }
    }
}

#[derive(Debug)]
pub struct ErrorData {
    pub kind: ErrorKind,
    pub message: Value,
}
impl Trace for ErrorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.message.trace(tracer);
    }
}

#[derive(Debug)]
pub struct ProxyData {
    pub target: Option<ObjId>,
    pub handler: Option<ObjId>,
    pub callable: bool,
}
impl Trace for ProxyData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.target.trace(tracer);
        self.handler.trace(tracer);
    }
}

macro_rules! payloads {
    ($( $name:ident { $( $field:ident : $ty:ty ),* } ),+ $(,)?) => {$(
        #[derive(Debug, Default)]
        pub struct $name { $(pub $field: $ty,)* }
        impl Trace for $name {
            fn trace(&self, _tracer: &Tracer<'_>) { $(self.$field.trace(_tracer);)* }
        }
    )+};
}

payloads!(
    StringObjectData { value: Value }, NumberObjectData { value: Value }, BooleanObjectData { value: Value },
    SymbolObjectData { value: Value }, BigIntObjectData { value: Value }, RegExpData { source: Value }, DateData {},
    ArrayBufferData {}, SharedArrayBufferData {}, TypedArrayData { buffer: Option<ObjId> }, DataViewData { buffer: Option<ObjId> },
    MapData { entries: TrackedVec<(Value, Value)> }, SetData { entries: TrackedVec<Value> },
    GeneratorData { state: Option<CoroId> },
    AsyncFunctionData { state: Option<CoroId> }, AsyncGeneratorData { state: Option<CoroId> }, IteratorData { target: Value },
    DisposableStackData { resources: TrackedVec<Value> }, AsyncDisposableStackData { resources: TrackedVec<Value> },
    ModuleNamespaceData { module: Option<ModId> }, DomExceptionData { message: Value }, PerformanceData {}, HostObjectData {},
);
