//! Instruction, operand and control-flow checks for compiled code.

use crate::{
    ClosureKind, ConstEntry, ExportEntry, FunctionBytecode, Instruction, Opcode, Operands,
    ProgramImage, ScopeLink, UnitAtom, VarDef,
};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    pub pc: usize,
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pc {}: {}", self.pc, self.message)
    }
}

impl std::error::Error for VerifyError {}

fn error(pc: u32, message: impl Into<String>) -> VerifyError {
    VerifyError {
        pc: pc as usize,
        message: message.into(),
    }
}

fn push<T>(items: &mut Vec<T>, value: T) -> Result<(), VerifyError> {
    items
        .try_reserve(1)
        .map_err(|_| error(0, "cannot allocate verifier state"))?;
    items.push(value);
    Ok(())
}

/// Checks a function against its declared dynamic atom count and stack limit.
/// Use [`verify_image`] to check references across the whole unit.
pub fn verify(func: &FunctionBytecode) -> Result<(), VerifyError> {
    stack_size(func, func.stack_size).map(|_| ())
}

/// Checks a function and returns its peak stack height without using the
/// declared `stack_size`. An assembler can use this to set that field.
pub fn compute_stack_size(func: &FunctionBytecode) -> Result<u16, VerifyError> {
    stack_size(func, u16::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageVerifyError {
    Function { index: usize, error: VerifyError },
    Metadata(String),
}

impl fmt::Display for ImageVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Function { index, error } => write!(f, "function {index}, {error}"),
            Self::Metadata(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ImageVerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Function { error, .. } => Some(error),
            Self::Metadata(_) => None,
        }
    }
}

/// Checks every function and the image's cross-table references.
pub fn verify_image(image: &ProgramImage) -> Result<(), ImageVerifyError> {
    check_image(image, true)
}

/// Checks the image's cross-table references, for an image whose functions
/// were each checked as they were assembled ([`compute_stack_size`]).
pub fn verify_image_references(image: &ProgramImage) -> Result<(), ImageVerifyError> {
    check_image(image, false)
}

