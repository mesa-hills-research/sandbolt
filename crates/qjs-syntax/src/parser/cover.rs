use super::*;

impl Parser<'_> {
    pub(super) fn cover_items(&mut self) -> Result<(Vec<(Item, Span)>, bool)> {
        self.expect("(")?;
        let mut items = Vec::new();
        let mut trailing = false;
        while !self.punct(")") {
            let start = self.start();
            let spread = self.eat("...")?;
            let expr = self.assignment(true)?;
            self.push(
                &mut items,
                (
                    if spread {
                        Item::Spread(expr)
                    } else {
                        Item::Value(expr)
                    },
                    self.span(start),
                ),
            )?;
            trailing = self.eat(",")?;
            if !trailing {
                break;
            }
        }
        self.expect(")")?;
        Ok((items, trailing))
    }

    pub(super) fn parameters(
        &mut self,
        items: Vec<(Item, Span)>,
        trailing: bool,
    ) -> Result<Vec<PatternId>> {
        let mut params = Vec::new();
        let len = items.len();
        for (i, (item, span)) in items.into_iter().enumerate() {
            let pattern = match item {
                Item::Spread(id) => {
                    if i + 1 != len || trailing {
                        return Err(self.error_at(span, "Rest parameter must be last"));
                    }
                    let pat = self.reinterpret_pattern(id, true, false, false)?;
                    self.pattern(span, Pattern::Rest(pat))?
                }
                Item::Value(id) => self.reinterpret_pattern(id, true, true, false)?,
            };
            self.push(&mut params, pattern)?;
        }
        Ok(params)
    }

    pub(super) fn arrow(
        &mut self,
        start: u32,
        items: Vec<(Item, Span)>,
        trailing: bool,
        is_async: bool,
        allow_in: bool,
    ) -> Result<ExprId> {
        self.expect("=>")?;
        let params = self.parameters(items, trailing)?;
        let outer = self.ctx;
        self.ctx.await_expr = is_async;
        self.ctx.yield_expr = false;
        let (body, use_strict) = if self.eat("{")? {
            let (body, strict) = self.stmt_list(true, true)?;
            self.expect("}")?;
            (FunctionBody::Block(body), strict)
        } else {
            (FunctionBody::Expression(self.assignment(allow_in)?), false)
        };
        let strict = self.ctx.strict;
        self.ctx = outer;
        let function = Function {
            span: self.span(start),
            name: None,
            params,
            body,
            is_async,
            is_generator: false,
            is_arrow: true,
            strict,
            use_strict,
        };
        self.expr(start, Expr::Function(function))
    }

    pub(super) fn binding(&mut self) -> Result<PatternId> {
        let start = self.start();
        if !self.punct("[") && !self.punct("{") {
            let id = self.ident()?;
            return self.pattern(id.span, Pattern::Identifier(id));
        }
        let kind = if self.punct("[") {
            self.array()?
        } else {
            self.object()?
        };
        let id = self.expr(start, kind)?;
        self.reinterpret_pattern(id, true, false, false)
    }

    pub(super) fn assignment_target(
        &mut self,
        id: ExprId,
        destructuring: bool,
        annex_call: bool,
    ) -> Result<PatternId> {
        if !destructuring
            && matches!(
                self.arena.expr(id).kind,
                Expr::Array { .. } | Expr::Object { .. }
            )
        {
            return Err(self.error_at(
                self.arena.expr(id).span,
                "Compound assignment requires a simple target",
            ));
        }
        self.reinterpret_pattern(id, false, false, annex_call)
    }

    fn reinterpret_pattern(
        &mut self,
        id: ExprId,
        binding: bool,
        default: bool,
        annex_call: bool,
    ) -> Result<PatternId> {
        self.nest(|p| p.reinterpret_pattern_inner(id, binding, default, annex_call))
    }

    fn reinterpret_pattern_inner(
        &mut self,
        id: ExprId,
        binding: bool,
        default: bool,
        annex_call: bool,
    ) -> Result<PatternId> {
        let span = self.arena.expr(id).span;
        let kind = match self.arena.expr(id).kind.clone() {
            Expr::Identifier(id) => Pattern::Identifier(id),
            Expr::Parenthesized(inner) if !binding && self.simple_target(inner, annex_call) => {
                Pattern::Reference(id)
            }
            Expr::Member { .. } if !binding => Pattern::Reference(id),
            Expr::Call { .. }
                if !binding && annex_call && self.opts.annex_b && !self.ctx.strict =>
            {
                Pattern::Reference(id)
            }
            Expr::Assignment {
                op: AssignOp::Assign,
                target,
                value,
            } if default => Pattern::Default { target, value },
            Expr::Array {
                items,
                trailing_comma,
            } => {
                let mut elements = Vec::new();
                let mut rest = None;
                let len = items.len();
                for (i, item) in items.into_iter().enumerate() {
                    match item {
                        None => self.push(&mut elements, None)?,
                        Some(Item::Value(id)) => {
                            let id = self.reinterpret_pattern(id, binding, true, false)?;
                            self.push(&mut elements, Some(id))?;
                        }
                        Some(Item::Spread(id)) => {
                            if i + 1 != len || trailing_comma {
                                return Err(self.error_at(span, "Rest element must be last"));
                            }
                            rest = Some(self.reinterpret_pattern(id, binding, false, false)?);
                        }
                    }
                }
                Pattern::Array {
                    items: elements,
                    rest,
                }
            }
            Expr::Object {
                properties,
                trailing_comma,
            } => {
                let mut entries = Vec::new();
                let mut rest = None;
                let len = properties.len();
                for (i, property) in properties.into_iter().enumerate() {
                    let entry = match property {
                        Property::Spread(id) => {
                            if i + 1 != len || trailing_comma || !self.simple_target(id, false) {
                                return Err(self.error_at(
                                    self.arena.expr(id).span,
                                    "Invalid object rest target",
                                ));
                            }
                            rest = Some(self.reinterpret_pattern(id, binding, false, false)?);
                            continue;
                        }
                        Property::Value { key, value } => PatternProperty {
                            key,
                            target: self.reinterpret_pattern(value, binding, true, false)?,
                        },
                        Property::Shorthand(key) => PatternProperty {
                            target: self.pattern(key.span, Pattern::Identifier(key.clone()))?,
                            key: Name::Ident(key),
                        },
                        Property::CoverInitialized { key, value } => {
                            let target =
                                self.pattern(key.span, Pattern::Identifier(key.clone()))?;
                            let default_span =
                                Span::new(key.span.start, self.arena.expr(value).span.end);
                            PatternProperty {
                                key: Name::Ident(key),
                                target: self
                                    .pattern(default_span, Pattern::Default { target, value })?,
                            }
                        }
                        Property::Method { .. } => {
                            return Err(self.error_at(span, "Method is not a destructuring target"));
                        }
                    };
                    self.push(&mut entries, entry)?;
                }
                Pattern::Object {
                    properties: entries,
                    rest,
                }
            }
            _ => {
                return Err(self.error_at(
                    span,
                    if binding {
                        "Invalid binding pattern"
                    } else {
                        "Invalid assignment target"
                    },
                ));
            }
        };
        self.pattern(span, kind)
    }

    pub(super) fn simple_target(&self, mut id: ExprId, annex_call: bool) -> bool {
        loop {
            match self.arena.expr(id).kind {
                Expr::Parenthesized(inner) => id = inner,
                Expr::Identifier(_) | Expr::Member { .. } => return true,
                Expr::Call { .. } => return annex_call && self.opts.annex_b && !self.ctx.strict,
                _ => return false,
            }
        }
    }
}
