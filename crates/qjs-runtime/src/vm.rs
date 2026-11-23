use crate::EngineLimits;
use crate::arena::ArenaItem;
use crate::gc::{Trace, Tracer};
use crate::ledger::TrackedVec;
use crate::value::*;

#[path = "vm_exec/mod.rs"]
pub(crate) mod exec;

use crate::atom::PropKey;
use qjs_bytecode::{
    ClosureVarDef, ExceptionHandler, FunctionFlags, FunctionKind, PcSourceMap, VarDef,
};

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
    pub(crate) compiled: Option<CompiledFunc>,
}

#[derive(Debug)]
pub(crate) struct CompiledFunc {
    pub code: TrackedVec<u8>,
    pub atoms: TrackedVec<PropKey>,
    pub name: Option<PropKey>,
    pub flags: FunctionFlags,
    pub kind: FunctionKind,
    pub strict: bool,
    pub arg_count: u16,
    pub var_count: u16,
    pub defined_arg_count: u16,
    pub stack_size: u16,
    pub vars: TrackedVec<VarDef>,
    pub closures: TrackedVec<ClosureVarDef>,
    pub handlers: TrackedVec<ExceptionHandler>,
    pub filename: TrackedVec<u8>,
    pub line_starts: TrackedVec<u32>,
    pub pc_map: TrackedVec<PcSourceMap>,
    pub source: Option<StrId>,
    pub source_start: u32,
    pub regex_constants: TrackedVec<u32>,
    pub module: Option<ModId>,
    pub hoisted: TrackedVec<(u32, u32)>,
    pub eval: Option<EvalEnvironment>,
    pub class_field_context: bool,
}

impl CompiledFunc {
    pub(crate) fn key(&self, atom: qjs_bytecode::UnitAtom) -> PropKey {
        if let Some(index) = atom.dynamic_index() {
            self.atoms[index as usize]
        } else {
            PropKey::atom(crate::atom::Atom(atom.raw()))
        }
    }
}

impl Trace for CompiledFunc {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.atoms.trace(tracer);
        self.name.trace(tracer);
        self.source.trace(tracer);
        self.module.trace(tracer);
        self.eval.trace(tracer);
    }
}

#[derive(Debug)]
pub(crate) struct EvalEnvironment {
    pub bindings: TrackedVec<EvalBinding>,
    pub declarations: Option<ObjId>,
    pub special: Option<[Value; 3]>,
}

