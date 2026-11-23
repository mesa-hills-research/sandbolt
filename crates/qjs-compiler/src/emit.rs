use crate::CompileError;
use qjs_bytecode::*;
use qjs_syntax::*;
use std::collections::HashMap;
use std::mem::size_of;

pub(crate) type Result<T = ()> = std::result::Result<T, CompileError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Arg(u16),
    Local(u16),
    Capture(u16),
    Global(UnitAtom),
}

#[derive(Default)]
pub(crate) struct Scope {
    pub(crate) info: ScopeInfo,
    pub(crate) names: HashMap<UnitAtom, Slot>,
    pub(crate) lexical: Vec<u16>,
    pub(crate) with_object: Option<u16>,
    pub(crate) eval_object: Option<u16>,
    pub(crate) parameter_end: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Cleanup {
    Drop,
    Catch,
    CloseIterator(Span),
    AsyncIterator(Span),
    Finally(u32),
    Dispose(u32, u16),
    Scope(u16),
}

pub(crate) struct Control {
    pub(crate) name: Option<UnitAtom>,
    pub(crate) exit: u32,
    pub(crate) next: Option<u32>,
    pub(crate) exit_depth: usize,
    pub(crate) next_depth: usize,
    pub(crate) anonymous: bool,
}

#[derive(Default)]
pub(crate) struct Draft {
    pub(crate) bytecode: FunctionBytecode,
    pub(crate) parent: Option<(usize, u16)>,
    pub(crate) ir: Vec<IrOp>,
    pub(crate) prologue: Vec<IrOp>,
    pub(crate) globals: Vec<(UnitAtom, VarKind, Option<u32>)>,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) scope: u16,
    pub(crate) vars_scope: u16,
    pub(crate) labels: u32,
    pub(crate) capture_slots: HashMap<(u8, u16), u16>,
    pub(crate) control: Vec<Control>,
    pub(crate) cleanup: Vec<Cleanup>,
    pub(crate) completion: Option<u16>,
    pub(crate) module: Option<ModuleRecord>,
    pub(crate) module_await: bool,
    pub(crate) disposed: HashMap<(u16, UnitAtom), (u16, bool)>,
    pub(crate) private_members: HashMap<(u16, UnitAtom), crate::class::PrivateMember>,
    pub(crate) annex_b: bool,
    pub(crate) annex_mirrors: HashMap<(u16, UnitAtom), u16>,
}

impl Draft {
    pub(crate) fn scope(&self) -> &Scope {
        &self.scopes[usize::from(self.scope)]
    }
    pub(crate) fn local(&self, slot: u16) -> &VarDef {
        &self.bytecode.vars[usize::from(self.bytecode.arg_count) + usize::from(slot)]
    }
}

pub(crate) struct Emitter<'a> {
    pub(crate) source: &'a SourceText,
    pub(crate) program: &'a ParsedProgram,
    pub(crate) options: ParseOptions<'a>,
    pub(crate) atoms: Vec<ConstString>,
    atom_ids: HashMap<ConstString, UnitAtom>,
    pub(crate) functions: Vec<Draft>,
    pub(crate) current: usize,
    depth: usize,
    remaining: usize,
    oom: bool,
}

pub(crate) fn compile(
    source: &SourceText,
    program: &ParsedProgram,
    options: ParseOptions,
) -> Result<ProgramImage> {
    let mut emitter = Emitter {
        source,
        program,
        options,
        atoms: Vec::new(),
        atom_ids: HashMap::new(),
        functions: Vec::new(),
        current: 0,
        depth: 0,
        remaining: options.max_bytes,
        oom: false,
    };
    emitter.charge(source.as_bytes().len())?;
    for (count, size) in [
        (program.arena.expressions().len(), size_of::<Node<Expr>>()),
        (program.arena.statements().len(), size_of::<Node<Stmt>>()),
        (program.arena.patterns().len(), size_of::<Node<Pattern>>()),
    ] {
        emitter.charge(count.saturating_mul(size))?;
    }
    emitter.entry()?;
    emitter.resolve()?;
    let mut image = ProgramImage {
        atoms: std::mem::take(&mut emitter.atoms),
        ..ProgramImage::default()
    };
    let atom_count = u32::try_from(image.atoms.len()).map_err(|_| CompileError::OutOfMemory)?;
    for mut draft in std::mem::take(&mut emitter.functions) {
        draft.bytecode.atom_count = atom_count;
        if let Some(mut module) = draft.module.take() {
            module.has_top_level_await = draft.module_await;
            image.modules.push(module);
        }
        emitter.charge(draft.ir.len().saturating_mul(40))?;
        image.functions.push(crate::assemble::assemble(draft)?);
    }
    qjs_bytecode::verify_image(&image)
        .map_err(|error| CompileError::Bytecode(error.to_string()))?;
    Ok(image)
}

