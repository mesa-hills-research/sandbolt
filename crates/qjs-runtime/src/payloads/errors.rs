use crate::gc::{Trace, Tracer};
use crate::value::Value;

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
    pub stack: Value,
}
impl Trace for ErrorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.message.trace(tracer);
        self.stack.trace(tracer);
    }
}
