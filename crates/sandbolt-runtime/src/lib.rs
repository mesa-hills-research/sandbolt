//! Pure-Rust ECMAScript runtime targeting QuickJS-ng v0.17.0 parity.
//!
//! Architecture overview (see `DESIGN.md`):
//! - Safe 8-byte NaN-boxed `Value(u64)` with generational arena handles.
//! - Precise stop-the-world tracing Mark-and-Sweep GC with ephemeron `WeakMap`,
//!   ECMA `KeepDuringJob` `WeakRef`, queued `FinalizationRegistry`, and `gc_stress`.
//! - Capacity-charged `HeapLedger` enforcing hard `heap_bytes` caps before allocation.
//! - Explicit heap-backed VM operand/call stacks with deterministic `stack_bytes`,
//!   unified coroutines, catchable OOM, and uncatchable host `Interrupted`.

#![deny(unsafe_code)]

pub mod api;
pub mod arena;
pub mod atom;
pub mod builtins;
pub mod gc;
pub mod heap;
pub mod ledger;
pub mod object;
pub mod ops;
mod payloads;
pub mod realm;
pub mod root;
pub mod shape;
pub mod string;
pub mod symbol;
pub mod value;
pub mod vm;

pub use realm::{NativeFnId, Runtime};
pub use root::{Args, ExecCtx, Handle, MutRoot, Persistent};
pub use value::{Abrupt, JsResult, Value, ValueKind};

#[cfg(test)]
mod tests;

/// Resource limits configured before any script executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineLimits {
    /// Hard cap on owned heap capacity in bytes.
    pub heap_bytes: usize,
    /// Hard cap on deterministic VM stack + re-entrancy budget in bytes.
    pub stack_bytes: usize,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            heap_bytes: 64 * 1024 * 1024,
            stack_bytes: 1024 * 1024,
        }
    }
}

/// A single JavaScript stack frame, innermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub file: String,
    pub function: String,
    pub line: u32,
    pub column: u32,
}

/// Host-visible execution error matching Arko `code_mode::engine::EngineError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    Thrown { message: String, frames: Vec<Frame> },
    OutOfMemory,
    Interrupted,
    Failed(String),
}