fn check_image(image: &ProgramImage, functions: bool) -> Result<(), ImageVerifyError> {
    let metadata = |message: &str| ImageVerifyError::Metadata(message.into());
    if image.entry_function as usize >= image.functions.len() {
        return Err(metadata("entry function index is out of bounds"));
    }
    if image.atoms.len() > (u32::MAX - crate::FIRST_DYNAMIC_ATOM + 1) as usize {
        return Err(metadata("too many dynamic atoms"));
    }
    let atom_count = image.atoms.len() as u32;
    let valid_atom = |atom: UnitAtom| atom.is_in_bounds(atom_count);
    for (index, func) in image.functions.iter().enumerate() {
        let function_error = |error| ImageVerifyError::Function { index, error };
        if func.atom_count > atom_count {
            return Err(function_error(error(
                0,
                "dynamic atom count exceeds the image table",
            )));
        }
        if functions {
            verify(func).map_err(function_error)?;
        }
        for constant in &func.constants {
            if let ConstEntry::Function(child) = constant {
                let child = image.functions.get(*child as usize).ok_or_else(|| {
                    function_error(error(0, "function constant index is out of bounds"))
                })?;
                for capture in &child.closure_vars {
                    let bound = match capture.kind {
                        ClosureKind::Local => usize::from(func.var_count),
                        ClosureKind::Argument => usize::from(func.arg_count),
                        ClosureKind::Ref | ClosureKind::GlobalRef => func.closure_vars.len(),
                        _ => continue,
                    };
                    if usize::from(capture.index) >= bound {
                        return Err(function_error(error(
                            0,
                            "enclosing capture index is out of bounds",
                        )));
                    }
                }
            }
        }
    }
    for module in &image.modules {
        let func = image
            .functions
            .get(module.function as usize)
            .ok_or_else(|| metadata("module function index is out of bounds"))?;
        if !valid_atom(module.name) {
            return Err(metadata("module name atom is out of bounds"));
        }
        for request in &module.requested_modules {
            if !valid_atom(request.name)
                || request
                    .attributes
                    .iter()
                    .any(|a| !valid_atom(a.key) || !valid_atom(a.value))
            {
                return Err(metadata("module request atom is out of bounds"));
            }
        }
        let valid_request = |index: u32| (index as usize) < module.requested_modules.len();
        let valid_slot = |slot: u16| usize::from(slot) < func.closure_vars.len();
        let valid_name = |name: Option<UnitAtom>| name.is_none_or(valid_atom);
        for import in &module.imports {
            if !valid_request(import.requested_module)
                || !valid_name(import.import_name)
                || !valid_slot(import.closure_var)
            {
                return Err(metadata("module import reference is out of bounds"));
            }
        }
        for export in &module.exports {
            let valid = match *export {
                ExportEntry::Local {
                    export_name,
                    closure_var,
                } => valid_atom(export_name) && valid_slot(closure_var),
                ExportEntry::Indirect {
                    export_name,
                    requested_module,
                    import_name,
                } => {
                    valid_atom(export_name)
                        && valid_request(requested_module)
                        && valid_name(import_name)
                }
                ExportEntry::Star { requested_module } => valid_request(requested_module),
            };
            if !valid {
                return Err(metadata("module export reference is out of bounds"));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Decoded {
    pc: u32,
    instruction: Instruction,
    target: Option<usize>,
    handler: Option<HandlerEdge>,
}

#[derive(Debug, Clone, Copy)]
struct HandlerEdge {
    target: usize,
    depth: u16,
}

fn boundary(code: &[Decoded], pc: u32) -> Option<usize> {
    code.binary_search_by_key(&pc, |insn| insn.pc).ok()
}

fn validate_operands(
    func: &FunctionBytecode,
    pc: u32,
    insn: Instruction,
) -> Result<(), VerifyError> {
    use Opcode::*;
    use Operands::*;

    let atom = |name: UnitAtom| {
        if name.is_in_bounds(func.atom_count) {
            Ok(())
        } else {
            Err(error(
                pc,
                format!("atom index {} is out of bounds", name.raw()),
            ))
        }
    };
    let check = |kind, index, bound| {
        if index < bound {
            Ok(())
        } else {
            Err(error(pc, format!("{kind} index {index} is out of bounds")))
        }
    };
    let local = |index| check("local", usize::from(index), usize::from(func.var_count));
    let arg = |index| check("argument", usize::from(index), usize::from(func.arg_count));
    let varref = |index| {
        check(
            "closure varref",
            usize::from(index),
            func.closure_vars.len(),
        )
    };
    let eval = |first| {
        // Zero and u16::MAX represent the two scope-chain terminators.
        if first == 0 || first == u16::MAX {
            Ok(())
        } else {
            local(first - 1)
        }
    };
    match insn.operands {
        Constant(index) => {
            check("constant", index as usize, func.constants.len())?;
            if matches!(insn.opcode, Fclosure | Fclosure8)
                && !matches!(func.constants[index as usize], ConstEntry::Function(_))
            {
                return Err(error(pc, "closure operand is not a function constant"));
            }
        }
        Local(index) => {
            local(index)?;
            if matches!(insn.opcode, UsingDispose | UsingDisposeAsync) {
                // The cached disposal method occupies the next local.
                local(index + 1)?;
            }
        }
        Locals(a, b) => {
            local(a)?;
            local(b)?;
        }
        Argument(index) => arg(index)?,
        VarRef(index) => varref(index)?,
        Operands::Eval { scope, .. } => eval(scope)?,
        Unsigned(first) if insn.opcode == ApplyEval => eval(first as u16)?,
        Unsigned(first) if insn.opcode == Rest && first > u32::from(func.arg_count) => {
            return Err(error(pc, "rest starts past the argument slots"));
        }
        Unsigned(kind) if insn.opcode == SpecialObject && kind > 7 => {
            return Err(error(pc, "invalid special_object selector"));
        }
        Atom(name) if !(insn.opcode == SetName && name.is_none()) => atom(name)?,
        AtomU8(name, kind) => {
            if !(insn.opcode == ThrowError && matches!(kind, 3 | 4) && name.is_none()) {
                atom(name)?;
            }
        }
        AtomJumpU8(name, ..) | AtomJumpU16(name, ..) => atom(name)?,
        AtomU16(name, index) => {
            atom(name)?;
            match insn.opcode {
                MakeLocRef => local(index)?,
                MakeArgRef => arg(index)?,
                MakeVarRefRef => varref(index)?,
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

fn decode(func: &FunctionBytecode) -> Result<Vec<Decoded>, VerifyError> {
    let len =
        u32::try_from(func.code.len()).map_err(|_| error(0, "bytecode exceeds the PC range"))?;
    if len == 0 {
        return Err(error(0, "empty bytecode"));
    }
    let mut code = Vec::new();
    let mut pc = 0;
    while pc < len {
        let instruction = Instruction::decode(&func.code, pc as usize)
            .map_err(|err| error(pc, err.to_string()))?;
        validate_operands(func, pc, instruction)?;
        push(
            &mut code,
            Decoded {
                pc,
                instruction,
                target: None,
                handler: None,
            },
        )?;
        pc += u32::from(instruction.opcode.info().size);
    }
    for i in 0..code.len() {
        let pc = code[i].pc;
        if let Some(target) = code[i].instruction.branch_target(pc) {
            code[i].target = Some(
                u32::try_from(target)
                    .ok()
                    .and_then(|pc| boundary(&code, pc))
                    .ok_or_else(|| {
                        error(
                            pc,
                            format!("branch target {target} is not an instruction boundary"),
                        )
                    })?,
            );
        }
    }
    validate_metadata(func, &code, len)?;
    attach_handlers(func, &mut code, len)?;
    Ok(code)
}

fn validate_metadata(
    func: &FunctionBytecode,
    code: &[Decoded],
    len: u32,
) -> Result<(), VerifyError> {
    let vars = usize::from(func.arg_count) + usize::from(func.var_count);
    if !func.vars.is_empty() && func.vars.len() != vars {
        return Err(error(
            0,
            "binding metadata disagrees with the argument and local counts",
        ));
    }
    if !func.name.is_none() && !func.name.is_in_bounds(func.atom_count) {
        return Err(error(0, "function name atom is out of bounds"));
    }
    for var in &func.vars {
        if (!var.name.is_none() && !var.name.is_in_bounds(func.atom_count))
            || matches!(var.scope_next, ScopeLink::Local(index) if index >= func.var_count)
            || (!func.scopes.is_empty() && usize::from(var.scope_level) >= func.scopes.len())
            || var.captured != var.var_ref_index.is_some()
            || var
                .var_ref_index
                .is_some_and(|slot| slot >= func.var_ref_count)
        {
            return Err(error(0, "invalid binding definition"));
        }
    }
    if !func.vars.is_empty() {
        validate_scope_links(&func.vars[usize::from(func.arg_count)..])?;
    }
    for capture in &func.closure_vars {
        if !capture.name.is_none() && !capture.name.is_in_bounds(func.atom_count) {
            return Err(error(0, "closure name atom is out of bounds"));
        }
    }
    for (i, scope) in func.scopes.iter().enumerate() {
        let end_or_boundary = |pc| pc == len || boundary(code, pc).is_some();
        if scope.start_pc > scope.end_pc
            || !end_or_boundary(scope.start_pc)
            || !end_or_boundary(scope.end_pc)
        {
            return Err(error(scope.start_pc, "invalid scope range"));
        }
        if let Some(parent) = scope.parent {
            if usize::from(parent) >= i {
                return Err(error(scope.start_pc, "parent scope must precede its child"));
            }
            let parent = &func.scopes[usize::from(parent)];
            if parent.start_pc > scope.start_pc || parent.end_pc < scope.end_pc {
                return Err(error(scope.start_pc, "scope lies outside its parent"));
            }
        }
    }
    let debug = &func.debug;
    if debug.source_start > debug.source_end {
        return Err(error(0, "invalid source range"));
    }
    let mut last = None;
    for point in &debug.pc_map {
        if last.is_some_and(|prev| prev >= point.pc)
            || boundary(code, point.pc).is_none()
            || !(debug.source_start..=debug.source_end).contains(&point.byte_offset)
        {
            return Err(error(point.pc, "invalid source map entry"));
        }
        last = Some(point.pc);
    }
    for constant in &func.constants {
        match constant {
            ConstEntry::TemplateObject(template) if template.cooked.len() != template.raw.len() => {
                return Err(error(0, "template cooked and raw arrays differ in length"));
            }
            ConstEntry::BigInt(decimal) => {
                let digits = decimal.strip_prefix('-').unwrap_or(decimal);
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(error(0, "invalid decimal BigInt constant"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_scope_links(locals: &[VarDef]) -> Result<(), VerifyError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Unseen,
        Active,
        Complete,
    }
    let mut marks = Vec::new();
    marks
        .try_reserve_exact(locals.len())
        .map_err(|_| error(0, "cannot allocate verifier state"))?;
    marks.resize(locals.len(), Mark::Unseen);
    let next = |i: usize| match locals[i].scope_next {
        ScopeLink::Local(index) => Some(usize::from(index)),
        _ => None,
    };
    for i in 0..locals.len() {
        let mut cursor = Some(i);
        while let Some(slot) = cursor {
            match marks[slot] {
                Mark::Complete => break,
                Mark::Active => return Err(error(0, "cyclic lexical scope chain")),
                Mark::Unseen => marks[slot] = Mark::Active,
            }
            cursor = next(slot);
        }
        cursor = Some(i);
        while let Some(slot) = cursor {
            if marks[slot] != Mark::Active {
                break;
            }
            marks[slot] = Mark::Complete;
            cursor = next(slot);
        }
    }
    Ok(())
}

fn attach_handlers(
    func: &FunctionBytecode,
    code: &mut [Decoded],
    len: u32,
) -> Result<(), VerifyError> {
    let mut ranges = Vec::new();
    for handler in &func.exception_handlers {
        let invalid = || {
            error(
                handler.start_pc,
                "invalid exception handler range or target",
            )
        };
        let start = boundary(code, handler.start_pc).ok_or_else(invalid)?;
        let end = if handler.end_pc == len {
            code.len()
        } else {
            boundary(code, handler.end_pc).ok_or_else(invalid)?
        };
        let target = boundary(code, handler.handler_pc).ok_or_else(invalid)?;
        if start >= end || (start..end).contains(&target) {
            return Err(invalid());
        }
        push(
            &mut ranges,
            (
                start,
                end,
                HandlerEdge {
                    target,
                    depth: handler.stack_depth,
                },
            ),
        )?;
    }
    ranges.sort_unstable_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
    let mut active: Vec<(usize, usize, HandlerEdge)> = Vec::new();
    let mut ranges = ranges.into_iter().peekable();
    for (i, insn) in code.iter_mut().enumerate() {
        while active.last().is_some_and(|&(_, end, _)| i >= end) {
            active.pop();
        }
        while ranges.peek().is_some_and(|&(start, _, _)| start == i) {
            let range = ranges.next().expect("peeked range");
            if active
                .last()
                .is_some_and(|&(start, end, _)| range.1 > end || (range.0, range.1) == (start, end))
            {
                return Err(error(insn.pc, "handler ranges must be disjoint or nested"));
            }
            push(&mut active, range)?;
        }
        insn.handler = active.last().map(|&(_, _, edge)| edge);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct State {
    height: u32,
    top_marker: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Catch,
    Iterator,
    Return(usize),
}

#[derive(Debug, Clone, Copy)]
struct Marker {
    slot: u32,
    kind: MarkerKind,
    previous: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Visit {
    entry: Option<State>,
    return_marker: Option<usize>,
    first_caller: Option<usize>,
    next_caller: Option<usize>,
    returns: bool,
}

struct Analysis<'a> {
    code: &'a [Decoded],
    visits: Vec<Visit>,
    pending: Vec<usize>,
    markers: Vec<Marker>,
    peak: u32,
    limit: u32,
}

impl Analysis<'_> {
    fn enqueue(&mut self, from: u32, to: usize, state: State) -> Result<(), VerifyError> {
        let insn = self
            .code
            .get(to)
            .ok_or_else(|| error(from, "execution falls off the end of bytecode"))?;
        self.check_limit(insn.pc, state.height)?;
        match self.visits[to].entry {
            Some(entry) if entry.height != state.height => Err(error(
                insn.pc,
                format!(
                    "inconsistent stack heights at join ({} and {})",
                    entry.height, state.height
                ),
            )),
            Some(entry) if entry.top_marker != state.top_marker => {
                Err(error(insn.pc, "inconsistent control stack at join"))
            }
            Some(_) => Ok(()),
            None => {
                self.visits[to].entry = Some(state);
                push(&mut self.pending, to)
            }
        }
    }

    fn check_limit(&mut self, pc: u32, height: u32) -> Result<(), VerifyError> {
        if height > self.limit {
            return Err(error(
                pc,
                format!(
                    "stack overflow: needs {height} slots, limit is {}",
                    self.limit
                ),
            ));
        }
        self.peak = self.peak.max(height);
        Ok(())
    }

    fn add_marker(&mut self, state: State, kind: MarkerKind) -> Result<State, VerifyError> {
        let index = self.markers.len();
        push(
            &mut self.markers,
            Marker {
                slot: state.height,
                kind,
                previous: state.top_marker,
            },
        )?;
        Ok(State {
            height: state.height + 1,
            top_marker: Some(index),
        })
    }

    fn truncate(&self, mut state: State, height: u32) -> State {
        while let Some(marker) = state.top_marker.map(|idx| self.markers[idx]) {
            if marker.slot < height {
                break;
            }
            state.top_marker = marker.previous;
        }
        state.height = height;
        state
    }

    fn top(&self, state: State) -> Option<Marker> {
        state.top_marker.map(|idx| self.markers[idx])
    }

    fn plain_slots(&self, pc: u32, state: State, depth: u32) -> Result<(), VerifyError> {
        if self
            .top(state)
            .is_some_and(|marker| marker.slot >= state.height - depth)
        {
            return Err(error(
                pc,
                "instruction uses a control marker as an ordinary operand",
            ));
        }
        Ok(())
    }

    fn gosub(&mut self, pc: u32, from: usize, to: usize, state: State) -> Result<(), VerifyError> {
        let entry = if let Some(id) = self.visits[to].return_marker {
            let marker = self.markers[id];
            if marker.slot != state.height || marker.previous != state.top_marker {
                return Err(error(pc, "inconsistent stack at finally entry"));
            }
            State {
                height: state.height + 1,
                top_marker: Some(id),
            }
        } else {
            let entry = self.add_marker(state, MarkerKind::Return(to))?;
            self.visits[to].return_marker = entry.top_marker;
            entry
        };
        self.enqueue(pc, to, entry)?;
        self.visits[from].next_caller = self.visits[to].first_caller;
        self.visits[to].first_caller = Some(from);
        if self.visits[to].returns {
            self.enqueue(pc, from + 1, state)?;
        }
        Ok(())
    }

    fn ret(&mut self, pc: u32, state: State) -> Result<(), VerifyError> {
        match self.top(state) {
            Some(Marker {
                kind: MarkerKind::Return(entry),
                slot,
                previous,
            }) if slot + 1 == state.height => {
                if !self.visits[entry].returns {
                    self.visits[entry].returns = true;
                    let result = State {
                        height: slot,
                        top_marker: previous,
                    };
                    let mut caller = self.visits[entry].first_caller;
                    while let Some(from) = caller {
                        self.enqueue(pc, from + 1, result)?;
                        caller = self.visits[from].next_caller;
                    }
                }
                Ok(())
            }
            _ => Err(error(pc, "ret has no finally return address")),
        }
    }

    fn step(&mut self, i: usize, state: State) -> Result<(), VerifyError> {
        use Opcode::*;
        let Decoded {
            pc,
            instruction,
            target,
            handler,
        } = self.code[i];
        let info = instruction.opcode.info();
        let (op, operands) = (instruction.opcode, instruction.operands);
        let immediate = match operands {
            Operands::Unsigned(value) => value,
            _ => 0,
        };
        let extra = match operands {
            Operands::Call(args)
            | Operands::Eval {
                arguments: args, ..
            } => u32::from(args),
            _ => 0,
        };
        let pop = u32::from(info.n_pop) + extra;
        let needed = match op {
            // These opcodes can read more than the fixed stack signature.
            ForOfNext => pop + immediate,
            IteratorGetValueDone => 4,
            CopyDataProperties => pop.max(
                1 + (immediate & 3)
                    .max((immediate >> 2) & 7)
                    .max(immediate >> 5),
            ),
            _ => pop,
        };
        if state.height < needed {
            return Err(error(
                pc,
                format!(
                    "stack underflow: {} needs {needed} slots, has {}",
                    info.name, state.height
                ),
            ));
        }
        if let Some(handler) = handler {
            let depth = u32::from(handler.depth);
            if depth > state.height {
                return Err(error(
                    pc,
                    "exception handler preserves more slots than are live",
                ));
            }
            let exception = State {
                height: depth + 1,
                ..self.truncate(state, depth)
            };
            self.enqueue(pc, handler.target, exception)?;
        }
        let height = state.height - pop + u32::from(info.n_push);
        let after = match op {
            Goto | Goto8 | Goto16 => {
                return self.enqueue(pc, target.expect("branch target"), state);
            }
            Gosub => return self.gosub(pc, i, target.expect("gosub target"), state),
            Ret => return self.ret(pc, state),
            Catch => {
                self.enqueue(pc, target.expect("catch target"), State { height, ..state })?;
                self.add_marker(state, MarkerKind::Catch)?
            }
            ForOfStart | ForAwaitOfStart => {
                self.plain_slots(pc, state, pop)?;
                self.add_marker(
                    State {
                        height: height - 1,
                        ..state
                    },
                    MarkerKind::Iterator,
                )?
            }
            NipCatch => {
                self.plain_slots(pc, state, 1)?;
                let mut marker = self.top(state);
                while marker.is_some_and(|marker| matches!(marker.kind, MarkerKind::Return(_))) {
                    marker = marker
                        .and_then(|marker| marker.previous)
                        .map(|id| self.markers[id]);
                }
                let marker =
                    marker.ok_or_else(|| error(pc, "nip_catch has no active catch marker"))?;
                State {
                    height: marker.slot + 1,
                    top_marker: marker.previous,
                }
            }
            Drop => self.truncate(state, height),
            Nip | Nip1 => {
                self.plain_slots(pc, state, pop - 1)?;
                State {
                    height,
                    ..self.truncate(state, state.height - pop)
                }
            }
            IteratorClose => {
                let rest = self.truncate(state, state.height - 1);
                self.plain_slots(pc, rest, 2)?;
                if self.top(state).is_some_and(|marker| {
                    marker.slot + 1 == state.height && marker.kind != MarkerKind::Iterator
                }) {
                    return Err(error(pc, "iterator_close expects an iterator marker"));
                }
                self.truncate(rest, height)
            }
            IteratorNext | IteratorCall | IteratorGetValueDone => {
                // The layout is iterator, next method, guard, result. Some
                // outcomes replace the iterator as well as the result.
                self.plain_slots(pc, self.truncate(state, state.height - 2), 2)?;
                self.plain_slots(pc, state, 1)?;
                State { height, ..state }
            }
            Dup3 if self.top(state).is_some_and(|marker| {
                marker.kind == MarkerKind::Iterator && marker.slot + 1 == state.height
            }) =>
            {
                // for-await duplicates the iterator triple and drops the copied marker
                // before calling `next`. The original still guards the iterator.
                self.plain_slots(pc, self.truncate(state, state.height - 1), 2)?;
                self.add_marker(
                    State {
                        height: height - 1,
                        ..state
                    },
                    MarkerKind::Iterator,
                )?
            }
            ForOfNext => {
                // Reads the iterator object and next method without removing the guard.
                let before_guard = self.truncate(state, state.height - immediate - 1);
                self.plain_slots(pc, before_guard, 2)?;
                State { height, ..state }
            }
            WithGetVar | WithPutVar | WithDeleteVar | WithMakeRef | WithGetRef
            | WithGetRefUndef => {
                self.plain_slots(pc, state, pop)?;
                let taken = match op {
                    WithPutVar => height - 1,
                    WithGetVar | WithDeleteVar => height + 1,
                    _ => height + 2,
                };
                self.enqueue(
                    pc,
                    target.expect("with target"),
                    State {
                        height: taken,
                        ..state
                    },
                )?;
                State { height, ..state }
            }
            _ => {
                self.plain_slots(pc, state, needed)?;
                State { height, ..state }
            }
        };
        self.check_limit(pc, after.height)?;
        match op {
            TailCall | TailCallMethod | Return | ReturnUndef | ReturnAsync | Throw | ThrowError => {
                return Ok(());
            }
            IfFalse | IfTrue | IfFalse8 | IfTrue8 => {
                self.enqueue(pc, target.expect("conditional target"), after)?
            }
            _ => {}
        }
        self.enqueue(pc, i + 1, after)
    }
}

fn stack_size(func: &FunctionBytecode, limit: u16) -> Result<u16, VerifyError> {
    let code = decode(func)?;
    let mut visits = Vec::new();
    visits
        .try_reserve_exact(code.len())
        .map_err(|_| error(0, "cannot allocate verifier state"))?;
    visits.resize(code.len(), Visit::default());
    let mut analysis = Analysis {
        code: &code,
        visits,
        pending: Vec::new(),
        markers: Vec::new(),
        peak: 0,
        limit: u32::from(limit),
    };
    analysis.enqueue(0, 0, State::default())?;
    while let Some(i) = analysis.pending.pop() {
        let state = analysis.visits[i].entry.expect("queued entry state");
        analysis.step(i, state)?;
    }
    Ok(analysis.peak as u16)
}
