use super::*;

impl Parser<'_> {
    pub(super) fn import(&mut self) -> Result<ImportDeclaration> {
        self.expect_word("import")?;
        let mut bindings = Vec::new();
        if !matches!(self.current.kind, TokenKind::String(_)) {
            let mut more = true;
            if matches!(self.current.kind, TokenKind::Word(_)) {
                let local = self.ident()?;
                self.push(&mut bindings, ImportBinding::Default(local))?;
                more = self.eat(",")?;
            }
            if more {
                if self.eat("*")? {
                    self.expect_word("as")?;
                    let local = self.ident()?;
                    self.push(&mut bindings, ImportBinding::Namespace(local))?;
                } else {
                    self.expect("{")?;
                    while !self.punct("}") {
                        let first_span = self.current.span;
                        let imported = self.module_name(true)?;
                        let local = if self.eat_word("as")? {
                            self.ident()?
                        } else if let ModuleName::Identifier(name) = &imported {
                            Ident {
                                span: first_span,
                                name: name.clone(),
                                escaped: self.source.slice(first_span).contains(&b'\\'),
                            }
                        } else {
                            return Err(self.error("String import name requires an alias"));
                        };
                        self.push(&mut bindings, ImportBinding::Named { imported, local })?;
                        if !self.eat(",")? {
                            break;
                        }
                    }
                    self.expect("}")?;
                }
            }
            self.expect_word("from")?;
        }
        let request = self.module_request()?;
        self.asi()?;
        Ok(ImportDeclaration { bindings, request })
    }

    pub(super) fn export(&mut self) -> Result<ExportDeclaration> {
        self.expect_word("export")?;
        let start = self.start();
        if self.eat_word("default")? {
            let start = self.start();
            if self.word("function") || self.is_async_function()? {
                let kind = Stmt::Function(self.function(false)?);
                return Ok(ExportDeclaration::DefaultDeclaration(
                    self.stmt(start, kind)?,
                ));
            }
            if self.word("class") {
                let kind = Stmt::Class(self.class(false)?);
                return Ok(ExportDeclaration::DefaultDeclaration(
                    self.stmt(start, kind)?,
                ));
            }
            let expr = self.assignment(true)?;
            self.asi()?;
            return Ok(ExportDeclaration::DefaultExpression(expr));
        }
        if self.eat("*")? {
            let alias = if self.eat_word("as")? {
                Some(self.module_name(true)?)
            } else {
                None
            };
            self.expect_word("from")?;
            let request = self.module_request()?;
            self.asi()?;
            return Ok(ExportDeclaration::All { alias, request });
        }
        if self.eat("{")? {
            let mut specifiers = Vec::new();
            while !self.punct("}") {
                let local = self.module_name(true)?;
                let exported = if self.eat_word("as")? {
                    self.module_name(true)?
                } else {
                    local.clone()
                };
                self.push(&mut specifiers, ExportSpecifier { local, exported })?;
                if !self.eat(",")? {
                    break;
                }
            }
            self.expect("}")?;
            let request = if self.eat_word("from")? {
                Some(self.module_request()?)
            } else {
                None
            };
            if request.is_none()
                && specifiers
                    .iter()
                    .any(|s| matches!(s.local, ModuleName::String(_)))
            {
                return Err(self.error("Local export name must be an identifier"));
            }
            self.asi()?;
            return Ok(ExportDeclaration::Named {
                specifiers,
                request,
            });
        }
        let kind = if self.word("function") || self.is_async_function()? {
            Stmt::Function(self.function(true)?)
        } else if self.word("class") {
            Stmt::Class(self.class(true)?)
        } else if word_is_one(&self.current, &["let", "const", "var"]) {
            let decl = if self.word("var") {
                DeclKind::Var
            } else if self.word("let") {
                DeclKind::Let
            } else {
                DeclKind::Const
            };
            let declaration = self.declaration(decl, true)?;
            self.asi()?;
            Stmt::Declaration(declaration)
        } else {
            return Err(self.error("Expected an export declaration"));
        };
        Ok(ExportDeclaration::Declaration(self.stmt(start, kind)?))
    }

    fn module_request(&mut self) -> Result<ModuleRequest> {
        let specifier = self.module_string()?;
        let mut attributes = Vec::new();
        if self.eat_word("with")? {
            self.expect("{")?;
            while !self.punct("}") {
                let key = self.module_name(false)?;
                self.expect(":")?;
                let value = self.module_string()?;
                self.push(&mut attributes, ImportAttribute { key, value })?;
                if !self.eat(",")? {
                    break;
                }
            }
            self.expect("}")?;
        }
        Ok(ModuleRequest {
            specifier,
            attributes,
        })
    }

    fn module_name(&mut self, well_formed: bool) -> Result<ModuleName> {
        if matches!(self.current.kind, TokenKind::String(_)) {
            let span = self.current.span;
            let value = self.module_string()?;
            if well_formed && char::decode_utf16(value.units()).any(|c| c.is_err()) {
                return Err(
                    self.error_at(span, "Module export name contains an unpaired surrogate")
                );
            }
            Ok(ModuleName::String(value))
        } else {
            Ok(ModuleName::Identifier(self.ident()?.name))
        }
    }

    fn module_string(&mut self) -> Result<JsStrLit> {
        let span = self.current.span;
        match self.next()?.kind {
            TokenKind::String(lit) if !lit.legacy_escape => Ok(lit.value),
            _ => Err(self.error_at(span, "Expected a module string literal")),
        }
    }
}
