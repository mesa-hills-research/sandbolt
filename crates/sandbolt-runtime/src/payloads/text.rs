use std::rc::Rc;

use sandbolt_regexp::CompiledRegExp;

use crate::gc::{Trace, Tracer};
use crate::ledger::HeapLedger;
use crate::value::Value;

payloads!(StringObjectData { value: Value });

#[derive(Debug)]
pub struct RegExpData {
    pub source: Value,
    pub(crate) program: Rc<RegExpProgram>,
}

impl Trace for RegExpData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.source.trace(tracer);
    }
}

#[derive(Debug)]
pub(crate) struct RegExpProgram {
    pub compiled: CompiledRegExp,
    pub ledger: HeapLedger,
    pub bytes: usize,
}

impl Drop for RegExpProgram {
    fn drop(&mut self) {
        self.ledger.release_bytes(self.bytes);
    }
}

#[derive(Debug)]
pub struct StringIteratorData {
    pub string: Value,
    pub index: usize,
}

impl Trace for StringIteratorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.string.trace(tracer);
    }
}

#[derive(Debug)]
pub struct RegExpStringIteratorData {
    pub matcher: Value,
    pub string: Value,
    pub global: bool,
    pub unicode: bool,
    pub done: bool,
}

impl Trace for RegExpStringIteratorData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.matcher.trace(tracer);
        self.string.trace(tracer);
    }
}
