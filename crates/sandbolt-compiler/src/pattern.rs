use crate::{
    emit::{Cleanup, Emitter, Result, Slot},
    expr::Reference,
};
use sandbolt_bytecode::*;
use sandbolt_syntax::*;

struct Target {
    pattern: PatternId,
    default: Option<ExprId>,
    reference: Option<Reference>,
    name: Option<UnitAtom>,
    operands: Vec<u16>,
}

impl Emitter<'_> {
    pub(crate) fn bind_pattern(&mut self, id: PatternId, kind: VarKind, scope: u16) -> Result {
        self.descent(|e| match &e.ast().pattern(id).kind {
            Pattern::Identifier(ident) => {
                let atom = e.atom(&ident.name)?;
                e.bind(atom, kind, scope)?;
                Ok(())
            }
            Pattern::Default { target, .. } | Pattern::Rest(target) => {
                e.bind_pattern(*target, kind, scope)
            }
            Pattern::Reference(_) => Ok(()),
            Pattern::Object { properties, rest } => {
                for property in properties {
                    e.bind_pattern(property.target, kind, scope)?;
                }
                if let Some(rest) = rest {
                    e.bind_pattern(*rest, kind, scope)?;
                }
                Ok(())
            }
            Pattern::Array { items, rest } => {
                for id in items.iter().flatten() {
                    e.bind_pattern(*id, kind, scope)?;
                }
                if let Some(rest) = rest {
                    e.bind_pattern(*rest, kind, scope)?;
                }
                Ok(())
            }
        })
    }

    pub(crate) fn parameter_expressions(&mut self, id: PatternId) -> Result<bool> {
        self.descent(|e| match &e.ast().pattern(id).kind {
            Pattern::Default { .. } => Ok(true),
            Pattern::Rest(id) => e.parameter_expressions(*id),
            Pattern::Object { properties, rest } => {
                for p in properties {
                    if matches!(p.key, Name::Computed(_)) || e.parameter_expressions(p.target)? {
                        return Ok(true);
                    }
                }
                rest.map(|id| e.parameter_expressions(id))
                    .unwrap_or(Ok(false))
            }
            Pattern::Array { items, rest } => {
                for &id in items.iter().flatten().chain(rest.iter()) {
                    if e.parameter_expressions(id)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => Ok(false),
        })
    }

    pub(crate) fn pattern_atom(&mut self, id: PatternId) -> Result<Option<UnitAtom>> {
        match &self.ast().pattern(id).kind {
            Pattern::Identifier(ident) => self.atom(&ident.name).map(Some),
            Pattern::Default { target, .. } | Pattern::Rest(target) => {
                self.descent(|e| e.pattern_atom(*target))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn pattern_atoms(&mut self, id: PatternId, out: &mut Vec<UnitAtom>) -> Result {
        self.descent(|e| match &e.ast().pattern(id).kind {
            Pattern::Identifier(i) => {
                out.push(e.atom(&i.name)?);
                Ok(())
            }
            Pattern::Default { target, .. } | Pattern::Rest(target) => {
                e.pattern_atoms(*target, out)
            }
            Pattern::Object { properties, rest } => {
                for p in properties {
                    e.pattern_atoms(p.target, out)?;
                }
                if let Some(id) = rest {
                    e.pattern_atoms(*id, out)?;
                }
                Ok(())
            }
            Pattern::Array { items, rest } => {
                for id in items.iter().flatten().chain(rest.iter()) {
                    e.pattern_atoms(*id, out)?;
                }
                Ok(())
            }
            Pattern::Reference(_) => Ok(()),
        })
    }

    fn prepare_target(&mut self, id: PatternId, init: bool, freeze: bool) -> Result<Target> {
        let mut pattern = id;
        let mut default = None;
        let mut reference = None;
        let mut name = None;
        loop {
            match &self.ast().pattern(pattern).kind {
                Pattern::Identifier(i) => {
                    reference = Some(self.reference_name(i, init)?);
                    name = Some(self.atom(&i.name)?);
                    break;
                }
                Pattern::Reference(id) => {
                    reference = Some(self.reference(*id)?);
                    break;
                }
                Pattern::Rest(id) => pattern = *id,
                Pattern::Default { target, value } => {
                    pattern = *target;
                    default = Some(*value);
                }
                _ => break,
            }
        }
        let mut operands = Vec::new();
        if freeze && let Some(r) = reference {
            for _ in 0..r.width() {
                operands.push(self.stash()?);
            }
        }
        Ok(Target {
            pattern,
            default,
            reference,
            name,
            operands,
        })
    }

    fn finish_target(&mut self, target: Target, init: bool) -> Result {
        if let Some(value) = target.default {
            let assigned = self.label();
            self.plain(Opcode::Dup);
            self.plain(Opcode::IsUndefined);
            self.if_false(assigned);
            self.plain(Opcode::Drop);
            self.expression_named(value, target.name)?;
            self.mark(assigned);
        }
        if let Some(reference) = target.reference {
            if !target.operands.is_empty() {
                let value = self.stash()?;
                for local in target.operands.into_iter().rev() {
                    self.get(local);
                }
                self.get(value);
            }
            self.store_reference(reference, init, false);
        } else {
            self.bind_value(target.pattern, init)?;
        }
        Ok(())
    }

    pub(crate) fn bind_value(&mut self, id: PatternId, init: bool) -> Result {
        self.descent(|e| e.bind_value_inner(id, init))
    }

    fn bind_value_inner(&mut self, id: PatternId, init: bool) -> Result {
        use Opcode::*;
        let node = self.ast().pattern(id);
        self.source_loc(node.span);
        match &node.kind {
            Pattern::Object { properties, rest } => {
                let source = self.stash()?;
                self.get(source);
                self.plain(ToObject);
                self.plain(Drop);
                let excluded = if rest.is_some() {
                    self.unsigned(SpecialObject, 7);
                    Some(self.stash()?)
                } else {
                    None
                };
                for property in properties {
                    let atom = self.property_key(&property.key)?;
                    let key = if atom.is_none() {
                        Some(self.stash()?)
                    } else {
                        None
                    };
                    let target = self.prepare_target(property.target, init, false)?;
                    if let Some(excluded) = excluded {
                        self.get(excluded);
                        if let Some(atom) = atom {
                            self.plain(Undefined);
                            self.named(DefineField, atom);
                        } else if let Some(key) = key {
                            self.get(key);
                            self.plain(Undefined);
                            self.plain(DefineArrayEl);
                            self.plain(Drop);
                        }
                        self.plain(Drop);
                    }
                    self.get(source);
                    if let Some(atom) = atom {
                        self.named(GetField, atom);
                    } else if let Some(key) = key {
                        self.get(key);
                        self.plain(GetArrayEl);
                    }
                    self.finish_target(target, init)?;
                }
                if let (Some(rest), Some(excluded)) = (rest, excluded) {
                    let target = self.prepare_target(*rest, init, false)?;
                    self.plain(Object);
                    self.get(source);
                    self.get(excluded);
                    self.unsigned(CopyDataProperties, 6);
                    self.plain(Drop);
                    self.plain(Drop);
                    self.finish_target(target, init)?;
                }
            }
            Pattern::Array { items, rest } => {
                self.plain(ForOfStart);
                self.fm().cleanup.push(Cleanup::CloseIterator(node.span));
                for item in items {
                    let target = item
                        .map(|item| self.prepare_target(item, init, true))
                        .transpose()?;
                    self.unsigned(ForOfNext, 0);
                    self.plain(Drop);
                    if let Some(target) = target {
                        self.finish_target(target, init)?;
                    } else {
                        self.plain(Drop);
                    }
                }
                if let Some(rest) = rest {
                    let target = self.prepare_target(*rest, init, true)?;
                    self.op(ArrayFrom, Operands::Call(0));
                    self.int(0);
                    let next = self.label();
                    let done = self.label();
                    self.mark(next);
                    self.unsigned(ForOfNext, 2);
                    self.if_true(done);
                    self.plain(DefineArrayEl);
                    self.plain(Inc);
                    self.jump(next);
                    self.mark(done);
                    self.plain(Drop);
                    self.plain(Drop);
                    self.finish_target(target, init)?;
                }
                self.fm().cleanup.pop();
                self.source_loc(node.span);
                self.plain(IteratorClose);
            }
            _ => {
                let value = self.stash()?;
                let target = self.prepare_target(id, init, false)?;
                self.get(value);
                self.finish_target(target, init)?;
            }
        }
        Ok(())
    }

    pub(crate) fn declare(&mut self, decl: &Declaration, scope: u16) -> Result {
        let kind = match decl.kind {
            DeclKind::Var => VarKind::Var,
            DeclKind::Let => VarKind::Let,
            DeclKind::Const => VarKind::Const,
            DeclKind::Using => VarKind::Using,
            DeclKind::AwaitUsing => VarKind::AwaitUsing,
        };
        for entry in &decl.entries {
            self.bind_pattern(entry.binding, kind, scope)?;
            if matches!(kind, VarKind::Using | VarKind::AwaitUsing) {
                let name = self.pattern_atom(entry.binding)?.ok_or_else(|| {
                    self.error(
                        self.ast().pattern(entry.binding).span,
                        "Expected a resource binding",
                    )
                })?;
                if !self.f().disposed.contains_key(&(scope, name)) {
                    let resource = self.local(UnitAtom::NONE, VarKind::Using, scope, true)?;
                    self.local(
                        BuiltinAtom::InternalUsingDispose.id(),
                        VarKind::DisposeMethod,
                        scope,
                        false,
                    )?;
                    self.fm()
                        .disposed
                        .insert((scope, name), (resource, kind == VarKind::AwaitUsing));
                }
                let info = &mut self.fm().scopes[usize::from(scope)].info;
                info.has_using |= kind == VarKind::Using;
                info.has_await_using |= kind == VarKind::AwaitUsing;
            }
        }
        Ok(())
    }

    pub(crate) fn dispose_resource(&mut self, binding: PatternId) -> Result {
        let scope = self.f().scope;
        if let Some(name) = self.pattern_atom(binding)?
            && let Some(&(resource, asynchronous)) = self.f().disposed.get(&(scope, name))
        {
            self.unsigned(Opcode::UsingCheck, u32::from(asynchronous));
            self.put(resource + 1);
            self.plain(Opcode::Dup);
            self.put(resource);
        }
        Ok(())
    }

    pub(crate) fn declaration_value(&mut self, decl: &Declaration) -> Result {
        for entry in &decl.entries {
            let name = self.pattern_atom(entry.binding)?;
            let reference = if decl.kind == DeclKind::Var && entry.initializer.is_some() {
                if let Pattern::Identifier(id) = &self.ast().pattern(entry.binding).kind {
                    Some(self.reference_name(id, false)?)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(initializer) = entry.initializer {
                self.expression_named(initializer, name)?;
            } else if decl.kind == DeclKind::Var {
                continue;
            } else {
                self.plain(Opcode::Undefined);
            }
            if matches!(decl.kind, DeclKind::Using | DeclKind::AwaitUsing) {
                self.dispose_resource(entry.binding)?;
            }
            if let Some(reference) = reference {
                self.store_reference(reference, false, false);
            } else {
                self.bind_value(entry.binding, decl.kind != DeclKind::Var)?;
            }
        }
        Ok(())
    }

    pub(crate) fn close_bindings(&mut self, scope: u16) {
        let locals = self.f().scopes[usize::from(scope)].lexical.clone();
        for local in locals {
            self.op(Opcode::CloseLoc, Operands::Local(local));
        }
    }

    pub(crate) fn copy_iteration(&mut self, scope: u16) {
        let locals = self.f().scopes[usize::from(scope)].lexical.clone();
        for local in locals {
            self.get(local);
            self.op(Opcode::CloseLoc, Operands::Local(local));
            self.put(local);
        }
    }

    pub(crate) fn declaration_slot(&self, scope: u16, name: UnitAtom) -> Option<Slot> {
        self.f().scopes[usize::from(scope)]
            .names
            .get(&name)
            .copied()
    }
}
