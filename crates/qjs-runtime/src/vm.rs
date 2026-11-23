use crate::EngineLimits;
use crate::arena::ArenaItem;
use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::*;

pub const REENTRY_COST: usize = 1024;
pub const MAX_REENTRY_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug)]
pub enum MiniOp {
    Push(Value),
    Constant(u16),
    This,
    Arg(u16),
    Capture(u16),
    Callee,
    Pop,
    Dup,
    Call(u16),
    Return,
    Jump(u32),
    Try { target: u32, finally: bool },
    EndTry,
    ResumeThrow,
    Throw,
}

impl Trace for MiniOp {
    fn trace(&self, tracer: &Tracer<'_>) {
        if let Self::Push(value) = self {
            value.trace(tracer);
        }
    }
}

#[derive(Debug)]
pub struct BytecodeFunc {
    pub code: TrackedVec<MiniOp>,
    pub constants: TrackedVec<Value>,
}
impl ArenaItem for BytecodeFunc {
    type Id = BcId;
}
impl Trace for BytecodeFunc {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.code.trace(tracer);
        self.constants.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CallFrame {
    pub callee: ObjId,
    pub realm: RealmId,
    pub code: Option<BcId>,
    pub new_target: Option<ObjId>,
    pub pc: u32,
    pub base: u32,
    pub arg_count: u32,
}
impl Trace for CallFrame {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.callee.trace(tracer);
        self.realm.trace(tracer);
        self.code.trace(tracer);
        self.new_target.trace(tracer);
    }
}

#[derive(Debug)]
pub struct CoroutineState {
    pub realm: RealmId,
    pub frames: TrackedVec<CallFrame>,
    pub stack: TrackedVec<Value>,
    pub open_vars: TrackedVec<VarRefId>,
}
impl ArenaItem for CoroutineState {
    type Id = CoroId;
}
impl Trace for CoroutineState {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.realm.trace(tracer);
        self.frames.trace(tracer);
        self.stack.trace(tracer);
        self.open_vars.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub struct ModuleRecord {
    pub realm: Option<RealmId>,
    pub namespace: Option<ObjId>,
    pub dependencies: TrackedVec<ModId>,
    pub exports: TrackedVec<Value>,
}
impl ArenaItem for ModuleRecord {
    type Id = ModId;
}
impl Trace for ModuleRecord {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.realm.trace(tracer);
        self.namespace.trace(tracer);
        self.dependencies.trace(tracer);
        self.exports.trace(tracer);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum VarRefState {
    OpenStack(u32),
    OpenCoro { coro: CoroId, local: u32 },
    Closed(Value),
}

#[derive(Debug)]
pub struct VarRef {
    pub state: VarRefState,
}
impl ArenaItem for VarRef {
    type Id = VarRefId;
}
impl Trace for VarRef {
    fn trace(&self, tracer: &Tracer<'_>) {
        match self.state {
            VarRefState::OpenStack(_) => {}
            VarRefState::OpenCoro { coro, .. } => coro.trace(tracer),
            VarRefState::Closed(value) => value.trace(tracer),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Job {
    pub callback: ObjId,
    pub argument: Value,
    pub realm: RealmId,
}
impl Trace for Job {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.callback.trace(tracer);
        self.argument.trace(tracer);
        self.realm.trace(tracer);
    }
}

pub struct VmState {
    pub(crate) stack: TrackedVec<Value>,
    pub(crate) frames: TrackedVec<CallFrame>,
    pub(crate) reentry_depth: usize,
    pub(crate) exception: Option<Value>,
    pub(crate) oom_thrown: bool,
    pub(crate) interrupt_handler: Option<Box<dyn FnMut() -> bool>>,
    pub can_block: bool,
    pub limits: EngineLimits,
}

impl VmState {
    pub(crate) fn new(limits: EngineLimits) -> Self {
        Self {
            stack: TrackedVec::new(),
            frames: TrackedVec::new(),
            reentry_depth: 0,
            exception: None,
            oom_thrown: false,
            interrupt_handler: None,
            can_block: true,
            limits,
        }
    }

    pub fn stack_len(&self) -> usize {
        self.stack.len()
    }
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
    pub fn reentry_depth(&self) -> usize {
        self.reentry_depth
    }

    pub(crate) fn check_budget(&self, values: usize, frames: usize, reentries: usize) -> bool {
        let usage = self
            .stack
            .len()
            .saturating_add(values)
            .saturating_mul(size_of::<Value>())
            .saturating_add(
                self.frames
                    .len()
                    .saturating_add(frames)
                    .saturating_mul(size_of::<CallFrame>()),
            )
            .saturating_add(
                self.reentry_depth
                    .saturating_add(reentries)
                    .saturating_mul(REENTRY_COST),
            );
        usage <= self.limits.stack_bytes
            && self.reentry_depth.saturating_add(reentries) <= MAX_REENTRY_DEPTH
    }
}
