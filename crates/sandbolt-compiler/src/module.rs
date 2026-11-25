use crate::{
    CompileError,
    emit::{Emitter, Result, Slot, string},
};
use sandbolt_bytecode::{
    BuiltinAtom, ClosureKind, ExportEntry, ImportAttribute, ImportEntry, RequestedModule, UnitAtom,
    VarKind,
};
use sandbolt_syntax::{ExportDeclaration, ImportBinding, ModuleName, ModuleRequest, Stmt, StmtId};

impl Emitter<'_> {
    fn module_name(&mut self, name: &ModuleName) -> Result<UnitAtom> {
        match name {
            ModuleName::Identifier(name) => self.atom(name),
            ModuleName::String(value) => self.atom_string(string(value)),
        }
    }

    fn request(&mut self, request: &ModuleRequest) -> Result<u32> {
        let name = self.atom_string(string(&request.specifier))?;
        let mut attributes = Vec::new();
        for attribute in &request.attributes {
            let key = self.module_name(&attribute.key)?;
            let value = self.atom_string(string(&attribute.value))?;
            attributes.push(ImportAttribute { key, value });
        }
        attributes.sort_unstable_by_key(|a| a.key.raw());
        let module = self.fm().module.as_mut().expect("module entry function");
        let request = RequestedModule { name, attributes };
        if let Some(i) = module.requested_modules.iter().position(|m| *m == request) {
            return Ok(i as u32);
        }
        let i =
            u32::try_from(module.requested_modules.len()).map_err(|_| CompileError::OutOfMemory)?;
        module.requested_modules.push(request);
        Ok(i)
    }

    pub(crate) fn module_entries(&mut self, body: &[StmtId]) -> Result {
        if self.f().module.is_none() {
            return Ok(());
        }
        for &id in body {
            match &self.ast().stmt(id).kind {
                Stmt::Import(import) => {
                    let requested_module = self.request(&import.request)?;
                    for binding in &import.bindings {
                        let (name, import_name) = match binding {
                            ImportBinding::Default(local) => {
                                (local, Some(BuiltinAtom::Default.id()))
                            }
                            ImportBinding::Namespace(local) => (local, None),
                            ImportBinding::Named { imported, local } => {
                                (local, Some(self.module_name(imported)?))
                            }
                        };
                        let name = self.atom(&name.name)?;
                        let Slot::Capture(closure_var) = self.bind(name, VarKind::Const, 0)? else {
                            unreachable!()
                        };
                        self.fm().bytecode.closure_vars[usize::from(closure_var)].kind =
                            ClosureKind::ModuleImport;
                        self.fm()
                            .module
                            .as_mut()
                            .expect("module entry function")
                            .imports
                            .push(ImportEntry {
                                requested_module,
                                import_name,
                                closure_var,
                            });
                    }
                }
                Stmt::Export(
                    ExportDeclaration::Named {
                        request: Some(request),
                        ..
                    }
                    | ExportDeclaration::All { request, .. },
                ) => {
                    self.request(request)?;
                }
                _ => (),
            }
        }
        for &id in body {
            let Stmt::Export(export) = &self.ast().stmt(id).kind else {
                continue;
            };
            match export {
                ExportDeclaration::Named {
                    specifiers,
                    request,
                } => {
                    let requested_module = request.as_ref().map(|m| self.request(m)).transpose()?;
                    for s in specifiers {
                        let local = self.module_name(&s.local)?;
                        let exported = self.module_name(&s.exported)?;
                        if let Some(requested_module) = requested_module {
                            self.add_export(ExportEntry::Indirect {
                                export_name: exported,
                                requested_module,
                                import_name: Some(local),
                            });
                        } else {
                            self.local_export(local, exported, id)?;
                        }
                    }
                }
                ExportDeclaration::Declaration(decl) => {
                    let mut names = Vec::new();
                    match &self.ast().stmt(*decl).kind {
                        Stmt::Declaration(d) => {
                            for entry in &d.entries {
                                self.pattern_atoms(entry.binding, &mut names)?;
                            }
                        }
                        Stmt::Function(function) => {
                            if let Some(name) = &function.name {
                                names.push(self.atom(&name.name)?);
                            }
                        }
                        Stmt::Class(class) => {
                            if let Some(name) = &class.name {
                                names.push(self.atom(&name.name)?);
                            }
                        }
                        _ => (),
                    }
                    for name in names {
                        self.local_export(name, name, id)?;
                    }
                }
                ExportDeclaration::DefaultDeclaration(decl) => {
                    let name = match &self.ast().stmt(*decl).kind {
                        Stmt::Function(f) => f.name.as_ref(),
                        Stmt::Class(c) => c.name.as_ref(),
                        _ => None,
                    };
                    let local = name
                        .map(|name| self.atom(&name.name))
                        .transpose()?
                        .unwrap_or(BuiltinAtom::DefaultExport.id());
                    self.local_export(local, BuiltinAtom::Default.id(), id)?;
                }
                ExportDeclaration::DefaultExpression(_) => self.local_export(
                    BuiltinAtom::DefaultExport.id(),
                    BuiltinAtom::Default.id(),
                    id,
                )?,
                ExportDeclaration::All { alias, request } => {
                    let requested_module = self.request(request)?;
                    if let Some(alias) = alias {
                        let export_name = self.module_name(alias)?;
                        self.add_export(ExportEntry::Indirect {
                            export_name,
                            requested_module,
                            import_name: None,
                        });
                    } else {
                        self.add_export(ExportEntry::Star { requested_module });
                    }
                }
            }
        }
        Ok(())
    }

    fn local_export(&mut self, name: UnitAtom, export_name: UnitAtom, at: StmtId) -> Result {
        let Some(Slot::Capture(closure_var)) = self.declaration_slot(0, name) else {
            return Err(self.error(self.ast().stmt(at).span, "Export has no local binding"));
        };
        let imports = &self
            .f()
            .module
            .as_ref()
            .expect("module entry function")
            .imports;
        if let Some(entry) = imports
            .iter()
            .find(|entry| entry.closure_var == closure_var && entry.import_name.is_some())
        {
            self.add_export(ExportEntry::Indirect {
                export_name,
                requested_module: entry.requested_module,
                import_name: entry.import_name,
            });
        } else {
            self.add_export(ExportEntry::Local {
                export_name,
                closure_var,
            });
        }
        Ok(())
    }

    fn add_export(&mut self, export: ExportEntry) {
        self.fm()
            .module
            .as_mut()
            .expect("module entry function")
            .exports
            .push(export);
    }
}
