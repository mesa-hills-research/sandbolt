use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::{BcId, ObjId, StrId, Value};

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

#[derive(Clone, Copy, Debug)]
pub enum FrameSource {
    Bytecode { code: BcId, pc: u32 },
    Parse { file: StrId, line: u32, column: u32 },
    Native,
}

#[derive(Clone, Copy, Debug)]
pub struct ErrorFrame {
    pub source: FrameSource,
    pub function: Option<ObjId>,
    pub name: Option<StrId>,
    pub constructor: bool,
}
impl Trace for ErrorFrame {
    fn trace(&self, tracer: &Tracer<'_>) {
        match self.source {
            FrameSource::Bytecode { code, .. } => code.trace(tracer),
            FrameSource::Parse { file, .. } => file.trace(tracer),
            FrameSource::Native => {}
        }
        self.function.trace(tracer);
        self.name.trace(tracer);
    }
}

#[derive(Debug)]
pub struct ErrorData {
    pub kind: ErrorKind,
    pub message: Value,
    pub stack: Value,
    pub frames: TrackedVec<ErrorFrame>,
}
impl Trace for ErrorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.message.trace(tracer);
        self.stack.trace(tracer);
        self.frames.trace(tracer);
    }
}
