use std::ops::{BitOr, BitOrAssign};

use crate::arena::ArenaItem;
use crate::gc::{Trace, Tracer};
use crate::ledger::{TrackedBox, TrackedVec};
use crate::value::{AccId, ObjId, ShapeId, Value};

pub use crate::payloads::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObjectFlags(u16);

impl ObjectFlags {
    pub const EXTENSIBLE: Self = Self(1);
    pub const IS_PROTOTYPE: Self = Self(2);
    pub const IS_CONSTRUCTOR: Self = Self(4);
    pub const IS_EXOTIC: Self = Self(8);
    pub const FAST_ARRAY: Self = Self(16);
    pub const IS_HTML_DDA: Self = Self(32);
    pub const IMMUTABLE_PROTO: Self = Self(64);

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub fn set(&mut self, flag: Self, on: bool) {
        if on {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

impl BitOr for ObjectFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for ObjectFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

#[derive(Debug)]
pub struct JsObject {
    pub shape: ShapeId,
    pub props: TrackedVec<Value>,
    pub flags: ObjectFlags,
    pub data: ObjectData,
}

impl ArenaItem for JsObject {
    type Id = ObjId;
}

impl Trace for JsObject {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.shape.trace(tracer);
        self.props.trace(tracer);
        self.data.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Accessor {
    pub getter: Option<ObjId>,
    pub setter: Option<ObjId>,
}
impl ArenaItem for Accessor {
    type Id = AccId;
}
impl Trace for Accessor {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.getter.trace(tracer);
        self.setter.trace(tracer);
    }
}

macro_rules! object_data {
    ($( $variant:ident($payload:ident) ),+ $(,)?) => {
        #[derive(Debug)]
        pub enum ObjectData {
            Ordinary,
            NativeFunction(NativeFunctionData),
            $( $variant(TrackedBox<$payload>), )+
        }

        impl Trace for ObjectData {
            fn trace(&self, tracer: &Tracer<'_>) {
                match self {
                    Self::Ordinary => {},
                    Self::NativeFunction(data) => data.trace(tracer),
                    $( Self::$variant(data) => data.trace(tracer), )+
                }
            }
        }
    };
}

object_data!(
    Array(ArrayData),
    BytecodeFunction(BytecodeFunctionData),
    BoundFunction(BoundFunctionData),
    Arguments(ArgumentsData),
    MappedArguments(MappedArgumentsData),
    StringObject(StringObjectData),
    NumberObject(NumberObjectData),
    BooleanObject(BooleanObjectData),
    SymbolObject(SymbolObjectData),
    BigIntObject(BigIntObjectData),
    Error(ErrorData),
    CallSite(ErrorFrame),
    RegExp(RegExpData),
    Date(DateData),
    ArrayBuffer(ArrayBufferData),
    SharedArrayBuffer(SharedArrayBufferData),
    TypedArray(TypedArrayData),
    DataView(DataViewData),
    Map(MapData),
    Set(SetData),
    CollectionIterator(CollectionIteratorData),
    WeakMap(WeakMapData),
    WeakSet(WeakSetData),
    WeakRef(WeakRefData),
    FinalizationRegistry(FinalizationRegistryData),
    Proxy(ProxyData),
    Promise(PromiseData),
    Generator(GeneratorData),
    AsyncFunction(AsyncFunctionData),
    AsyncGenerator(AsyncGeneratorData),
    Iterator(IteratorData),
    StringIterator(StringIteratorData),
    RegExpStringIterator(RegExpStringIteratorData),
    DisposableStack(DisposableStackData),
    AsyncDisposableStack(AsyncDisposableStackData),
    ModuleNamespace(ModuleNamespaceData),
    DomException(DomExceptionData),
    Performance(PerformanceData),
    HostObject(HostObjectData),
);
