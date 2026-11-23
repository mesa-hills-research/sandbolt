use crate::{
    CompileError,
    emit::{Cleanup, Control, Emitter, Result, Slot},
};
use qjs_bytecode::*;
use qjs_syntax::*;

impl Emitter<'_> {
    pub(crate) fn predeclare_vars(&mut self, body: &[StmtId], scope: u16) -> Result {
        for &id in body {
            self.scan_var(id, scope)?;
        }
        Ok(())
    }

    fn scan_var(&mut self, id: StmtId, scope: u16) -> Result {
        self.descent(|e| {
            let node = e.ast().stmt(id);
            match &node.kind {
                Stmt::Declaration(decl) if decl.kind == DeclKind::Var => e.declare(decl, scope)?,
                Stmt::Block(body) => e.predeclare_vars(body, scope)?,
                Stmt::If {
                    consequent,
                    alternate,
                    ..
                } => {
                    e.scan_var(*consequent, scope)?;
                    if let Some(stmt) = alternate {
                        e.scan_var(*stmt, scope)?;
                    }
                }
                Stmt::For { init, body, .. } => {
                    if let Some(ForInit::Declaration(decl)) = init
                        && decl.kind == DeclKind::Var
                    {
                        e.declare(decl, scope)?;
                    }
                    e.scan_var(*body, scope)?;
                }
                Stmt::ForEach { target, body, .. } => {
                    if let ForTarget::Declaration(decl) = target
                        && decl.kind == DeclKind::Var
                    {
                        e.declare(decl, scope)?;
                    }
                    e.scan_var(*body, scope)?;
                }
                Stmt::Switch { cases, .. } => {
                    for case in cases {
                        e.predeclare_vars(&case.body, scope)?;
                    }
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                } => {
                    e.scan_var(*body, scope)?;
                    if let Some(catch) = catch {
                        e.scan_var(catch.body, scope)?;
                    }
                    if let Some(finally) = finally {
                        e.scan_var(*finally, scope)?;
                    }
                }
                Stmt::While { body, .. }
                | Stmt::DoWhile { body, .. }
                | Stmt::With { body, .. }
                | Stmt::Label { body, .. } => e.scan_var(*body, scope)?,
                Stmt::Export(
                    ExportDeclaration::Declaration(id) | ExportDeclaration::DefaultDeclaration(id),
                ) => e.scan_var(*id, scope)?,
                _ => (),
            }
            Ok(())
        })
    }

    pub(crate) fn predeclare_block(&mut self, body: &[StmtId], top: bool) -> Result {
        for &id in body {
            self.predeclare_stmt(id, top)?;
        }
        Ok(())
    }

    fn predeclare_stmt(&mut self, id: StmtId, top: bool) -> Result {
        let scope = self.f().scope;
        let node = self.ast().stmt(id);
        match &node.kind {
            Stmt::Declaration(decl) if decl.kind != DeclKind::Var => self.declare(decl, scope)?,
            Stmt::Function(f) => {
                let name = f
                    .name
                    .as_ref()
                    .map(|i| self.atom(&i.name))
                    .transpose()?
                    .unwrap_or(BuiltinAtom::DefaultExport.id());
                let (scope, kind) = if top && self.f().module.is_none() {
                    (self.f().vars_scope, VarKind::Var)
                } else {
                    (
                        scope,
                        if f.is_async || f.is_generator {
                            VarKind::AsyncOrGeneratorDeclaration
                        } else {
                            VarKind::FunctionDeclaration
                        },
                    )
                };
                self.bind(name, kind, scope)?;
            }
            Stmt::Class(class) => {
                let name = class
                    .name
                    .as_ref()
                    .map(|i| self.atom(&i.name))
                    .transpose()?
                    .unwrap_or(BuiltinAtom::DefaultExport.id());
                self.bind(name, VarKind::Let, scope)?;
            }
            Stmt::Export(
                ExportDeclaration::Declaration(id) | ExportDeclaration::DefaultDeclaration(id),
            ) => self.predeclare_stmt(*id, top)?,
            Stmt::Export(ExportDeclaration::DefaultExpression(_)) => {
                self.bind(BuiltinAtom::DefaultExport.id(), VarKind::Const, scope)?;
            }
            _ => (),
        }
        Ok(())
    }

    fn hoist(&mut self, body: &[StmtId], top: bool) -> Result {
        let mut initialized = std::collections::HashSet::new();
        for &id in body {
            let node = self.ast().stmt(id);
            let node = match node.kind {
                Stmt::Export(
                    ExportDeclaration::Declaration(id) | ExportDeclaration::DefaultDeclaration(id),
                ) => self.ast().stmt(id),
                _ => node,
            };
            if let Stmt::Function(function) = &node.kind {
                let name = function
                    .name
                    .as_ref()
                    .map(|i| self.atom(&i.name))
                    .transpose()?
                    .unwrap_or(BuiltinAtom::DefaultExport.id());
                let display = if name == BuiltinAtom::DefaultExport.id() {
                    BuiltinAtom::Default.id()
                } else {
                    name
                };
                let scope = if top && self.f().module.is_none() {
                    self.f().vars_scope
                } else {
                    self.f().scope
                };
                if !top && !function.is_async && !function.is_generator && self.f().annex_b {
                    self.annex_mirror(name, scope)?;
                }
                self.source_loc(node.span);
                self.function(function, display, false, false, None)?;
                if self.declaration_slot(scope, name) == Some(Slot::Global(name)) {
                    let Some(IrOp::Resolved(Instruction {
                        operands: Operands::Constant(i),
                        ..
                    })) = self.fm().ir.pop()
                    else {
                        return Err(CompileError::Bytecode(
                            "missing hoisted function constant".into(),
                        ));
                    };
                    if let Some(declaration) =
                        self.fm().globals.iter_mut().find(|entry| entry.0 == name)
                    {
                        declaration.2 = Some(i);
                    }
                } else {
                    let name = ScopedName { name, scope };
                    if top || initialized.insert(name.name) {
                        self.ir(IrOp::ScopePutVarInit(name));
                    } else {
                        self.ir(IrOp::ScopePutVar(name));
                    }
                }
            }
        }
        Ok(())
    }

    fn annex_mirror(&mut self, name: UnitAtom, scope: u16) -> Result {
        let variable = self.f().vars_scope;
        let mut parent = self.f().scopes[usize::from(scope)].info.parent;
        while let Some(index) = parent {
            if let Some(binding) = self.declaration_slot(index, name) {
                let conflict = match binding {
                    Slot::Arg(_) => true,
                    Slot::Local(slot) => {
                        let var = self.f().local(slot);
                        (var.lexical_tdz && var.kind != VarKind::Catch)
                            || (name == BuiltinAtom::ArgumentsName.id() && var.kind == VarKind::Var)
                    }
                    Slot::Global(atom) => self
                        .f()
                        .globals
                        .iter()
                        .any(|&(a, kind, _)| a == atom && kind != VarKind::Var),
                    Slot::Capture(_) => true,
                };
                if conflict {
                    return Ok(());
                }
            }
            parent = self.f().scopes[usize::from(index)].info.parent;
        }
        self.bind(name, VarKind::Var, variable)?;
        self.fm().annex_mirrors.insert((scope, name), variable);
        Ok(())
    }

    pub(crate) fn scoped_body(&mut self, body: &[StmtId], top: bool) -> Result {
        let scope = self.f().scope;
        self.hoist(body, top)?;
        self.disposal_scope(scope, |e| {
            for &id in body {
                e.statement(id)?;
            }
            Ok(())
        })
    }

    fn disposal_scope(&mut self, scope: u16, body: impl FnOnce(&mut Self) -> Result) -> Result {
        use Opcode::*;
        let info = &self.f().scopes[usize::from(scope)].info;
        if !info.has_using && !info.has_await_using {
            return body(self);
        }
        let handler = self.label();
        let cleanup = self.label();
        let done = self.label();
        let error = self.temp()?;
        self.ir(IrOp::Catch(handler));
        let depth = self.f().cleanup.len();
        self.fm()
            .cleanup
            .extend([Cleanup::Dispose(cleanup, error), Cleanup::Catch]);
        body(self)?;
        self.fm().cleanup.truncate(depth);
        self.plain(Drop);
        self.plain(UsingDisposeInit);
        self.put(error);
        self.ir(IrOp::Gosub(cleanup));
        self.jump(done);
        self.mark(handler);
        self.put(error);
        self.ir(IrOp::Gosub(cleanup));
        self.jump(done);
        self.mark(cleanup);
        self.get(error);
        self.ir(IrOp::DisposeScope(scope));
        if self.f().scopes[usize::from(scope)].info.has_await_using && self.f().module.is_some() {
            self.fm().module_await = true;
        }
        self.plain(UsingDisposeEnd);
        self.plain(Ret);
        self.mark(done);
        Ok(())
    }

    fn completion_reset(&mut self) {
        if let Some(local) = self.f().completion {
            self.plain(Opcode::Undefined);
            self.put(local);
        }
    }

    pub(crate) fn statement(&mut self, id: StmtId) -> Result {
        self.descent(|e| e.statement_inner(id, &[]))
    }

    fn statement_inner(&mut self, id: StmtId, labels: &[UnitAtom]) -> Result {
        use Opcode::*;
        let node = self.ast().stmt(id);
        self.source_loc(node.span);
        match &node.kind {
            Stmt::Empty | Stmt::Debugger | Stmt::Import(_) => (),
            Stmt::Function(f) => {
                if let Some(name) = &f.name {
                    let name = self.atom(&name.name)?;
                    if let Some(&scope) = self.f().annex_mirrors.get(&(self.f().scope, name)) {
                        self.read(name);
                        self.ir(IrOp::ScopePutVar(ScopedName { name, scope }));
                    }
                }
            }
            Stmt::Expression(expr) => {
                self.expression(*expr)?;
                if let Some(local) = self.f().completion {
                    self.put(local);
                } else {
                    self.plain(Drop);
                }
            }
            Stmt::Block(body) => {
                let scope = self.start_scope(Some(self.f().scope))?;
                self.predeclare_block(body, false)?;
                self.fm().cleanup.push(Cleanup::Scope(scope));
                self.scoped_body(body, false)?;
                self.fm().cleanup.pop();
                self.end_scope(scope);
            }
            Stmt::Declaration(decl) => self.declaration_value(decl)?,
            Stmt::Class(c) => {
                let name = c
                    .name
                    .as_ref()
                    .map(|i| self.atom(&i.name))
                    .transpose()?
                    .unwrap_or(BuiltinAtom::DefaultExport.id());
                self.class(
                    c,
                    node.span,
                    (name == BuiltinAtom::DefaultExport.id()).then_some(BuiltinAtom::Default.id()),
                )?;
                self.ir(IrOp::ScopePutVarInit(self.scoped(name)));
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                self.completion_reset();
                let otherwise = self.label();
                let done = self.label();
                self.expression(*test)?;
                self.if_false(otherwise);
                self.single_statement(*consequent)?;
                self.jump(done);
                self.mark(otherwise);
                if let Some(stmt) = alternate {
                    self.single_statement(*stmt)?;
                }
                self.mark(done);
            }
            Stmt::While { test, body } => self.while_loop(*test, *body, false, labels)?,
            Stmt::DoWhile { body, test } => self.while_loop(*test, *body, true, labels)?,
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.for_loop(init.as_ref(), *test, *update, *body, labels)?,
            Stmt::ForEach {
                kind,
                target,
                iterable,
                body,
            } => self.for_each(node.span, *kind, target, *iterable, *body, labels)?,
            Stmt::Switch {
                discriminant,
                cases,
            } => self.switch(*discriminant, cases)?,
            Stmt::Try {
                body,
                catch,
                finally,
            } => self.try_statement(*body, catch.as_ref(), *finally)?,
            Stmt::With { object, body } => {
                self.completion_reset();
                self.expression(*object)?;
                self.plain(ToObject);
                let scope = self.start_scope(Some(self.f().scope))?;
                let with = self.local(BuiltinAtom::InternalWith.id(), VarKind::Let, scope, true)?;
                self.fm().scopes[usize::from(scope)].with_object = Some(with);
                self.put(with);
                self.fm().cleanup.push(Cleanup::Scope(scope));
                self.statement(*body)?;
                self.fm().cleanup.pop();
                self.end_scope(scope);
            }
            Stmt::Label { name, body } => {
                let name = self.atom(&name.name)?;
                let mut labels = labels.to_vec();
                labels.push(name);
                match self.ast().stmt(*body).kind {
                    Stmt::Label { .. }
                    | Stmt::While { .. }
                    | Stmt::DoWhile { .. }
                    | Stmt::For { .. }
                    | Stmt::ForEach { .. } => {
                        self.descent(|e| e.statement_inner(*body, &labels))?;
                    }
                    _ => {
                        let exit = self.label();
                        let depth = self.f().cleanup.len();
                        let before = self.f().control.len();
                        for name in labels {
                            self.control(Some(name), exit, None, depth, false);
                        }
                        self.single_statement(*body)?;
                        self.fm().control.truncate(before);
                        self.mark(exit);
                    }
                }
            }
            Stmt::Break(label) | Stmt::Continue(label) => {
                let continuing = matches!(node.kind, Stmt::Continue(_));
                let name = label
                    .as_ref()
                    .map(|label| self.atom(&label.name))
                    .transpose()?;
                let destination = self
                    .f()
                    .control
                    .iter()
                    .rev()
                    .find(|control| {
                        (if name.is_none() {
                            control.anonymous
                        } else {
                            control.name == name
                        }) && (!continuing || control.next.is_some())
                    })
                    .ok_or_else(|| self.error(node.span, "No matching control target"))?;
                let (label, depth) = if continuing {
                    (
                        destination.next.expect("loop target"),
                        destination.next_depth,
                    )
                } else {
                    (destination.exit, destination.exit_depth)
                };
                self.cleanup_to(depth)?;
                self.jump(label);
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expression(*value)?;
                    if self.kind() == FunctionKind::AsyncGenerator {
                        self.await_value();
                    }
                } else {
                    self.plain(Undefined);
                }
                self.return_value()?;
            }
            Stmt::Throw(value) => {
                self.expression(*value)?;
                self.plain(Throw);
            }
            Stmt::Export(export) => match export {
                ExportDeclaration::Declaration(id) | ExportDeclaration::DefaultDeclaration(id) => {
                    self.statement(*id)?
                }
                ExportDeclaration::DefaultExpression(id) => {
                    self.expression_named(*id, Some(BuiltinAtom::Default.id()))?;
                    self.ir(IrOp::ScopePutVarInit(
                        self.scoped(BuiltinAtom::DefaultExport.id()),
                    ));
                }
                _ => (),
            },
        }
        Ok(())
    }

    fn single_statement(&mut self, id: StmtId) -> Result {
        if matches!(self.ast().stmt(id).kind, Stmt::Function(_)) {
            let scope = self.start_scope(Some(self.f().scope))?;
            self.predeclare_stmt(id, false)?;
            self.scoped_body(&[id], false)?;
            self.end_scope(scope);
        } else {
            self.statement(id)?;
        }
        Ok(())
    }

    fn control(
        &mut self,
        name: Option<UnitAtom>,
        exit: u32,
        next: Option<u32>,
        depth: usize,
        anonymous: bool,
    ) {
        self.fm().control.push(Control {
            name,
            exit,
            next,
            exit_depth: depth,
            next_depth: depth,
            anonymous,
        });
    }

    fn loop_controls(&mut self, labels: &[UnitAtom], exit: u32, next: u32) -> usize {
        let count = self.f().control.len();
        let depth = self.f().cleanup.len();
        self.control(None, exit, Some(next), depth, true);
        for &label in labels {
            self.control(Some(label), exit, Some(next), depth, false);
        }
        count
    }

    fn while_loop(
        &mut self,
        test: ExprId,
        body: StmtId,
        post: bool,
        labels: &[UnitAtom],
    ) -> Result {
        self.completion_reset();
        let next = self.label();
        let enter = self.label();
        let exit = self.label();
        let count = self.loop_controls(labels, exit, next);
        if post {
            self.jump(enter);
        }
        self.mark(next);
        self.expression(test)?;
        self.if_false(exit);
        self.mark(enter);
        self.statement(body)?;
        self.jump(next);
        self.mark(exit);
        self.fm().control.truncate(count);
        Ok(())
    }

    fn for_loop(
        &mut self,
        init: Option<&ForInit>,
        test: Option<ExprId>,
        update: Option<ExprId>,
        body: StmtId,
        labels: &[UnitAtom],
    ) -> Result {
        self.completion_reset();
        let scope = self.start_scope(Some(self.f().scope))?;
        if let Some(ForInit::Declaration(d)) = init
            && d.kind != DeclKind::Var
        {
            self.declare(d, scope)?;
        }
        self.fm().cleanup.push(Cleanup::Scope(scope));
        self.disposal_scope(scope, |e| {
            let per_iteration =
                matches!(init, Some(ForInit::Declaration(d)) if d.kind == DeclKind::Let);
            match init {
                Some(ForInit::Declaration(d)) => e.declaration_value(d)?,
                Some(ForInit::Expression(expr)) => {
                    e.expression(*expr)?;
                    e.plain(Opcode::Drop);
                }
                None => (),
            }
            if per_iteration {
                e.copy_iteration(scope);
            }
            let test_label = e.label();
            let update_label = e.label();
            let exit = e.label();
            let count = e.loop_controls(labels, exit, update_label);
            e.mark(test_label);
            if let Some(test) = test {
                e.expression(test)?;
                e.if_false(exit);
            }
            e.statement(body)?;
            e.mark(update_label);
            if per_iteration {
                e.copy_iteration(scope);
            }
            if let Some(update) = update {
                e.expression(update)?;
                e.plain(Opcode::Drop);
            }
            e.jump(test_label);
            e.mark(exit);
            e.fm().control.truncate(count);
            Ok(())
        })?;
        self.fm().cleanup.pop();
        self.end_scope(scope);
        Ok(())
    }

    fn for_each(
        &mut self,
        span: Span,
        kind: ForKind,
        target: &ForTarget,
        iterable: ExprId,
        body: StmtId,
        labels: &[UnitAtom],
    ) -> Result {
        use Opcode::*;
        self.completion_reset();
        let scope = self.start_scope(Some(self.f().scope))?;
        let (p, lexical) = match target {
            ForTarget::Pattern(id) => (*id, false),
            ForTarget::Declaration(d) => {
                if d.kind != DeclKind::Var {
                    self.declare(d, scope)?;
                }
                if d.kind == DeclKind::Var && d.entries[0].initializer.is_some() {
                    self.declaration_value(d)?;
                }
                (d.entries[0].binding, d.kind != DeclKind::Var)
            }
        };
        self.expression(iterable)?;
        let cleanup = match kind {
            ForKind::In => {
                self.plain(ForInStart);
                Cleanup::Drop
            }
            ForKind::Of => {
                self.plain(ForOfStart);
                Cleanup::CloseIterator(span)
            }
            ForKind::AwaitOf => {
                self.plain(ForAwaitOfStart);
                Cleanup::AsyncIterator(span)
            }
        };
        self.fm().cleanup.push(cleanup);
        let next = self.label();
        let done = self.label();
        let exit = self.label();
        let count = self.loop_controls(labels, exit, next);
        let asynchronous = if kind == ForKind::AwaitOf {
            self.plain(Dup3);
            self.plain(Drop);
            let method = self.stash()?;
            let iterator = self.stash()?;
            Some((iterator, method, self.label(), self.label(), self.label()))
        } else {
            None
        };
        self.mark(next);
        match kind {
            ForKind::In => self.plain(ForInNext),
            ForKind::Of => self.unsigned(ForOfNext, 0),
            ForKind::AwaitOf => {
                let (iterator, method, step_error, _, _) = asynchronous.expect("async iterator");
                self.ir(IrOp::Catch(step_error));
                self.get(iterator);
                self.get(method);
                self.op(CallMethod, Operands::Call(0));
                self.await_value();
                self.plain(CheckObject);
                self.named(GetField2, BuiltinAtom::Done.id());
            }
        }
        self.if_true(done);
        if asynchronous.is_some() {
            self.named(GetField, BuiltinAtom::Value.id());
        }
        if lexical {
            let vars = self.f().scopes[usize::from(scope)].lexical.clone();
            for slot in vars {
                self.op(SetLocUninitialized, Operands::Local(slot));
            }
        }
        self.fm().cleanup.push(Cleanup::Scope(scope));
        let iteration_value = self.stash()?;
        if let Some((_, _, _, body_error, _)) = asynchronous {
            self.plain(Drop);
            self.ir(IrOp::Catch(body_error));
            self.fm().cleanup.push(Cleanup::Catch);
        }
        self.disposal_scope(scope, |e| {
            e.get(iteration_value);
            if matches!(target, ForTarget::Declaration(d) if matches!(d.kind, DeclKind::Using | DeclKind::AwaitUsing)) { e.dispose_resource(p)?; }
            e.bind_value(p, lexical)?;
            e.statement(body)
        })?;
        if asynchronous.is_some() {
            self.fm().cleanup.pop();
            self.plain(Drop);
        }
        self.fm().cleanup.pop();
        self.close_bindings(scope);
        self.jump(next);
        self.mark(done);
        self.plain(Drop);
        if let Some((iterator, _, step_error, body_error, complete)) = asynchronous {
            for _ in 0..4 {
                self.plain(Drop);
            }
            self.jump(complete);
            self.mark(step_error);
            let error = self.stash()?;
            for _ in 0..3 {
                self.plain(Drop);
            }
            self.get(error);
            self.plain(Throw);
            self.mark(body_error);
            self.put(error);
            for _ in 0..3 {
                self.plain(Drop);
            }
            let ignored = self.label();
            let rethrow = self.label();
            self.ir(IrOp::Catch(ignored));
            self.get(iterator);
            self.async_close_object();
            self.plain(Drop);
            self.jump(rethrow);
            self.mark(ignored);
            self.plain(Drop);
            self.mark(rethrow);
            self.get(error);
            self.plain(Throw);
        }
        self.mark(exit);
        self.fm().control.truncate(count);
        self.fm().cleanup.pop();
        self.one_cleanup(cleanup)?;
        if let Some((_, _, _, _, complete)) = asynchronous {
            self.mark(complete);
        }
        self.end_scope(scope);
        Ok(())
    }

    fn switch(&mut self, discriminant: ExprId, cases: &[SwitchCase]) -> Result {
        use Opcode::*;
        self.completion_reset();
        self.expression(discriminant)?;
        let value = self.stash()?;
        let scope = self.start_scope(Some(self.f().scope))?;
        let body: Vec<_> = cases
            .iter()
            .flat_map(|case| case.body.iter().copied())
            .collect();
        self.predeclare_block(&body, false)?;
        self.hoist(&body, false)?;
        let done = self.label();
        let labels: Vec<_> = cases.iter().map(|_| self.label()).collect();
        let count = self.f().control.len();
        self.fm().cleanup.push(Cleanup::Scope(scope));
        self.disposal_scope(scope, |e| {
            let depth = e.f().cleanup.len();
            e.control(None, done, None, depth, true);
            let mut default = done;
            for (case, &label) in cases.iter().zip(&labels) {
                if let Some(test) = case.test {
                    e.get(value);
                    e.expression(test)?;
                    e.plain(StrictEq);
                    e.if_true(label);
                } else {
                    default = label;
                }
            }
            e.jump(default);
            for (case, &label) in cases.iter().zip(&labels) {
                e.mark(label);
                for &id in &case.body {
                    e.statement(id)?;
                }
            }
            e.mark(done);
            Ok(())
        })?;
        self.fm().control.truncate(count);
        self.fm().cleanup.pop();
        self.end_scope(scope);
        Ok(())
    }

    fn try_statement(
        &mut self,
        body: StmtId,
        catch: Option<&Catch>,
        finally: Option<StmtId>,
    ) -> Result {
        use Opcode::*;
        self.completion_reset();
        let depth = self.f().cleanup.len();
        let finalizer = finally.map(|id| (id, self.label(), self.label(), self.label()));
        if let Some((_, handler, sub, _)) = finalizer {
            self.ir(IrOp::Catch(handler));
            self.fm()
                .cleanup
                .extend([Cleanup::Finally(sub), Cleanup::Catch]);
        }
        if let Some(catch) = catch {
            let handler = self.label();
            let after = self.label();
            self.ir(IrOp::Catch(handler));
            self.fm().cleanup.push(Cleanup::Catch);
            self.statement(body)?;
            self.fm().cleanup.pop();
            self.plain(Drop);
            self.jump(after);
            self.mark(handler);
            self.completion_reset();
            let scope = self.start_scope(Some(self.f().scope))?;
            if let Some(p) = catch.binding {
                let kind = if matches!(self.ast().pattern(p).kind, Pattern::Identifier(_)) {
                    VarKind::Catch
                } else {
                    VarKind::Let
                };
                self.bind_pattern(p, kind, scope)?;
                self.bind_value(p, true)?;
            } else {
                self.plain(Drop);
            }
            self.fm().cleanup.push(Cleanup::Scope(scope));
            self.statement(catch.body)?;
            self.fm().cleanup.pop();
            self.end_scope(scope);
            self.mark(after);
        } else {
            self.statement(body)?;
        }
        self.fm().cleanup.truncate(depth);
        if let Some((finally, handler, sub, done)) = finalizer {
            self.plain(Drop);
            self.ir(IrOp::Gosub(sub));
            self.jump(done);
            self.mark(handler);
            let error = self.stash()?;
            self.ir(IrOp::Gosub(sub));
            self.get(error);
            self.plain(Throw);
            self.mark(sub);
            let completion = self.f().completion;
            let saved = if let Some(completion) = completion {
                self.get(completion);
                Some(self.stash()?)
            } else {
                None
            };
            self.completion_reset();
            self.fm().cleanup.push(Cleanup::Drop);
            self.statement(finally)?;
            self.fm().cleanup.pop();
            if let (Some(saved), Some(completion)) = (saved, completion) {
                self.get(saved);
                self.put(completion);
            }
            self.plain(Ret);
            self.mark(done);
        }
        Ok(())
    }

    pub(crate) fn return_value(&mut self) -> Result {
        use Opcode::*;
        for i in (0..self.f().cleanup.len()).rev() {
            let cleanup = self.f().cleanup[i];
            match cleanup {
                Cleanup::Catch => self.plain(NipCatch),
                Cleanup::CloseIterator(_) | Cleanup::AsyncIterator(_) => {
                    self.plain(NipCatch);
                    self.plain(Rot3r);
                    self.plain(Undefined);
                    self.one_cleanup(cleanup)?;
                }
                Cleanup::Finally(_) | Cleanup::Dispose(_, _) => {
                    let value = self.stash()?;
                    self.one_cleanup(cleanup)?;
                    self.get(value);
                }
                Cleanup::Scope(_) => self.one_cleanup(cleanup)?,
                Cleanup::Drop => (),
            }
        }
        if self.f().bytecode.flags.is_derived_class_constructor {
            let object = self.label();
            self.plain(CheckCtorReturn);
            self.if_false(object);
            self.plain(Drop);
            self.read_internal("<this>")?;
            self.mark(object);
        }
        if self.kind() == FunctionKind::Normal {
            self.plain(Return);
        } else {
            self.plain(ReturnAsync);
        }
        Ok(())
    }

    fn cleanup_to(&mut self, depth: usize) -> Result {
        for i in (depth..self.f().cleanup.len()).rev() {
            self.one_cleanup(self.f().cleanup[i])?;
        }
        Ok(())
    }

    fn async_close_object(&mut self) {
        use Opcode::*;
        let discard = self.label();
        let call = self.label();
        self.plain(Dup);
        self.plain(IsUndefined);
        self.if_true(discard);
        self.named(GetField2, BuiltinAtom::Return.id());
        self.plain(Dup);
        self.plain(IsUndefinedOrNull);
        self.if_false(call);
        self.plain(Drop);
        self.jump(discard);
        self.mark(call);
        self.op(CallMethod, Operands::Call(0));
        self.await_value();
        self.plain(CheckObject);
        self.mark(discard);
        self.plain(Drop);
    }

    fn one_cleanup(&mut self, cleanup: Cleanup) -> Result {
        use Opcode::*;
        match cleanup {
            Cleanup::Drop | Cleanup::Catch => self.plain(Drop),
            Cleanup::CloseIterator(span) => {
                self.source_loc(span);
                self.plain(IteratorClose);
            }
            Cleanup::AsyncIterator(span) => {
                self.source_loc(span);
                self.plain(Drop);
                self.plain(Drop);
                self.async_close_object();
            }
            Cleanup::Finally(label) => self.ir(IrOp::Gosub(label)),
            Cleanup::Dispose(label, error) => {
                self.plain(UsingDisposeInit);
                self.put(error);
                self.ir(IrOp::Gosub(label));
            }
            Cleanup::Scope(scope) => self.close_bindings(scope),
        }
        Ok(())
    }
}
