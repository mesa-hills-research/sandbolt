use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Root,
    List,
    Case,
    Embedded,
    Branch,
    Label,
    EmbeddedLabel,
}

impl Parser<'_> {
    pub(super) fn stmt_list(
        &mut self,
        block: bool,
        directives: bool,
    ) -> Result<(Vec<StmtId>, bool)> {
        let mut list = Vec::new();
        let mut prologue = directives;
        let mut strict_directive = false;
        while !(self.eof() || block && self.punct("}")) {
            let position = if block {
                Position::List
            } else {
                Position::Root
            };
            let stmt = self.statement(position)?;
            if prologue {
                if let Stmt::Expression(expr) = self.arena.stmt(stmt).kind
                    && let Expr::String(value) = &self.arena.expr(expr).kind
                {
                    if !value.escaped
                        && matches!(&value.value, JsStrLit::Latin1(value) if value == b"use strict")
                    {
                        self.ctx.strict = true;
                        strict_directive = true;
                    }
                } else {
                    prologue = false;
                }
            }
            self.push(&mut list, stmt)?;
        }
        Ok((list, strict_directive))
    }

    fn statement(&mut self, pos: Position) -> Result<StmtId> {
        self.nest(|p| p.statement_inner(pos))
    }

    fn statement_inner(&mut self, pos: Position) -> Result<StmtId> {
        let start = self.start();
        let list = matches!(pos, Position::Root | Position::List | Position::Case);
        let kind = if self.eat(";")? {
            Stmt::Empty
        } else if self.eat("{")? {
            let (body, _) = self.stmt_list(true, false)?;
            self.expect("}")?;
            Stmt::Block(body)
        } else if self.word("function") || self.is_async_function()? {
            if !list
                && (!matches!(pos, Position::Branch | Position::Label)
                    || !self.opts.annex_b
                    || self.ctx.strict)
            {
                return Err(self.error("Function declaration requires a statement list"));
            }
            let f = self.function(true)?;
            if !list && (f.is_generator || f.is_async) {
                return Err(self.error_at(f.span, "Expected an ordinary function declaration"));
            }
            Stmt::Function(f)
        } else if self.word("class") {
            if !list {
                return Err(self.error("Class declaration requires a statement list"));
            }
            Stmt::Class(self.class(true)?)
        } else if let Some(decl) = self.declaration_kind(false, list)? {
            if !list && decl != DeclKind::Var {
                return Err(self.error("Lexical declaration requires a statement list"));
            }
            if matches!(decl, DeclKind::Using | DeclKind::AwaitUsing)
                && (pos == Position::Case
                    || (pos == Position::Root
                        && matches!(
                            self.opts.goal,
                            ParseGoal::Script | ParseGoal::DirectEval { .. }
                        )))
            {
                return Err(
                    self.error("Resource declaration needs a block, function, loop, or module")
                );
            }
            let declaration = self.declaration(decl, true)?;
            self.asi()?;
            Stmt::Declaration(declaration)
        } else if self.eat_word("if")? {
            let test = self.paren_expression()?;
            let consequent = self.statement(Position::Branch)?;
            let alternate = if self.eat_word("else")? {
                Some(self.statement(Position::Branch)?)
            } else {
                None
            };
            Stmt::If {
                test,
                consequent,
                alternate,
            }
        } else if self.eat_word("while")? {
            let test = self.paren_expression()?;
            let body = self.statement(Position::Embedded)?;
            Stmt::While { test, body }
        } else if self.eat_word("do")? {
            let body = self.statement(Position::Embedded)?;
            self.expect_word("while")?;
            let test = self.paren_expression()?;
            self.eat(";")?;
            Stmt::DoWhile { body, test }
        } else if self.eat_word("for")? {
            self.for_statement()?
        } else if self.eat_word("switch")? {
            let discriminant = self.paren_expression()?;
            self.expect("{")?;
            let mut cases = Vec::new();
            let mut default = false;
            while !self.punct("}") {
                let test = if self.eat_word("case")? {
                    Some(self.expression(true)?)
                } else if self.eat_word("default")? {
                    if default {
                        return Err(self.error("Duplicate switch default"));
                    }
                    default = true;
                    None
                } else {
                    return Err(self.error("Expected 'case' or 'default'"));
                };
                self.expect(":")?;
                let mut body = Vec::new();
                while !self.punct("}") && !self.word("case") && !self.word("default") {
                    let stmt = self.statement(Position::Case)?;
                    self.push(&mut body, stmt)?;
                }
                self.push(&mut cases, SwitchCase { test, body })?;
            }
            self.expect("}")?;
            Stmt::Switch {
                discriminant,
                cases,
            }
        } else if self.eat_word("try")? {
            let body = self.block_statement()?;
            let catch = if self.eat_word("catch")? {
                let binding = if self.eat("(")? {
                    let binding = self.binding()?;
                    self.expect(")")?;
                    Some(binding)
                } else {
                    None
                };
                Some(Catch {
                    binding,
                    body: self.block_statement()?,
                })
            } else {
                None
            };
            let finally = if self.eat_word("finally")? {
                Some(self.block_statement()?)
            } else {
                None
            };
            if catch.is_none() && finally.is_none() {
                return Err(self.error("Expected 'catch' or 'finally'"));
            }
            Stmt::Try {
                body,
                catch,
                finally,
            }
        } else if self.eat_word("with")? {
            let object = self.paren_expression()?;
            let body = self.statement(Position::Embedded)?;
            Stmt::With { object, body }
        } else if self.word("break") || self.word("continue") {
            let is_break = self.word("break");
            self.next()?;
            let label =
                if !self.current.line_before && matches!(self.current.kind, TokenKind::Word(_)) {
                    Some(self.ident()?)
                } else {
                    None
                };
            self.asi()?;
            if is_break {
                Stmt::Break(label)
            } else {
                Stmt::Continue(label)
            }
        } else if self.eat_word("return")? {
            let value =
                if self.current.line_before || self.punct(";") || self.punct("}") || self.eof() {
                    None
                } else {
                    Some(self.expression(true)?)
                };
            self.asi()?;
            Stmt::Return(value)
        } else if self.eat_word("throw")? {
            if self.current.line_before {
                return Err(self.error("Line terminator after 'throw'"));
            }
            let argument = self.expression(true)?;
            self.asi()?;
            Stmt::Throw(argument)
        } else if self.eat_word("debugger")? {
            self.asi()?;
            Stmt::Debugger
        } else if self.word("export") {
            if pos != Position::Root || self.opts.goal != ParseGoal::Module {
                return Err(self.error("Export declaration requires module top level"));
            }
            Stmt::Export(self.export()?)
        } else if self.word("import") && !matches!(self.peek()?.kind, TokenKind::Punct("(" | ".")) {
            if pos != Position::Root || self.opts.goal != ParseGoal::Module {
                return Err(self.error("Import declaration requires module top level"));
            }
            Stmt::Import(self.import()?)
        } else if matches!(self.current.kind, TokenKind::Word(_))
            && self.peek()?.kind == TokenKind::Punct(":")
        {
            let name = self.ident()?;
            self.expect(":")?;
            let label_pos = if list || pos == Position::Label {
                Position::Label
            } else {
                Position::EmbeddedLabel
            };
            Stmt::Label {
                name,
                body: self.statement(label_pos)?,
            }
        } else {
            if !list && self.word("let") && self.peek()?.kind == TokenKind::Punct("[") {
                return Err(self.error("Statement cannot begin with 'let ['"));
            }
            let expr = self.expression(true)?;
            self.asi()?;
            Stmt::Expression(expr)
        };
        self.stmt(start, kind)
    }

    fn block_statement(&mut self) -> Result<StmtId> {
        let start = self.start();
        self.expect("{")?;
        let (body, _) = self.stmt_list(true, false)?;
        self.expect("}")?;
        self.stmt(start, Stmt::Block(body))
    }

    fn paren_expression(&mut self) -> Result<ExprId> {
        self.expect("(")?;
        let result = self.expression(true)?;
        self.expect(")")?;
        Ok(result)
    }

    fn declaration_kind(&self, for_head: bool, contextual: bool) -> Result<Option<DeclKind>> {
        if self.word("var") {
            return Ok(Some(DeclKind::Var));
        }
        if self.word("const") {
            return Ok(Some(DeclKind::Const));
        }
        if !contextual {
            return Ok(None);
        }
        if self.word("let") {
            let next = self.peek()?;
            if matches!(next.kind, TokenKind::Word(_) | TokenKind::Punct("{" | "["))
                && !word_is_one(&next, &["in", "instanceof"])
            {
                return Ok(Some(DeclKind::Let));
            }
        }
        if self.word("using") || (self.ctx.await_expr && self.word("await")) {
            let mut look = self.lexer;
            let mut next = look.next(LexicalGoal::Div)?;
            let is_await = self.word("await");
            if is_await {
                if next.line_before || !word_is(&next, "using") {
                    return Ok(None);
                }
                next = look.next(LexicalGoal::Div)?;
            }
            if !next.line_before
                && matches!(next.kind, TokenKind::Word(_))
                && !word_is_one(&next, &["in", "instanceof"])
            {
                if for_head
                    && word_is(&next, "of")
                    && !is_await
                    && look.next(LexicalGoal::Div)?.kind != TokenKind::Punct("=")
                {
                    return Ok(None);
                }
                return Ok(Some(if is_await {
                    DeclKind::AwaitUsing
                } else {
                    DeclKind::Using
                }));
            }
        }
        Ok(None)
    }

    pub(super) fn declaration(&mut self, kind: DeclKind, allow_in: bool) -> Result<Declaration> {
        self.next()?;
        if kind == DeclKind::AwaitUsing {
            self.expect_word("using")?;
        }
        let mut entries = Vec::new();
        loop {
            let binding = self.binding()?;
            let initializer = if self.eat("=")? {
                Some(self.assignment(allow_in)?)
            } else {
                None
            };
            self.push(
                &mut entries,
                Declarator {
                    binding,
                    initializer,
                },
            )?;
            if !self.eat(",")? {
                break;
            }
        }
        Ok(Declaration { kind, entries })
    }

    fn for_statement(&mut self) -> Result<Stmt> {
        let async_loop = self.eat_word("await")?;
        self.expect("(")?;
        let let_head = self.word("let");
        let async_head = self.word("async");
        let init = if self.punct(";") {
            None
        } else if let Some(decl) = self.declaration_kind(true, true)? {
            Some(ForInit::Declaration(self.declaration(decl, false)?))
        } else {
            Some(ForInit::Expression(self.expression(false)?))
        };
        let is_in = self.word("in");
        if is_in || self.word("of") {
            if async_loop && is_in {
                return Err(self.error("Expected 'of' in for-await loop"));
            }
            let kind = if is_in {
                ForKind::In
            } else if async_loop {
                ForKind::AwaitOf
            } else {
                ForKind::Of
            };
            self.next()?;
            let target = match init {
                Some(ForInit::Expression(id)) => {
                    if !is_in
                        && (let_head
                            || (async_head
                                && !async_loop
                                && matches!(&self.arena.expr(id).kind, Expr::Identifier(i) if i.name == "async")))
                    {
                        return Err(self.error("Invalid for-of loop head"));
                    }
                    ForTarget::Pattern(self.assignment_target(id, true, true)?)
                }
                Some(ForInit::Declaration(decl)) => ForTarget::Declaration(decl),
                None => return Err(self.error("Missing for-loop target")),
            };
            let iterable = if is_in {
                self.expression(true)?
            } else {
                self.assignment(true)?
            };
            self.expect(")")?;
            let body = self.statement(Position::Embedded)?;
            return Ok(Stmt::ForEach {
                kind,
                target,
                iterable,
                body,
            });
        }
        if async_loop {
            return Err(self.error("Expected 'of' in for-await loop"));
        }
        self.expect(";")?;
        let test = if self.punct(";") {
            None
        } else {
            Some(self.expression(true)?)
        };
        self.expect(";")?;
        let update = if self.punct(")") {
            None
        } else {
            Some(self.expression(true)?)
        };
        self.expect(")")?;
        let body = self.statement(Position::Embedded)?;
        Ok(Stmt::For {
            init,
            test,
            update,
            body,
        })
    }

    pub(super) fn is_async_function(&self) -> Result<bool> {
        if !self.word("async") {
            return Ok(false);
        }
        let next = self.peek()?;
        Ok(!next.line_before && word_is(&next, "function"))
    }

    pub(super) fn function(&mut self, named: bool) -> Result<Function> {
        let start = self.start();
        let is_async = self.eat_word("async")?;
        self.expect_word("function")?;
        let is_generator = self.eat("*")?;
        let name = if named || !self.punct("(") {
            Some(self.ident()?)
        } else {
            None
        };
        let mut function = self.method(start, is_async, is_generator, false)?;
        function.name = name;
        Ok(function)
    }

    pub(super) fn method(
        &mut self,
        start: u32,
        is_async: bool,
        is_generator: bool,
        strict: bool,
    ) -> Result<Function> {
        let outer = self.ctx;
        self.ctx = Context {
            strict: outer.strict || strict,
            await_expr: false,
            yield_expr: false,
        };
        let (items, trailing) = self.cover_items()?;
        let params = self.parameters(items, trailing)?;
        self.ctx.await_expr = is_async;
        self.ctx.yield_expr = is_generator;
        self.expect("{")?;
        let (body, use_strict) = self.stmt_list(true, true)?;
        self.expect("}")?;
        let strict = self.ctx.strict;
        self.ctx = outer;
        Ok(Function {
            span: self.span(start),
            name: None,
            params,
            body: FunctionBody::Block(body),
            is_async,
            is_generator,
            is_arrow: false,
            strict,
            use_strict,
        })
    }

    pub(super) fn class(&mut self, named: bool) -> Result<Class> {
        self.expect_word("class")?;
        let outer = self.ctx;
        self.ctx.strict = true;
        let name = if named || (!self.punct("{") && !self.word("extends")) {
            Some(self.ident()?)
        } else {
            None
        };
        let extends = if self.eat_word("extends")? {
            let heritage = self.left_hand_side(true)?;
            if self.is_arrow(heritage) {
                return Err(self.error_at(
                    self.arena.expr(heritage).span,
                    "Parenthesize arrow function in class heritage",
                ));
            }
            Some(heritage)
        } else {
            None
        };
        self.expect("{")?;
        let mut elements = Vec::new();
        while !self.eat("}")? {
            if self.eat(";")? {
                continue;
            }
            let mut is_static = false;
            if self.word("static") {
                let next = self.peek()?;
                if next.kind == TokenKind::Punct("{") {
                    self.next()?;
                    self.next()?;
                    let context = self.ctx;
                    self.ctx.await_expr = false;
                    self.ctx.yield_expr = false;
                    let (body, _) = self.stmt_list(true, false)?;
                    self.expect("}")?;
                    self.ctx = context;
                    self.push(&mut elements, ClassElement::StaticBlock(body))?;
                    continue;
                }
                if next.kind == TokenKind::Punct("*") || property_name_start(&next, true) {
                    self.next()?;
                    is_static = true;
                }
            }
            let start = self.start();
            let (key, mut kind, is_async, is_generator) = self.method_name(true)?;
            let element =
                if self.punct("(") || kind != MethodKind::Method || is_async || is_generator {
                    let function = self.method(start, is_async, is_generator, true)?;
                    if !is_static && name_is(&key, "constructor") {
                        if kind != MethodKind::Method || is_async || is_generator {
                            return Err(self.error("Constructor must be an ordinary method"));
                        }
                        kind = MethodKind::Constructor;
                    }
                    ClassElement::Method {
                        key,
                        function,
                        kind,
                        is_static,
                    }
                } else {
                    let initializer = if self.eat("=")? {
                        let ctx = self.ctx;
                        self.ctx.await_expr = false;
                        self.ctx.yield_expr = false;
                        let expr = self.assignment(true)?;
                        self.ctx = ctx;
                        Some(expr)
                    } else {
                        None
                    };
                    self.asi()?;
                    ClassElement::Field {
                        key,
                        initializer,
                        is_static,
                    }
                };
            self.push(&mut elements, element)?;
        }
        self.ctx = outer;
        Ok(Class {
            name,
            extends,
            elements,
        })
    }
}

pub(crate) fn name_is(name: &Name, text: &str) -> bool {
    match name {
        Name::Ident(id) => id.name == text,
        Name::String(s) => s.value.units().eq(text.encode_utf16()),
        _ => false,
    }
}
