use crate::{
    CompileError,
    emit::{Emitter, Result, Slot},
};
use qjs_bytecode::*;

fn resolved(opcode: Opcode, operands: Operands) -> IrOp {
    IrOp::Resolved(Instruction { opcode, operands })
}
fn plain(opcode: Opcode) -> IrOp {
    resolved(opcode, Operands::None)
}

#[derive(Clone, Copy)]
enum Access {
    Read,
    ReadUndef,
    Write,
    Init,
    Delete,
    MakeRef,
    Call,
}

impl Emitter<'_> {
    pub(crate) fn local_binding(&self, name: ScopedName) -> bool {
        let mut function = self.current;
        let mut scope = name.scope;
        loop {
            let draft = &self.functions[function];
            let block = &draft.scopes[usize::from(scope)];
            if let Some(slot) = block.names.get(&name.name) {
                return !matches!(slot, Slot::Global(_));
            }
            if block.with_object.is_some() {
                return false;
            }
            if let Some(parent) = block.info.parent {
                scope = parent;
            } else if let Some((parent, enclosing)) = draft.parent {
                function = parent;
                scope = enclosing;
            } else {
                return false;
            }
        }
    }

    pub(crate) fn capture_frame_slot(&mut self, function: usize, slot: Slot) -> Result {
        let f = &mut self.functions[function].bytecode;
        let index = match slot {
            Slot::Arg(n) => usize::from(n),
            Slot::Local(n) => usize::from(f.arg_count) + usize::from(n),
            _ => return Ok(()),
        };
        let var = &mut f.vars[index];
        if var.var_ref_index.is_none() {
            let index = f.var_ref_count;
            f.var_ref_count = index.checked_add(1).ok_or(CompileError::OutOfMemory)?;
            var.captured = true;
            var.var_ref_index = Some(index);
        }
        Ok(())
    }

    fn carry(&mut self, mut from: usize, to: usize, mut slot: Slot) -> Result<Slot> {
        if let Slot::Global(_) = slot {
            return Ok(slot);
        }
        let mut chain = Vec::new();
        let mut n = to;
        while n != from {
            chain.push(n);
            n = self.functions[n]
                .parent
                .ok_or_else(|| CompileError::Bytecode("invalid enclosing function".into()))?
                .0;
        }
        for child in chain.into_iter().rev() {
            let parent = &self.functions[from];
            let (key, kind, name, var_kind, is_const, lexical_tdz) = match slot {
                Slot::Arg(n) | Slot::Local(n) => {
                    let (key, kind, v) = if matches!(slot, Slot::Arg(_)) {
                        (
                            (0, n),
                            ClosureKind::Argument,
                            &parent.bytecode.vars[usize::from(n)],
                        )
                    } else {
                        ((1, n), ClosureKind::Local, parent.local(n))
                    };
                    (key, kind, v.name, v.kind, v.is_const, v.lexical_tdz)
                }
                Slot::Capture(n) => {
                    let v = &parent.bytecode.closure_vars[usize::from(n)];
                    (
                        (2, n),
                        ClosureKind::Ref,
                        v.name,
                        v.var_kind,
                        v.is_const,
                        v.lexical_tdz,
                    )
                }
                Slot::Global(_) => unreachable!(),
            };
            let index = if let Some(&index) = self.functions[child].capture_slots.get(&key) {
                index
            } else {
                self.charge(std::mem::size_of::<ClosureVarDef>() + 16)?;
                self.capture_frame_slot(from, slot)?;
                let child = &mut self.functions[child];
                let index = u16::try_from(child.bytecode.closure_vars.len())
                    .map_err(|_| CompileError::OutOfMemory)?;
                child.bytecode.closure_vars.push(ClosureVarDef {
                    name,
                    kind,
                    index: key.1,
                    var_kind,
                    is_const,
                    lexical_tdz,
                });
                child.capture_slots.insert(key, index);
                index
            };
            from = child;
            slot = Slot::Capture(index);
        }
        Ok(slot)
    }

    fn lookup(&mut self, name: ScopedName, dynamic: bool) -> Result<(Slot, Vec<(Slot, bool)>)> {
        let mut function = self.current;
        let mut scope = name.scope;
        let mut with = Vec::new();
        loop {
            let block = &self.functions[function].scopes[usize::from(scope)];
            if let Some(&slot) = block.names.get(&name.name) {
                return Ok((self.carry(function, self.current, slot)?, with));
            }
            let parent = block.info.parent;
            let objects = [(block.with_object, true), (block.eval_object, false)];
            if dynamic {
                for (slot, is_with) in objects {
                    if let Some(slot) = slot {
                        with.push((
                            self.carry(function, self.current, Slot::Local(slot))?,
                            is_with,
                        ));
                    }
                }
            }
            if let Some(parent) = parent {
                scope = parent;
            } else if let Some((parent, enclosing)) = self.functions[function].parent {
                function = parent;
                scope = enclosing;
            } else {
                return Ok((Slot::Global(name.name), with));
            }
        }
    }

    fn read_slot(&self, slot: Slot, undef: bool) -> IrOp {
        use Opcode::*;
        match slot {
            Slot::Arg(n) => resolved(GetArg, Operands::Argument(n)),
            Slot::Local(n) => resolved(
                if self.f().local(n).lexical_tdz {
                    GetLocCheck
                } else {
                    GetLoc
                },
                Operands::Local(n),
            ),
            Slot::Capture(n) => resolved(
                if self.f().bytecode.closure_vars[usize::from(n)].lexical_tdz {
                    GetVarRefCheck
                } else {
                    GetVarRef
                },
                Operands::VarRef(n),
            ),
            Slot::Global(name) => resolved(
                if undef { GetVarUndef } else { GetVar },
                Operands::Atom(name),
            ),
        }
    }

    fn write_slot(&self, slot: Slot, name: UnitAtom, init: bool, out: &mut Vec<IrOp>) {
        use Opcode::*;
        let (is_const, kind, tdz) = match slot {
            Slot::Local(n) => {
                let v = self.f().local(n);
                (v.is_const, v.kind, v.lexical_tdz)
            }
            Slot::Capture(n) => {
                let v = &self.f().bytecode.closure_vars[usize::from(n)];
                (v.is_const, v.var_kind, v.lexical_tdz)
            }
            _ => (false, VarKind::Var, false),
        };
        if is_const && !init {
            if tdz {
                out.extend([self.read_slot(slot, false), plain(Drop)]);
            }
            if kind != VarKind::FunctionName || self.f().bytecode.strict {
                out.push(resolved(ThrowError, Operands::AtomU8(name, 0)));
            } else {
                out.push(plain(Drop));
            }
            return;
        }
        out.push(match slot {
            Slot::Arg(n) => resolved(PutArg, Operands::Argument(n)),
            Slot::Local(n) => resolved(
                match (tdz, init) {
                    (true, true) => PutLocCheckInit,
                    (true, false) => PutLocCheck,
                    _ => PutLoc,
                },
                Operands::Local(n),
            ),
            Slot::Capture(n) => resolved(
                match (tdz, init) {
                    (true, true) => PutVarRefCheckInit,
                    (true, false) => PutVarRefCheck,
                    _ => PutVarRef,
                },
                Operands::VarRef(n),
            ),
            Slot::Global(name) => {
                resolved(if init { PutVarInit } else { PutVar }, Operands::Atom(name))
            }
        });
    }

    fn name_op(&mut self, name: ScopedName, action: Access, out: &mut Vec<IrOp>) -> Result {
        use Opcode::*;
        let internal = name
            .name
            .builtin()
            .is_some_and(|a| a.as_str().starts_with(['<', '#']))
            || name
                .name
                .dynamic_index()
                .is_some_and(|i| match &self.atoms[i as usize] {
                    ConstString::Latin1(bytes) => matches!(bytes.first(), Some(b'<' | b'#')),
                    ConstString::Utf16(units) => matches!(units.first(), Some(60 | 35)),
                });
        let dynamic = !matches!(action, Access::Init) && !internal;
        let (slot, with) = self.lookup(name, dynamic)?;
        let done = (!with.is_empty()).then(|| self.label());
        for (object, is_with) in with {
            let opcode = match action {
                Access::Read | Access::ReadUndef => WithGetVar,
                Access::Write => WithPutVar,
                Access::Delete => WithDeleteVar,
                Access::MakeRef => WithMakeRef,
                Access::Call => {
                    if is_with {
                        WithGetRef
                    } else {
                        WithGetRefUndef
                    }
                }
                Access::Init => unreachable!(),
            };
            out.push(self.read_slot(object, false));
            out.push(resolved(
                opcode,
                Operands::AtomJumpU8(
                    name.name,
                    done.expect("with label") as i32,
                    u8::from(is_with),
                ),
            ));
        }
        match action {
            Access::Read | Access::ReadUndef => {
                out.push(self.read_slot(slot, matches!(action, Access::ReadUndef)))
            }
            Access::Call => out.extend([plain(Undefined), self.read_slot(slot, false)]),
            Access::Write | Access::Init => {
                self.write_slot(slot, name.name, matches!(action, Access::Init), out)
            }
            Access::Delete => out.push(if let Slot::Global(a) = slot {
                resolved(DeleteVar, Operands::Atom(a))
            } else {
                plain(PushFalse)
            }),
            Access::MakeRef => {
                self.capture_frame_slot(self.current, slot)?;
                out.push(match slot {
                    Slot::Local(n) => resolved(MakeLocRef, Operands::AtomU16(name.name, n)),
                    Slot::Arg(n) => resolved(MakeArgRef, Operands::AtomU16(name.name, n)),
                    Slot::Capture(n) => resolved(MakeVarRefRef, Operands::AtomU16(name.name, n)),
                    Slot::Global(a) => resolved(MakeVarRef, Operands::Atom(a)),
                });
            }
        }
        if matches!(action, Access::MakeRef) {
            out.push(plain(PushFalse));
            if let Some(label) = done {
                let end = self.label();
                out.extend([
                    IrOp::Jump(end),
                    IrOp::Label(label),
                    plain(PushTrue),
                    IrOp::Label(end),
                ]);
            }
        } else if let Some(label) = done {
            out.push(IrOp::Label(label));
        }
        Ok(())
    }

    fn link_scopes(&mut self) -> Result<Vec<u16>> {
        let f = self.fm();
        let mut heads = Vec::with_capacity(f.scopes.len());
        let args = usize::from(f.bytecode.arg_count);
        for scope in &f.scopes {
            let mut next = if let Some(parent) = scope.info.parent {
                match heads[usize::from(parent)] {
                    0 => ScopeLink::End,
                    u16::MAX => ScopeLink::ParameterEnd,
                    n => ScopeLink::Local(n - 1),
                }
            } else {
                ScopeLink::End
            };
            let mut non_lexical: Vec<_> = scope
                .names
                .values()
                .filter_map(|&slot| match slot {
                    Slot::Local(slot) if !f.bytecode.vars[args + usize::from(slot)].lexical_tdz => {
                        Some(slot)
                    }
                    _ => None,
                })
                .collect();
            non_lexical.sort_unstable();
            for &slot in scope
                .eval_object
                .iter()
                .chain(&scope.lexical)
                .chain(&non_lexical)
            {
                f.bytecode.vars[args + usize::from(slot)].scope_next = next;
                next = ScopeLink::Local(slot);
            }
            heads.push(match next {
                ScopeLink::End => 0,
                ScopeLink::ParameterEnd => u16::MAX,
                ScopeLink::Local(n) if n < u16::MAX - 1 => n + 1,
                ScopeLink::Local(_) => return Err(CompileError::OutOfMemory),
            });
        }
        Ok(heads)
    }

    fn eval_captures(&mut self) -> Result {
        for n in 0..self.f().bytecode.var_count {
            self.capture_frame_slot(self.current, Slot::Local(n))?;
        }
        for n in 0..self.f().bytecode.arg_count {
            self.capture_frame_slot(self.current, Slot::Arg(n))?;
        }
        let mut function = self.current;
        while let Some((parent, enclosing)) = self.functions[function].parent {
            function = parent;
            let mut scope = enclosing;
            loop {
                let block = &self.functions[function].scopes[usize::from(scope)];
                let mut bindings: Vec<_> = block
                    .names
                    .iter()
                    .map(|(&name, &slot)| (name, slot))
                    .collect();
                bindings.sort_unstable_by_key(|&(name, _)| name.raw());
                let outer = block.info.parent;
                let objects = [block.with_object, block.eval_object];
                for (_, slot) in bindings {
                    self.carry(function, self.current, slot)?;
                }
                for slot in objects.into_iter().flatten() {
                    self.carry(function, self.current, Slot::Local(slot))?;
                }
                if let Some(outer) = outer {
                    scope = outer;
                } else {
                    break;
                }
            }
        }
        Ok(())
    }

    fn prepare_eval_objects(&mut self) -> Result {
        for index in 0..self.functions.len() {
            self.current = index;
            let f = self.f();
            if f.bytecode.strict
                || (f.parent.is_none()
                    && !matches!(
                        self.options.goal,
                        qjs_syntax::ParseGoal::FunctionBody { .. }
                    ))
                || !Self::has_direct_eval(f)
            {
                continue;
            }
            let params = f
                .scopes
                .iter()
                .position(|scope| scope.parameter_end)
                .map(|i| i as u16);
            let body = f.vars_scope;
            for (scope, name) in [
                (Some(body), BuiltinAtom::InternalVar),
                (params, BuiltinAtom::InternalArgVar),
            ] {
                if let Some(scope) = scope {
                    let slot = self.local(name.id(), VarKind::Var, scope, false)?;
                    self.fm().scopes[usize::from(scope)].eval_object = Some(slot);
                    self.fm().prologue.extend([
                        resolved(Opcode::SpecialObject, Operands::Unsigned(5)),
                        resolved(Opcode::PutLoc, Operands::Local(slot)),
                    ]);
                }
            }
        }
        Ok(())
    }

    fn has_direct_eval(f: &crate::emit::Draft) -> bool {
        f.ir.iter()
            .any(|op| matches!(op, IrOp::DirectEval { .. } | IrOp::ApplyEval { .. }))
    }

    pub(crate) fn resolve(&mut self) -> Result {
        use Opcode::*;
        self.charge(0)?;
        self.prepare_eval_objects()?;
        // Fill each eval's captures inside out before ordinary lookup can add any of them.
        for index in 0..self.functions.len() {
            self.current = index;
            if Self::has_direct_eval(self.f()) {
                self.eval_captures()?;
            }
        }
        for index in (0..self.functions.len()).rev() {
            self.current = index;
            let heads = if Self::has_direct_eval(self.f()) {
                self.link_scopes()?
            } else {
                Vec::new()
            };
            let original = std::mem::take(&mut self.fm().ir);
            let globals = std::mem::take(&mut self.fm().globals);
            let flags = |kind, function| {
                (if kind == VarKind::Var { 0 } else { 128 }) | (if function { 64 } else { 0 })
            };
            let mut prologue = std::mem::take(&mut self.fm().prologue);
            for &(atom, kind, function) in &globals {
                prologue.push(resolved(
                    CheckDefineVar,
                    Operands::AtomU8(atom, flags(kind, function.is_some())),
                ));
            }
            for (atom, kind, function) in globals {
                let attributes = flags(kind, function.is_some())
                    | if matches!(kind, VarKind::Const | VarKind::Using | VarKind::AwaitUsing) {
                        0
                    } else {
                        2
                    };
                if let Some(constant) = function {
                    prologue.extend([
                        resolved(Fclosure, Operands::Constant(constant)),
                        resolved(DefineFunc, Operands::AtomU8(atom, attributes)),
                    ]);
                } else {
                    prologue.push(resolved(DefineVar, Operands::AtomU8(atom, attributes)));
                }
            }
            self.fm().prologue = prologue;
            let mut out = Vec::new();
            for op in original {
                match op {
                    IrOp::ScopeGetVar(n) => self.name_op(n, Access::Read, &mut out)?,
                    IrOp::ScopeGetVarUndef(n) => self.name_op(n, Access::ReadUndef, &mut out)?,
                    IrOp::ScopePutVar(n) => self.name_op(n, Access::Write, &mut out)?,
                    IrOp::ScopePutVarInit(n) => self.name_op(n, Access::Init, &mut out)?,
                    IrOp::ScopeDeleteVar(n) => self.name_op(n, Access::Delete, &mut out)?,
                    IrOp::ScopeMakeRef { binding, .. } => {
                        self.name_op(binding, Access::MakeRef, &mut out)?
                    }
                    IrOp::ScopeGetRef(n) => self.name_op(n, Access::Call, &mut out)?,
                    IrOp::DirectEval { arguments, scope } => {
                        out.push(resolved(
                            Eval,
                            Operands::Eval {
                                arguments,
                                scope: heads[usize::from(scope)],
                            },
                        ));
                    }
                    IrOp::ApplyEval { scope } => {
                        out.push(resolved(
                            ApplyEval,
                            Operands::Unsigned(u32::from(heads[usize::from(scope)])),
                        ));
                    }
                    IrOp::EnterScope(scope) => {
                        out.push(op);
                        for &slot in &self.f().scopes[usize::from(scope)].lexical {
                            out.push(resolved(SetLocUninitialized, Operands::Local(slot)));
                        }
                    }
                    IrOp::DisposeScope(scope) => {
                        let mut resources: Vec<_> = self
                            .f()
                            .disposed
                            .iter()
                            .filter_map(|(&(s, _), &v)| (s == scope).then_some(v))
                            .collect();
                        resources.sort_unstable_by_key(|&(slot, _)| std::cmp::Reverse(slot));
                        for (slot, asynchronous) in resources {
                            if asynchronous {
                                let error = self.label();
                                let done = self.label();
                                out.extend([
                                    resolved(GetLoc, Operands::Local(slot)),
                                    plain(UsingDisposeInit),
                                    plain(StrictEq),
                                    IrOp::JumpIfTrue(done),
                                    IrOp::Catch(error),
                                    resolved(UsingDisposeAsync, Operands::Local(slot)),
                                    plain(Await),
                                    plain(Drop),
                                    plain(Drop),
                                    IrOp::Jump(done),
                                    IrOp::Label(error),
                                    plain(UsingDisposeMerge),
                                    IrOp::Label(done),
                                ]);
                            } else {
                                out.push(resolved(UsingDispose, Operands::Local(slot)));
                            }
                        }
                    }
                    IrOp::LeaveScope(scope) => {
                        for &slot in &self.f().scopes[usize::from(scope)].lexical {
                            out.push(resolved(CloseLoc, Operands::Local(slot)));
                        }
                        out.push(op);
                    }
                    IrOp::GetFieldOptionalChain(name) => {
                        out.push(resolved(GetField, Operands::Atom(name)))
                    }
                    IrOp::GetArrayElementOptionalChain => out.push(plain(GetArrayEl)),
                    _ => out.push(op),
                }
            }
            let f = self.fm();
            f.ir = out
                .into_iter()
                .flat_map(|op| {
                    match op {
                        IrOp::Resolved(Instruction {
                            opcode: SetLocUninitialized,
                            operands: Operands::Local(n),
                        }) if f.local(n).captured => {
                            // A throw can skip the scope exit. Detach the old cell before resetting its slot.
                            [Some(resolved(CloseLoc, Operands::Local(n))), Some(op)]
                        }
                        IrOp::Resolved(Instruction {
                            opcode: CloseLoc,
                            operands: Operands::Local(n),
                        }) if !f.local(n).captured => [None, None],
                        _ => [Some(op), None],
                    }
                })
                .flatten()
                .collect();
        }
        Ok(())
    }
}