pub(crate) fn string(value: &JsStrLit) -> ConstString {
    match value {
        JsStrLit::Latin1(value) => ConstString::Latin1(value.clone()),
        JsStrLit::Utf16(value) => ConstString::Utf16(value.clone()),
    }
}

impl<'a> Emitter<'a> {
    pub(crate) fn f(&self) -> &Draft {
        &self.functions[self.current]
    }
    pub(crate) fn fm(&mut self) -> &mut Draft {
        &mut self.functions[self.current]
    }
    pub(crate) fn ast(&self) -> &'a Arena {
        &self.program.arena
    }
    pub(crate) fn kind(&self) -> FunctionKind {
        self.f().bytecode.kind.unwrap_or_default()
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> Result {
        match self.remaining.checked_sub(bytes) {
            Some(remaining) if !self.oom => {
                self.remaining = remaining;
                Ok(())
            }
            _ => {
                self.oom = true;
                Err(CompileError::OutOfMemory)
            }
        }
    }

    pub(crate) fn descent<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.oom {
            return Err(CompileError::OutOfMemory);
        }
        if self.depth >= self.options.max_depth {
            return Err(CompileError::StackOverflow);
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    pub(crate) fn error(&self, span: Span, message: &str) -> CompileError {
        let (line, col) = self.source.line_col(span.start);
        CompileError::Lowering {
            message: message.into(),
            line,
            col,
        }
    }

    pub(crate) fn atom(&mut self, text: &str) -> Result<UnitAtom> {
        self.atom_string(string(&JsStrLit::from_units(text.encode_utf16().collect())))
    }

    pub(crate) fn atom_string(&mut self, value: ConstString) -> Result<UnitAtom> {
        if let ConstString::Latin1(bytes) = &value
            && let Some(atom) = BuiltinAtom::ALL
                .iter()
                .find(|atom| atom.as_str().as_bytes() == bytes)
        {
            return Ok(atom.id());
        }
        if let Some(atom) = self.atom_ids.get(&value) {
            return Ok(*atom);
        }
        let index = u32::try_from(self.atoms.len()).map_err(|_| CompileError::OutOfMemory)?;
        let atom = UnitAtom::from_dynamic_index(index).ok_or(CompileError::OutOfMemory)?;
        self.charge(64 + 2 * string_bytes(&value))?;
        self.atom_ids
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.atoms
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.atom_ids.insert(value.clone(), atom);
        self.atoms.push(value);
        Ok(atom)
    }

    pub(crate) fn constant(&mut self, value: ConstEntry) -> Result<u32> {
        let bytes = match &value {
            ConstEntry::BigInt(s) => s.len(),
            ConstEntry::String(s) | ConstEntry::RegExp { pattern: s, .. } => string_bytes(s),
            ConstEntry::TemplateObject(t) => t
                .raw
                .iter()
                .chain(t.cooked.iter().flatten())
                .map(string_bytes)
                .sum(),
            _ => 0,
        };
        self.charge(size_of::<ConstEntry>() + bytes)?;
        let constants = &mut self.fm().bytecode.constants;
        let i = u32::try_from(constants.len()).map_err(|_| CompileError::OutOfMemory)?;
        constants
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        constants.push(value);
        Ok(i)
    }

    pub(crate) fn ir(&mut self, op: IrOp) {
        if self.charge(size_of::<IrOp>()).is_ok() {
            let ir = &mut self.fm().ir;
            if ir.try_reserve(1).is_ok() {
                ir.push(op);
            } else {
                self.oom = true;
            }
        }
    }
    pub(crate) fn op(&mut self, opcode: Opcode, operands: Operands) {
        self.ir(IrOp::Resolved(Instruction { opcode, operands }));
    }
    pub(crate) fn plain(&mut self, opcode: Opcode) {
        self.op(opcode, Operands::None);
    }
    pub(crate) fn int(&mut self, n: i32) {
        self.op(Opcode::PushI32, Operands::Signed(n));
    }
    pub(crate) fn unsigned(&mut self, opcode: Opcode, n: u32) {
        self.op(opcode, Operands::Unsigned(n));
    }
    pub(crate) fn named(&mut self, opcode: Opcode, atom: UnitAtom) {
        self.op(opcode, Operands::Atom(atom));
    }
    pub(crate) fn flag(&mut self, opcode: Opcode, atom: UnitAtom, flag: u8) {
        self.op(opcode, Operands::AtomU8(atom, flag));
    }
    pub(crate) fn get(&mut self, local: u16) {
        self.op(Opcode::GetLoc, Operands::Local(local));
    }
    pub(crate) fn put(&mut self, local: u16) {
        self.op(Opcode::PutLoc, Operands::Local(local));
    }
    pub(crate) fn label(&mut self) -> u32 {
        let id = self.f().labels;
        self.fm().labels += 1;
        id
    }
    pub(crate) fn mark(&mut self, label: u32) {
        self.ir(IrOp::Label(label));
    }
    pub(crate) fn jump(&mut self, label: u32) {
        self.ir(IrOp::Jump(label));
    }
    pub(crate) fn if_false(&mut self, label: u32) {
        self.ir(IrOp::JumpIfFalse(label));
    }
    pub(crate) fn if_true(&mut self, label: u32) {
        self.ir(IrOp::JumpIfTrue(label));
    }
    pub(crate) fn scoped(&self, name: UnitAtom) -> ScopedName {
        ScopedName {
            name,
            scope: self.f().scope,
        }
    }
    pub(crate) fn read(&mut self, name: UnitAtom) {
        self.ir(IrOp::ScopeGetVar(self.scoped(name)));
    }
    pub(crate) fn temp(&mut self) -> Result<u16> {
        self.local(UnitAtom::NONE, VarKind::Var, self.f().scope, false)
    }
    pub(crate) fn stash(&mut self) -> Result<u16> {
        let slot = self.temp()?;
        self.put(slot);
        Ok(slot)
    }
    pub(crate) fn source_loc(&mut self, span: Span) {
        self.ir(IrOp::SourceLoc(span.start));
    }

    pub(crate) fn start_scope(&mut self, parent: Option<u16>) -> Result<u16> {
        self.charge(size_of::<Scope>())?;
        let index = u16::try_from(self.f().scopes.len()).map_err(|_| CompileError::OutOfMemory)?;
        self.fm().scopes.push(Scope {
            info: ScopeInfo {
                parent,
                ..ScopeInfo::default()
            },
            ..Scope::default()
        });
        self.fm().scope = index;
        self.ir(IrOp::EnterScope(index));
        Ok(index)
    }
    pub(crate) fn end_scope(&mut self, index: u16) {
        self.ir(IrOp::LeaveScope(index));
        if let Some(parent) = self.f().scopes[usize::from(index)].info.parent {
            self.fm().scope = parent;
        }
    }

    pub(crate) fn local(
        &mut self,
        name: UnitAtom,
        kind: VarKind,
        scope: u16,
        lexical_tdz: bool,
    ) -> Result<u16> {
        self.charge(size_of::<VarDef>() + 16)?;
        let f = self.fm();
        let slot = f.bytecode.var_count;
        f.bytecode.var_count = slot.checked_add(1).ok_or(CompileError::OutOfMemory)?;
        let is_const = matches!(
            kind,
            VarKind::Const
                | VarKind::Using
                | VarKind::AwaitUsing
                | VarKind::FunctionName
                | VarKind::PrivateField
                | VarKind::PrivateMethod
                | VarKind::PrivateGetterSetter
        );
        f.bytecode.vars.push(VarDef {
            name,
            kind,
            scope_level: scope,
            is_const,
            lexical_tdz,
            ..VarDef::default()
        });
        if lexical_tdz {
            f.scopes[usize::from(scope)].lexical.push(slot);
        }
        Ok(slot)
    }

    pub(crate) fn bind(&mut self, name: UnitAtom, kind: VarKind, scope: u16) -> Result<Slot> {
        if let Some(slot) = self.f().scopes[usize::from(scope)].names.get(&name) {
            return Ok(*slot);
        }
        let is_global_scope = self.current == 0 && scope == 0;
        let lexical = kind != VarKind::Var;
        let slot = if is_global_scope && self.f().module.is_some() {
            let f = self.fm();
            let vars = &mut f.bytecode.closure_vars;
            let slot = u16::try_from(vars.len()).map_err(|_| CompileError::OutOfMemory)?;
            vars.push(ClosureVarDef {
                name,
                kind: ClosureKind::ModuleDeclaration,
                index: slot,
                var_kind: kind,
                is_const: matches!(kind, VarKind::Const | VarKind::Using | VarKind::AwaitUsing),
                lexical_tdz: lexical,
            });
            Slot::Capture(slot)
        } else if is_global_scope
            && (matches!(self.options.goal, ParseGoal::Script)
                || (!lexical
                    && !self.f().bytecode.strict
                    && matches!(self.options.goal, ParseGoal::DirectEval { .. })))
        {
            self.fm().globals.push((name, kind, None));
            Slot::Global(name)
        } else {
            Slot::Local(self.local(name, kind, scope, lexical)?)
        };
        self.fm().scopes[usize::from(scope)]
            .names
            .insert(name, slot);
        Ok(slot)
    }

    pub(crate) fn draft(
        &mut self,
        span: Span,
        name: UnitAtom,
        strict: bool,
        parent: Option<(usize, u16)>,
    ) -> Result<usize> {
        self.charge(size_of::<Draft>() + self.source.filename.len())?;
        let id = self.functions.len();
        self.functions.push(Draft {
            bytecode: FunctionBytecode {
                name,
                strict,
                debug: DebugInfo {
                    filename: self.source.filename.clone(),
                    source_start: span.start,
                    source_end: span.end,
                    pc_map: Vec::new(),
                },
                ..FunctionBytecode::default()
            },
            parent,
            annex_b: self.options.annex_b && !strict,
            ..Draft::default()
        });
        self.current = id;
        Ok(id)
    }

    fn entry(&mut self) -> Result {
        let name = if matches!(self.options.goal, ParseGoal::DirectEval { .. }) {
            BuiltinAtom::InternalEval.id()
        } else {
            UnitAtom::NONE
        };
        self.draft(self.program.span, name, self.program.strict, None)?;
        let body = &self.program.body;
        if self.options.goal == ParseGoal::Module {
            let name = self.atom(&self.source.filename)?;
            self.fm().module = Some(ModuleRecord {
                name,
                ..ModuleRecord::default()
            });
        }
        self.start_scope(None)?;
        self.inherit_private_names()?;
        if !matches!(self.options.goal, ParseGoal::FunctionBody { .. }) {
            let opcode = if self.f().module.is_some() {
                Opcode::Undefined
            } else {
                Opcode::PushThis
            };
            self.special_local("<this>", opcode, Operands::None, false)?;
        }
        match self.options.goal {
            ParseGoal::FunctionBody {
                is_async,
                is_generator,
            } => {
                self.fm().bytecode.kind = Some(function_kind(is_async, is_generator));
                self.function_environment(false, false, None)?;
            }
            ParseGoal::DirectEval {
                allow_new_target,
                allow_super_property,
                allow_super_call,
                ..
            } => {
                let flags = &mut self.fm().bytecode.flags;
                flags.new_target_allowed = allow_new_target;
                flags.super_allowed = allow_super_property;
                flags.super_call_allowed = allow_super_call;
            }
            _ => (),
        }
        if let Some(slot) = (!matches!(
            self.options.goal,
            ParseGoal::Module | ParseGoal::FunctionBody { .. }
        ))
        .then(|| self.temp())
        .transpose()?
        {
            self.fm().completion = Some(slot);
            self.plain(Opcode::Undefined);
            self.put(slot);
        }
        self.predeclare_vars(body, 0)?;
        self.predeclare_block(body, true)?;
        self.module_entries(body)?;
        if matches!(
            self.kind(),
            FunctionKind::Generator | FunctionKind::AsyncGenerator
        ) {
            self.plain(Opcode::InitialYield);
        }
        self.scoped_body(body, true)?;
        if let Some(local) = self.f().completion {
            self.get(local);
        } else {
            self.plain(Opcode::Undefined);
        }
        if self.f().module_await || self.kind() != FunctionKind::Normal {
            self.plain(Opcode::ReturnAsync);
        } else {
            self.plain(Opcode::Return);
        }
        if self.f().module_await {
            self.fm().bytecode.kind = Some(FunctionKind::Async);
        }
        self.end_scope(0);
        Ok(())
    }

    fn special_local(
        &mut self,
        name: &str,
        opcode: Opcode,
        operand: Operands,
        tdz: bool,
    ) -> Result {
        let atom = self.atom(name)?;
        let scope = self.f().scope;
        let local = self.local(atom, VarKind::Var, scope, tdz)?;
        self.fm().scopes[usize::from(scope)]
            .names
            .insert(atom, Slot::Local(local));
        if !tdz {
            self.op(opcode, operand);
            self.put(local);
        }
        Ok(())
    }

    pub(crate) fn function_environment(
        &mut self,
        method: bool,
        derived: bool,
        self_name: Option<UnitAtom>,
    ) -> Result {
        self.special_local("<this>", Opcode::PushThis, Operands::None, derived)?;
        self.special_local(
            "<new.target>",
            Opcode::SpecialObject,
            Operands::Unsigned(3),
            false,
        )?;
        self.special_local(
            "<active_function>",
            Opcode::SpecialObject,
            Operands::Unsigned(2),
            false,
        )?;
        if method {
            self.special_local(
                "<home_object>",
                Opcode::SpecialObject,
                Operands::Unsigned(4),
                false,
            )?;
        }
        if let Some(name) = self_name {
            let slot = self.local(name, VarKind::FunctionName, 0, false)?;
            self.fm().scopes[0].names.insert(name, Slot::Local(slot));
            self.unsigned(Opcode::SpecialObject, 2);
            self.put(slot);
        }
        Ok(())
    }

    fn copy_parameter_bindings(&mut self, parameters: u16, body: u16) {
        let f = self.f();
        let param_names = &f.scopes[usize::from(parameters)].names;
        let mut copies: Vec<_> = f.scopes[usize::from(body)]
            .names
            .iter()
            .filter_map(|(&name, &slot)| {
                if let Slot::Local(slot) = slot
                    && f.local(slot).kind == VarKind::Var
                    && param_names.contains_key(&name)
                {
                    return Some((name, slot));
                }
                None
            })
            .collect();
        copies.sort_unstable_by_key(|&(_, slot)| slot);
        for (name, slot) in copies {
            self.ir(IrOp::ScopeGetVar(ScopedName {
                name,
                scope: parameters,
            }));
            self.put(slot);
        }
    }

    pub(crate) fn function(
        &mut self,
        function: &Function,
        name: UnitAtom,
        expression: bool,
        method: bool,
        ctor: Option<bool>,
    ) -> Result {
        let parent = self.current;
        let parent_scope = self.f().scope;
        let child = self.draft(
            function.span,
            name,
            function.strict,
            Some((parent, parent_scope)),
        )?;
        self.start_scope(None)?;
        let simple = function
            .params
            .iter()
            .all(|&id| matches!(self.ast().pattern(id).kind, Pattern::Identifier(_)));
        let length = function
            .params
            .iter()
            .take_while(|&&id| {
                !matches!(
                    self.ast().pattern(id).kind,
                    Pattern::Default { .. } | Pattern::Rest(_)
                )
            })
            .count();
        let kind = function_kind(function.is_async, function.is_generator);
        let argc = u16::try_from(function.params.len()).map_err(|_| CompileError::OutOfMemory)?;
        let flags = FunctionFlags {
            has_prototype: !function.is_arrow && !method && kind == FunctionKind::Normal,
            has_simple_parameter_list: simple,
            is_derived_class_constructor: ctor == Some(true),
            need_home_object: method,
            new_target_allowed: !function.is_arrow
                || self.functions[parent].bytecode.flags.new_target_allowed,
            super_call_allowed: ctor == Some(true)
                || (function.is_arrow && self.functions[parent].bytecode.flags.super_call_allowed),
            super_allowed: method
                || (function.is_arrow && self.functions[parent].bytecode.flags.super_allowed),
            arguments_allowed: true,
            name_is_inferred: method || function.name.is_none(),
            ..FunctionFlags::default()
        };
        let f = self.fm();
        f.bytecode.kind = Some(kind);
        f.bytecode.arg_count = argc;
        f.bytecode.defined_arg_count = length as u16;
        f.bytecode.flags = flags;
        f.bytecode.vars.resize(usize::from(argc), VarDef::default());
        if ctor.is_some() {
            self.plain(Opcode::CheckCtor);
        }
        let self_name = if expression {
            function
                .name
                .as_ref()
                .map(|name| self.atom(&name.name))
                .transpose()?
        } else {
            None
        };
        if !function.is_arrow {
            self.function_environment(method, ctor == Some(true), self_name)?;
        }
        if ctor == Some(false) {
            self.initialize_instance_fields()?;
        }
        let mut parameter_expressions = false;
        for &id in &function.params {
            if self.parameter_expressions(id)? {
                parameter_expressions = true;
                break;
            }
        }
        let params = self.start_scope(Some(0))?;
        self.fm().scopes[usize::from(params)].parameter_end = parameter_expressions;
        for (i, &p) in function.params.iter().enumerate() {
            if simple && let Pattern::Identifier(name) = &self.ast().pattern(p).kind {
                let atom = self.atom(&name.name)?;
                let arg = &mut self.fm().bytecode.vars[i];
                arg.name = atom;
                arg.scope_level = params;
                self.fm().scopes[usize::from(params)]
                    .names
                    .insert(atom, Slot::Arg(i as u16));
                continue;
            }
            self.bind_pattern(p, VarKind::Let, params)?;
        }
        if !simple && !parameter_expressions {
            // These parameters need TDZ checks during initialization but share the var scope.
            let f = self.fm();
            for &slot in &f.scopes[usize::from(params)].lexical {
                f.bytecode.vars[usize::from(f.bytecode.arg_count) + usize::from(slot)].kind =
                    VarKind::Var;
            }
        }
        if !function.is_arrow
            && !self
                .f()
                .scope()
                .names
                .contains_key(&BuiltinAtom::ArgumentsName.id())
        {
            let mapped = !function.strict && simple;
            self.special_local(
                "arguments",
                Opcode::SpecialObject,
                Operands::Unsigned(u32::from(mapped)),
                false,
            )?;
            if parameter_expressions
                && let Some(&Slot::Local(slot)) =
                    self.f().scope().names.get(&BuiltinAtom::ArgumentsName.id())
            {
                // Direct eval in parameter initializers cannot redeclare this binding.
                let arguments = &mut self.fm().bytecode.vars[usize::from(argc) + usize::from(slot)];
                arguments.kind = VarKind::Let;
                arguments.lexical_tdz = true;
                self.fm().scopes[usize::from(params)].lexical.push(slot);
            }
            if mapped {
                for index in 0..argc {
                    self.capture_frame_slot(child, Slot::Arg(index))?;
                }
            }
        }
        if !simple {
            for (i, &p) in function.params.iter().enumerate() {
                if matches!(self.ast().pattern(p).kind, Pattern::Rest(_)) {
                    self.unsigned(Opcode::Rest, i as u32);
                } else {
                    self.op(Opcode::GetArg, Operands::Argument(i as u16));
                }
                self.bind_value(p, true)?;
            }
        }
        if matches!(kind, FunctionKind::Generator | FunctionKind::AsyncGenerator) {
            self.plain(Opcode::InitialYield);
        }
        let body_scope = self.start_scope(Some(params))?;
        self.fm().vars_scope = if parameter_expressions {
            body_scope
        } else {
            params
        };
        match &function.body {
            FunctionBody::Block(body) => {
                self.predeclare_vars(body, self.f().vars_scope)?;
                self.predeclare_block(body, true)?;
                if parameter_expressions {
                    self.copy_parameter_bindings(params, body_scope);
                }
                self.scoped_body(body, true)?;
                self.plain(Opcode::Undefined);
            }
            FunctionBody::Expression(expr) => self.expression(*expr)?,
        }
        self.return_value()?;
        self.end_scope(body_scope);
        self.end_scope(params);
        self.end_scope(0);
        self.current = parent;
        let constant = self.constant(ConstEntry::Function(
            u32::try_from(child).map_err(|_| CompileError::OutOfMemory)?,
        ))?;
        if ctor.is_some() {
            self.op(Opcode::PushConst, Operands::Constant(constant));
        } else {
            self.op(Opcode::Fclosure, Operands::Constant(constant));
        }
        Ok(())
    }
}

pub(crate) fn string_bytes(s: &ConstString) -> usize {
    match s {
        ConstString::Latin1(v) => v.len(),
        ConstString::Utf16(v) => v.len().saturating_mul(2),
    }
}

fn function_kind(is_async: bool, is_generator: bool) -> FunctionKind {
    match (is_async, is_generator) {
        (false, false) => FunctionKind::Normal,
        (false, true) => FunctionKind::Generator,
        (true, false) => FunctionKind::Async,
        (true, true) => FunctionKind::AsyncGenerator,
    }
}
