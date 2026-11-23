use std::time::Instant;

use crate::gc::{Trace, Tracer};
use crate::value::{ModId, Value};

payloads!(ModuleNamespaceData { module: Option<ModId> });

#[derive(Debug, Default)]
pub struct DomExceptionData {
    pub(crate) name: Value,
    pub(crate) message: Value,
    pub(crate) code: i32,
}

impl Trace for DomExceptionData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.name.trace(tracer);
        self.message.trace(tracer);
    }
}

#[derive(Debug)]
pub struct PerformanceData {
    pub(crate) start: Instant,
}

impl Trace for PerformanceData {
    fn trace(&self, _: &Tracer<'_>) {}
}

#[derive(Clone, Copy, Debug)]
pub enum HostObjectData {
    ResolvingPair {
        promise: Value,
        resolved: bool,
    },
    CapabilityExecutor {
        resolve: Value,
        reject: Value,
    },
    ReactionJob {
        handler: Value,
        resolve: Value,
        reject: Value,
    },
    ThenableJob {
        value: Value,
        then: Value,
        promise: Value,
    },
    FinallyHandler {
        constructor: Value,
        callback: Value,
    },
    AggregateState {
        values: Value,
        settle: Value,
        remaining: u64,
    },
    AggregateElement {
        state: Value,
        index: u64,
        called: bool,
    },
    RawJson(Value),
    JsonParseNode {
        value: Value,
        start: usize,
        end: usize,
    },
}

impl Trace for HostObjectData {
    fn trace(&self, tracer: &Tracer<'_>) {
        match *self {
            Self::ResolvingPair { promise, .. } => promise.trace(tracer),
            Self::CapabilityExecutor { resolve, reject }
            | Self::ReactionJob {
                handler: _,
                resolve,
                reject,
            } => {
                resolve.trace(tracer);
                reject.trace(tracer);
                if let Self::ReactionJob { handler, .. } = *self {
                    handler.trace(tracer);
                }
            }
            Self::ThenableJob {
                value,
                then,
                promise,
            } => {
                value.trace(tracer);
                then.trace(tracer);
                promise.trace(tracer);
            }
            Self::FinallyHandler {
                constructor,
                callback,
            } => {
                constructor.trace(tracer);
                callback.trace(tracer);
            }
            Self::AggregateState { values, settle, .. } => {
                values.trace(tracer);
                settle.trace(tracer);
            }
            Self::AggregateElement { state, .. } => state.trace(tracer),
            Self::RawJson(value) | Self::JsonParseNode { value, .. } => value.trace(tracer),
        }
    }
}