impl Trace for EvalEnvironment {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.bindings.trace(tracer);
        self.declarations.trace(tracer);
        if let Some(special) = &self.special {
            special.as_slice().trace(tracer);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EvalBinding {
    Cell {
        name: PropKey,
        id: VarRefId,
        lexical: bool,
        writable: bool,
    },
    Object {
        id: ObjId,
        with: bool,
    },
}
impl Trace for EvalBinding {
    fn trace(&self, tracer: &Tracer<'_>) {
        match *self {
            Self::Cell { name, id, .. } => {
                name.trace(tracer);
                id.trace(tracer);
            }
            Self::Object { id, .. } => id.trace(tracer),
        }
    }
}
impl ArenaItem for BytecodeFunc {
    type Id = BcId;
}
impl Trace for BytecodeFunc {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.code.trace(tracer);
        self.constants.trace(tracer);
        self.compiled.trace(tracer);
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
    pub(crate) arg_slots: u32,
    pub(crate) capture_base: u32,
    pub(crate) floor: u32,
    pub(crate) last_pc: u32,
    pub(crate) return_call: bool,
    pub(crate) require_object: bool,
    pub(crate) coroutine: Option<CoroId>,
}
impl Trace for CallFrame {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.callee.trace(tracer);
        self.realm.trace(tracer);
        self.code.trace(tracer);
        self.new_target.trace(tracer);
        self.coroutine.trace(tracer);
    }
}

#[derive(Debug)]
pub struct CoroutineState {
    pub realm: RealmId,
    pub frames: TrackedVec<CallFrame>,
    pub stack: TrackedVec<Value>,
    pub open_vars: TrackedVec<VarRefId>,
    pub(crate) owner: Option<ObjId>,
    pub(crate) kind: FunctionKind,
    pub(crate) status: CoroStatus,
    pub(crate) initial_call: bool,
    pub(crate) capability: [Value; 3],
    pub(crate) requests: TrackedVec<CoroRequest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoroStatus {
    Running,
    Start,
    Yield,
    YieldStar,
    Await,
    AwaitReturn,
    Done,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CoroRequest {
    pub mode: u16,
    pub value: Value,
    pub capability: [Value; 3],
}

impl Trace for CoroRequest {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.value.trace(tracer);
        self.capability.as_slice().trace(tracer);
    }
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
        self.owner.trace(tracer);
        self.capability.as_slice().trace(tracer);
        self.requests.trace(tracer);
    }
}

#[derive(Debug, Default)]
pub struct ModuleRecord {
    pub realm: Option<RealmId>,
    pub namespace: Option<ObjId>,
    pub dependencies: TrackedVec<ModId>,
    pub exports: TrackedVec<Value>,
    pub(crate) lexical: TrackedVec<(PropKey, (VarRefId, bool))>,
    pub(crate) global_scope: bool,
    pub(crate) name: Option<PropKey>,
    pub(crate) module_type: u8,
    pub(crate) code: Option<BcId>,
    pub(crate) requests: TrackedVec<ModuleRequest>,
    pub(crate) imports: TrackedVec<ModuleImport>,
    pub(crate) export_bindings: TrackedVec<ModuleExport>,
    pub(crate) namespace_bindings: TrackedVec<(PropKey, VarRefId)>,
    pub(crate) namespace_ref: Option<VarRefId>,
    pub(crate) meta: Option<ObjId>,
    pub(crate) status: ModuleStatus,
    pub(crate) has_await: bool,
    pub(crate) capability: Option<(Value, (Value, Value))>,
    pub(crate) evaluation_error: Option<Value>,
    pub(crate) pending_dependencies: usize,
    pub(crate) waiting_host: bool,
    pub(crate) eval_index: Option<usize>,
    pub(crate) eval_ancestor: usize,
    pub(crate) cycle_root: Option<ModId>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleStatus {
    #[default]
    Unlinked,
    Linking,
    Linked,
    Visiting,
    Waiting,
    Async,
    Evaluated,
    Error,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModuleRequest {
    pub name: PropKey,
    pub module_type: u8,
}
impl Trace for ModuleRequest {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.name.trace(tracer);
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModuleImport {
    pub name: Option<PropKey>,
    pub dependency: usize,
    pub slot: usize,
}
impl Trace for ModuleImport {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.name.trace(tracer);
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ModuleExport {
    Local {
        name: PropKey,
        slot: usize,
    },
    Indirect {
        name: PropKey,
        dependency: usize,
        import_name: Option<PropKey>,
    },
    Star(usize),
}
impl Trace for ModuleExport {
    fn trace(&self, tracer: &Tracer<'_>) {
        match *self {
            Self::Local { name, .. } => name.trace(tracer),
            Self::Indirect {
                name, import_name, ..
            } => {
                name.trace(tracer);
                import_name.trace(tracer);
            }
            Self::Star(_) => {}
        }
    }
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
        self.name.trace(tracer);
        self.code.trace(tracer);
        self.requests.trace(tracer);
        self.imports.trace(tracer);
        self.export_bindings.trace(tracer);
        self.namespace_bindings.trace(tracer);
        self.namespace_ref.trace(tracer);
        self.meta.trace(tracer);
        self.capability.trace(tracer);
        self.evaluation_error.trace(tracer);
        self.cycle_root.trace(tracer);
        for (key, (reference, _)) in self.lexical.iter() {
            key.trace(tracer);
            reference.trace(tracer);
        }
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
    pub(crate) thrown_frames: Vec<crate::Frame>,
    pub(crate) host_callback: Option<crate::api::HostCallback>,
    pub(crate) print_callback: Option<crate::api::PrintCallback>,
    pub(crate) module_loader: Option<Box<dyn crate::api::ModuleLoader>>,
    pub(crate) resolution_failed: bool,
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
            can_block: false,
            limits,
            thrown_frames: Vec::new(),
            host_callback: None,
            print_callback: None,
            module_loader: None,
            resolution_failed: false,
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
