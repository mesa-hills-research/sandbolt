use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::{BcId, Value};

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
pub struct ErrorFrame {
    pub code: BcId,
    pub pc: u32,
}
impl Trace for ErrorFrame {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.code.trace(tracer);
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
