use super::*;

impl Parser<'_> {
    pub(super) fn expression(&mut self, allow_in: bool) -> Result<ExprId> {
        let start = self.start();
        let first = self.assignment(allow_in)?;
        if !self.eat(",")? {
            return Ok(first);
        }
        let mut list = Vec::new();
        self.push(&mut list, first)?;
        loop {
            let item = self.assignment(allow_in)?;
            self.push(&mut list, item)?;
            if !self.eat(",")? {
                break;
            }
        }
        self.expr(start, Expr::Sequence(list))
    }

    pub(super) fn assignment(&mut self, allow_in: bool) -> Result<ExprId> {
        let old = mem::replace(&mut self.allow_in_arrow, allow_in);
        let result = self.nest(|p| p.assignment_inner(allow_in));
        self.allow_in_arrow = old;
        result
    }

    fn assignment_inner(&mut self, allow_in: bool) -> Result<ExprId> {
        let start = self.start();
        if self.ctx.yield_expr && self.eat_word("yield")? {
            let delegate = !self.current.line_before && self.eat("*")?;
            let argument = if delegate || (!self.current.line_before && self.expression_start()) {
                Some(self.assignment(allow_in)?)
            } else {
                None
            };
            return self.expr(start, Expr::Yield { argument, delegate });
        }
        let mut result = self.binary(1, allow_in)?;
        if self.is_arrow(result) {
            return Ok(result);
        }
        if self.punct("=>")
            && !self.current.line_before
            && matches!(self.arena.expr(result).kind, Expr::Identifier(_))
        {
            return self.arrow(
                start,
                vec![(Item::Value(result), self.arena.expr(result).span)],
                false,
                false,
                allow_in,
            );
        }
        if self.eat("?")? {
            let consequent = self.assignment(true)?;
            self.expect(":")?;
            let alternate = self.assignment(allow_in)?;
            result = self.expr(
                start,
                Expr::Conditional {
                    test: result,
                    consequent,
                    alternate,
                },
            )?;
        }
        if let Some(op) = assignment_op(&self.current) {
            self.next()?;
            let annex_call = !matches!(op, AssignOp::And | AssignOp::Or | AssignOp::Nullish);
            let target = self.assignment_target(result, op == AssignOp::Assign, annex_call)?;
            let value = self.assignment(allow_in)?;
            return self.expr(start, Expr::Assignment { op, target, value });
        }
        Ok(result)
    }

    fn expression_start(&self) -> bool {
        match &self.current.kind {
            TokenKind::Word(w) => {
                w.escaped
                    || !matches!(
                        w.name.as_str(),
                        "in" | "instanceof" | "else" | "case" | "default"
                    )
            }
            TokenKind::Number(_)
            | TokenKind::String(_)
            | TokenKind::Template { .. }
            | TokenKind::Private(_) => true,
            TokenKind::Punct(p) => matches!(
                *p,
                "(" | "[" | "{" | "+" | "-" | "++" | "--" | "~" | "!" | "/" | "/="
            ),
            _ => false,
        }
    }

    fn binary(&mut self, min_prec: u8, allow_in: bool) -> Result<ExprId> {
        let start = self.start();
        let mut left = self.unary()?;
        while let Some((op, prec)) = binary_op(&self.current, allow_in) {
            if prec < min_prec || self.is_arrow(left) {
                break;
            }
            let left_node = self.arena.expr(left);
            if op == BinaryOp::Pow && matches!(left_node.kind, Expr::Unary { .. }) {
                return Err(
                    self.error_at(left_node.span, "Parenthesize unary expression before '**'")
                );
            }
            self.next()?;
            let right = self.nest(|p| p.binary(prec + u8::from(op != BinaryOp::Pow), allow_in))?;
            if self.is_arrow(right) {
                return Err(self.error_at(
                    self.arena.expr(right).span,
                    "Parenthesize arrow function in binary expression",
                ));
            }
            if incompatible(op, &self.arena.expr(left).kind)
                || incompatible(op, &self.arena.expr(right).kind)
            {
                return Err(self.error("Parenthesize '??' when mixing it with '&&' or '||'"));
            }
            left = self.expr(start, Expr::Binary { op, left, right })?;
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<ExprId> {
        let start = self.start();
        let unary = match &self.current.kind {
            TokenKind::Punct("+") => Some(UnaryOp::Plus),
            TokenKind::Punct("-") => Some(UnaryOp::Minus),
            TokenKind::Punct("!") => Some(UnaryOp::Not),
            TokenKind::Punct("~") => Some(UnaryOp::BitNot),
            _ if self.word("delete") => Some(UnaryOp::Delete),
            _ if self.word("void") => Some(UnaryOp::Void),
            _ if self.word("typeof") => Some(UnaryOp::Typeof),
            _ if self.ctx.await_expr && self.word("await") => Some(UnaryOp::Await),
            _ => None,
        };
        if let Some(op) = unary {
            self.next()?;
            let argument = self.nest(Self::unary)?;
            if self.is_arrow(argument) {
                return Err(self.error("Parenthesize arrow function after unary operator"));
            }
            return self.expr(start, Expr::Unary { op, argument });
        }
        if let Some(op) = update_op(&self.current) {
            self.next()?;
            let argument = self.nest(Self::unary)?;
            return self.expr(
                start,
                Expr::Update {
                    op,
                    argument,
                    prefix: true,
                },
            );
        }
        let argument = self.left_hand_side(true)?;
        if !self.current.line_before
            && !self.is_arrow(argument)
            && let Some(op) = update_op(&self.current)
        {
            self.next()?;
            return self.expr(
                start,
                Expr::Update {
                    op,
                    argument,
                    prefix: false,
                },
            );
        }
        Ok(argument)
    }

    pub(super) fn left_hand_side(&mut self, calls: bool) -> Result<ExprId> {
        let start = self.start();
        let mut base = self.primary(calls)?;
        let mut chain = false;
        loop {
            if self.is_arrow(base) {
                break;
            }
            if !calls && self.punct("?.") {
                break;
            }
            let optional = self.eat("?.")?;
            chain |= optional;
            if (optional || calls) && self.punct("(") {
                let async_arrow = !optional
                    && !self.current.line_before
                    && matches!(&self.arena.expr(base).kind, Expr::Identifier(id) if id.name == "async" && !id.escaped);
                let (items, trailing) = self.cover_items()?;
                if async_arrow && self.punct("=>") && !self.current.line_before {
                    return self.arrow(start, items, trailing, true, self.allow_in_arrow);
                }
                let mut arguments = Vec::new();
                for (item, _) in items {
                    self.push(&mut arguments, item)?;
                }
                base = self.expr(
                    start,
                    Expr::Call {
                        callee: base,
                        arguments,
                        optional,
                    },
                )?;
            } else if self.eat("[")? {
                let key = Name::Computed(self.expression(true)?);
                self.expect("]")?;
                base = self.expr(
                    start,
                    Expr::Member {
                        object: base,
                        key,
                        optional,
                    },
                )?;
            } else if optional || self.eat(".")? {
                let key = if matches!(self.current.kind, TokenKind::Private(_)) {
                    Name::Private(self.private()?)
                } else {
                    Name::Ident(self.ident()?)
                };
                base = self.expr(
                    start,
                    Expr::Member {
                        object: base,
                        key,
                        optional,
                    },
                )?;
            } else if matches!(self.current.kind, TokenKind::Template { .. }) {
                if chain {
                    return Err(self.error("Optional chain cannot be used as a template tag"));
                }
                let template = self.template()?;
                base = self.expr(
                    start,
                    Expr::TaggedTemplate {
                        tag: base,
                        template,
                    },
                )?;
            } else {
                break;
            }
        }
        if chain {
            self.expr(start, Expr::Chain(base))
        } else {
            Ok(base)
        }
    }

    fn primary(&mut self, calls: bool) -> Result<ExprId> {
        let start = self.start();
        let kind = match &self.current.kind {
            TokenKind::Punct("[") => self.array()?,
            TokenKind::Punct("{") => self.object()?,
            TokenKind::Punct("(") => {
                let (items, trailing) = self.cover_items()?;
                if self.punct("=>") && !self.current.line_before {
                    return self.arrow(start, items, trailing, false, self.allow_in_arrow);
                }
                if items.is_empty()
                    || trailing
                    || items
                        .iter()
                        .any(|(item, _)| matches!(item, Item::Spread(_)))
                {
                    return Err(self.error("Invalid parenthesized expression"));
                }
                let mut list = Vec::new();
                for (item, _) in items {
                    let Item::Value(id) = item else {
                        unreachable!()
                    };
                    self.push(&mut list, id)?;
                }
                let inner = if list.len() == 1 {
                    list[0]
                } else {
                    self.expr(self.arena.expr(list[0]).span.start, Expr::Sequence(list))?
                };
                Expr::Parenthesized(inner)
            }
            TokenKind::Number(_) => match self.next()?.kind {
                TokenKind::Number(literal) => Expr::Number(literal),
                _ => unreachable!(),
            },
            TokenKind::String(_) => match self.next()?.kind {
                TokenKind::String(literal) => Expr::String(literal),
                _ => unreachable!(),
            },
            TokenKind::Private(_) => Expr::PrivateIdentifier(self.private()?),
            TokenKind::Template { .. } => Expr::Template(self.template()?),
            TokenKind::Punct("/" | "/=") => {
                self.current = self
                    .lexer
                    .at_token(self.current.span, LexicalGoal::RegExp)?;
                match self.next()?.kind {
                    TokenKind::RegExp(literal) => Expr::RegExp(literal),
                    _ => unreachable!(),
                }
            }
            TokenKind::Word(_) if self.word("function") || self.is_async_function()? => {
                Expr::Function(self.function(false)?)
            }
            TokenKind::Word(_) if self.word("class") => Expr::Class(self.class(false)?),
            TokenKind::Word(_) if self.word("this") => {
                self.next()?;
                Expr::This
            }
            TokenKind::Word(_) if self.word("super") => {
                self.next()?;
                Expr::Super
            }
            TokenKind::Word(_) if self.word("null") => {
                self.next()?;
                Expr::Null
            }
            TokenKind::Word(_) if self.word("true") || self.word("false") => {
                let value = self.word("true");
                self.next()?;
                Expr::Boolean(value)
            }
            TokenKind::Word(_) if self.word("new") => {
                self.next()?;
                if self.eat(".")? {
                    self.expect_word("target")?;
                    Expr::NewTarget
                } else {
                    let callee = self.nest(|p| p.left_hand_side(false))?;
                    if self.punct("?.") || self.is_arrow(callee) {
                        return Err(self.error("Invalid constructor expression"));
                    }
                    let mut arguments = Vec::new();
                    if self.punct("(") {
                        for (item, _) in self.cover_items()?.0 {
                            self.push(&mut arguments, item)?;
                        }
                    }
                    Expr::New { callee, arguments }
                }
            }
            TokenKind::Word(_) if self.word("import") => {
                self.next()?;
                if self.eat(".")? {
                    self.expect_word("meta")?;
                    Expr::ImportMeta
                } else {
                    if !calls {
                        return Err(self.error("Dynamic import cannot be a constructor"));
                    }
                    self.expect("(")?;
                    let specifier = self.assignment(true)?;
                    let options = if self.eat(",")? && !self.punct(")") {
                        let options = self.assignment(true)?;
                        self.eat(",")?;
                        Some(options)
                    } else {
                        None
                    };
                    self.expect(")")?;
                    Expr::ImportCall { specifier, options }
                }
            }
            TokenKind::Word(_) => {
                if self.word("async") {
                    let mut look = self.lexer;
                    let binding = look.next(LexicalGoal::Div)?;
                    if !binding.line_before && matches!(binding.kind, TokenKind::Word(_)) {
                        let arrow = look.next(LexicalGoal::Div)?;
                        if !arrow.line_before && arrow.kind == TokenKind::Punct("=>") {
                            self.next()?;
                            let id = self.ident()?;
                            let span = id.span;
                            let parameter = self.expr(span.start, Expr::Identifier(id))?;
                            return self.arrow(
                                start,
                                vec![(Item::Value(parameter), span)],
                                false,
                                true,
                                self.allow_in_arrow,
                            );
                        }
                    }
                }
                Expr::Identifier(self.ident()?)
            }
            _ => return Err(self.error("Expected an expression")),
        };
        self.expr(start, kind)
    }

    pub(super) fn array(&mut self) -> Result<Expr> {
        self.expect("[")?;
        let mut items = Vec::new();
        let mut trailing_comma = false;
        while !self.punct("]") {
            if self.eat(",")? {
                self.push(&mut items, None)?;
                trailing_comma = true;
                continue;
            }
            let spread = self.eat("...")?;
            let expr = self.assignment(true)?;
            self.push(
                &mut items,
                Some(if spread {
                    Item::Spread(expr)
                } else {
                    Item::Value(expr)
                }),
            )?;
            trailing_comma = self.eat(",")?;
            if !trailing_comma {
                break;
            }
        }
        self.expect("]")?;
        Ok(Expr::Array {
            items,
            trailing_comma,
        })
    }

    pub(super) fn object(&mut self) -> Result<Expr> {
        self.expect("{")?;
        let mut properties = Vec::new();
        let mut trailing_comma = false;
        while !self.punct("}") {
            let start = self.start();
            let property = if self.eat("...")? {
                Property::Spread(self.assignment(true)?)
            } else {
                let (key, kind, is_async, is_generator) = self.method_name(false)?;
                if self.punct("(") || kind != MethodKind::Method || is_async || is_generator {
                    let function = self.method(start, is_async, is_generator, false)?;
                    Property::Method {
                        key,
                        kind,
                        function,
                    }
                } else if self.eat(":")? {
                    Property::Value {
                        key,
                        value: self.assignment(true)?,
                    }
                } else if let Name::Ident(key) = key {
                    if self.eat("=")? {
                        Property::CoverInitialized {
                            key,
                            value: self.assignment(true)?,
                        }
                    } else {
                        Property::Shorthand(key)
                    }
                } else {
                    return Err(self.error("Expected ':' or a method body"));
                }
            };
            self.push(&mut properties, property)?;
            trailing_comma = self.eat(",")?;
            if !trailing_comma {
                break;
            }
        }
        self.expect("}")?;
        Ok(Expr::Object {
            properties,
            trailing_comma,
        })
    }

    pub(super) fn method_name(&mut self, class: bool) -> Result<(Name, MethodKind, bool, bool)> {
        let mut is_async = false;
        let mut is_generator = self.eat("*")?;
        let mut kind = MethodKind::Method;
        if !is_generator {
            if self.word("async") {
                let next = self.peek()?;
                if !next.line_before
                    && (next.kind == TokenKind::Punct("*") || property_name_start(&next, class))
                {
                    self.next()?;
                    is_async = true;
                    is_generator = self.eat("*")?;
                }
            } else if self.word("get") || self.word("set") {
                let next = self.peek()?;
                if property_name_start(&next, class) {
                    kind = if self.word("get") {
                        MethodKind::Getter
                    } else {
                        MethodKind::Setter
                    };
                    self.next()?;
                }
            }
        }
        Ok((self.property_name(class)?, kind, is_async, is_generator))
    }

    pub(super) fn property_name(&mut self, private: bool) -> Result<Name> {
        match &self.current.kind {
            TokenKind::Word(_) => Ok(Name::Ident(self.ident()?)),
            TokenKind::Private(_) if private => Ok(Name::Private(self.private()?)),
            TokenKind::String(_) => match self.next()?.kind {
                TokenKind::String(lit) => Ok(Name::String(lit)),
                _ => unreachable!(),
            },
            TokenKind::Number(_) => match self.next()?.kind {
                TokenKind::Number(lit) => Ok(Name::Number(lit)),
                _ => unreachable!(),
            },
            TokenKind::Punct("[") => {
                self.next()?;
                let id = self.assignment(true)?;
                self.expect("]")?;
                Ok(Name::Computed(id))
            }
            _ => Err(self.error("Expected a property name")),
        }
    }

    fn template(&mut self) -> Result<Template> {
        let mut elements = Vec::new();
        let mut substitutions = Vec::new();
        loop {
            let TokenKind::Template { element, tail } = self.next()?.kind else {
                return Err(self.error("Expected a template element"));
            };
            self.push(&mut elements, element)?;
            if tail {
                break;
            }
            let expr = self.expression(true)?;
            self.push(&mut substitutions, expr)?;
            if !self.punct("}") {
                return Err(self.error("Expected '}' in template literal"));
            }
            self.current = self
                .lexer
                .at_token(self.current.span, LexicalGoal::TemplateTail)?;
        }
        Ok(Template {
            elements,
            substitutions,
        })
    }

    pub(super) fn is_arrow(&self, id: ExprId) -> bool {
        matches!(&self.arena.expr(id).kind, Expr::Function(f) if f.is_arrow)
    }
}

fn incompatible(op: BinaryOp, expr: &Expr) -> bool {
    let Expr::Binary { op: sub, .. } = expr else {
        return false;
    };
    matches!(
        (op, sub),
        (BinaryOp::Nullish, BinaryOp::And | BinaryOp::Or)
            | (BinaryOp::And | BinaryOp::Or, BinaryOp::Nullish)
    )
}

fn binary_op(token: &Token, allow_in: bool) -> Option<(BinaryOp, u8)> {
    use BinaryOp::*;
    Some(match &token.kind {
        TokenKind::Punct("??") => (Nullish, 1),
        TokenKind::Punct("||") => (Or, 1),
        TokenKind::Punct("&&") => (And, 2),
        TokenKind::Punct("|") => (BitOr, 3),
        TokenKind::Punct("^") => (BitXor, 4),
        TokenKind::Punct("&") => (BitAnd, 5),
        TokenKind::Punct("==") => (Eq, 6),
        TokenKind::Punct("!=") => (Ne, 6),
        TokenKind::Punct("===") => (StrictEq, 6),
        TokenKind::Punct("!==") => (StrictNe, 6),
        TokenKind::Punct("<") => (Lt, 7),
        TokenKind::Punct("<=") => (Le, 7),
        TokenKind::Punct(">") => (Gt, 7),
        TokenKind::Punct(">=") => (Ge, 7),
        _ if allow_in && word_is(token, "in") => (In, 7),
        _ if word_is(token, "instanceof") => (Instanceof, 7),
        TokenKind::Punct("<<") => (Shl, 8),
        TokenKind::Punct(">>") => (Shr, 8),
        TokenKind::Punct(">>>") => (UShr, 8),
        TokenKind::Punct("+") => (Add, 9),
        TokenKind::Punct("-") => (Sub, 9),
        TokenKind::Punct("*") => (Mul, 10),
        TokenKind::Punct("/") => (Div, 10),
        TokenKind::Punct("%") => (Rem, 10),
        TokenKind::Punct("**") => (Pow, 11),
        _ => return None,
    })
}

fn update_op(token: &Token) -> Option<UpdateOp> {
    match token.kind {
        TokenKind::Punct("++") => Some(UpdateOp::Inc),
        TokenKind::Punct("--") => Some(UpdateOp::Dec),
        _ => None,
    }
}

fn assignment_op(token: &Token) -> Option<AssignOp> {
    use AssignOp::*;
    Some(match token.kind {
        TokenKind::Punct("=") => Assign,
        TokenKind::Punct("+=") => Add,
        TokenKind::Punct("-=") => Sub,
        TokenKind::Punct("*=") => Mul,
        TokenKind::Punct("/=") => Div,
        TokenKind::Punct("%=") => Rem,
        TokenKind::Punct("**=") => Pow,
        TokenKind::Punct("<<=") => Shl,
        TokenKind::Punct(">>=") => Shr,
        TokenKind::Punct(">>>=") => UShr,
        TokenKind::Punct("&=") => BitAnd,
        TokenKind::Punct("^=") => BitXor,
        TokenKind::Punct("|=") => BitOr,
        TokenKind::Punct("&&=") => And,
        TokenKind::Punct("||=") => Or,
        TokenKind::Punct("??=") => Nullish,
        _ => return None,
    })
}
