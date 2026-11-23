use std::collections::{HashMap, HashSet};

use crate::*;

type Result<T = ()> = std::result::Result<T, ParseError>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Var,
    Module,
    Block,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum LexKind {
    Normal,
    OrdinaryFunction,
    SimpleCatch,
}
struct Scope<'a> {
    kind: ScopeKind,
    lexical: HashMap<&'a str, LexKind>,
    var: HashSet<&'a str>,
    params: HashSet<&'a str>,
}

impl Scope<'_> {
    fn new(kind: ScopeKind) -> Self {
        Self {
            kind,
            lexical: HashMap::new(),
            var: HashSet::new(),
            params: HashSet::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct Context {
    strict: bool,
    await_reserved: bool,
    yield_reserved: bool,
    await_allowed: bool,
    yield_allowed: bool,
    return_allowed: bool,
    new_target: bool,
    super_property: bool,
    super_call: bool,
    no_arguments: bool,
    label_base: usize,
    loop_depth: usize,
    switch_depth: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    Var,
    Lexical,
    Parameter,
    Catch,
}

struct Validator<'a> {
    source: &'a SourceText,
    arena: &'a Arena,
    opts: ParseOptions,
    ctx: Context,
    depth: usize,
    scopes: Vec<Scope<'a>>,
    labels: Vec<(&'a str, bool)>,
    private: Vec<HashMap<&'a str, (bool, u8)>>,
    exports: HashSet<Vec<u16>>,
    local_exports: Vec<(&'a str, Span)>,
}

pub(crate) fn validate(source: &SourceText, opts: ParseOptions, program: &ParsedProgram) -> Result {
    let module = opts.goal == ParseGoal::Module;
    let function = matches!(opts.goal, ParseGoal::FunctionBody { .. });
    let is_async = matches!(opts.goal, ParseGoal::FunctionBody { is_async: true, .. });
    let is_generator = matches!(
        opts.goal,
        ParseGoal::FunctionBody {
            is_generator: true,
            ..
        }
    );
    let mut ctx = Context {
        strict: program.strict,
        await_reserved: module || is_async,
        yield_reserved: is_generator,
        await_allowed: module || is_async,
        yield_allowed: is_generator,
        return_allowed: function,
        new_target: function,
        super_property: false,
        super_call: false,
        no_arguments: false,
        label_base: 0,
        loop_depth: 0,
        switch_depth: 0,
    };
    if let ParseGoal::DirectEval {
        allow_new_target,
        allow_super_property,
        allow_super_call,
        in_class_field_init,
    } = opts.goal
    {
        ctx.new_target = allow_new_target;
        ctx.super_property = allow_super_property;
        ctx.super_call = allow_super_call;
        ctx.no_arguments = in_class_field_init;
    }
    let mut v = Validator {
        source,
        arena: &program.arena,
        opts,
        ctx,
        depth: 0,
        scopes: vec![Scope::new(if module {
            ScopeKind::Module
        } else {
            ScopeKind::Var
        })],
        labels: Vec::new(),
        private: Vec::new(),
        exports: HashSet::new(),
        local_exports: Vec::new(),
    };
    v.list(&program.body)?;
    let root = &v.scopes[0];
    for &(name, span) in &v.local_exports {
        if !root.lexical.contains_key(name) && !root.var.contains(name) {
            return Err(v.error(span, "Export has no local binding"));
        }
    }
    Ok(())
}

impl<'a> Validator<'a> {
    fn error(&self, span: Span, message: &str) -> ParseError {
        ParseError::new(self.source, ParseErrorKind::Syntax, span, message)
    }
    fn nest<T>(&mut self, span: Span, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= self.opts.max_depth {
            return Err(ParseError::new(
                self.source,
                ParseErrorKind::StackOverflow,
                span,
                "Syntax tree exceeds the parse depth limit",
            ));
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }
    fn scope(&mut self, kind: ScopeKind, f: impl FnOnce(&mut Self) -> Result) -> Result {
        self.scopes.push(Scope::new(kind));
        let result = f(self);
        self.scopes.pop();
        result
    }
    fn list(&mut self, body: &[StmtId]) -> Result {
        for &stmt in body {
            self.statement(stmt)?;
        }
        Ok(())
    }
    fn statement(&mut self, id: StmtId) -> Result {
        let node = self.arena.stmt(id);
        self.nest(node.span, |v| v.statement_inner(node))
    }
    fn statement_inner(&mut self, node: &'a Node<Stmt>) -> Result {
        let span = node.span;
        match &node.kind {
            Stmt::Empty | Stmt::Debugger => (),
            Stmt::Expression(expr) | Stmt::Throw(expr) => self.expression(*expr)?,
            Stmt::Block(body) => self.scope(ScopeKind::Block, |v| v.list(body))?,
            Stmt::Declaration(declaration) => self.declaration(declaration, None, span)?,
            Stmt::Function(function) => {
                if let Some(name) = &function.name {
                    self.identifier(name, true)?;
                    if self.scopes.last().unwrap().kind == ScopeKind::Var {
                        self.var(name)?;
                    } else {
                        let kind = if !function.is_async && !function.is_generator {
                            LexKind::OrdinaryFunction
                        } else {
                            LexKind::Normal
                        };
                        self.lex(name, kind)?;
                    }
                }
                self.function(function, None, false)?;
            }
            Stmt::Class(class) => {
                if let Some(name) = &class.name {
                    self.lex(name, LexKind::Normal)?;
                }
                self.class(class, span)?;
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                self.expression(*test)?;
                self.branch(*consequent)?;
                if let Some(other) = alternate {
                    self.branch(*other)?;
                }
            }
            Stmt::While { test, body } | Stmt::DoWhile { test, body } => {
                self.expression(*test)?;
                self.iteration(*body)?;
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                self.scope(ScopeKind::Block, |v| {
                    if let Some(init) = init {
                        match init {
                            ForInit::Expression(id) => v.expression(*id)?,
                            ForInit::Declaration(decl) => v.declaration(decl, None, span)?,
                        }
                    }
                    if let Some(id) = test {
                        v.expression(*id)?;
                    }
                    if let Some(id) = update {
                        v.expression(*id)?;
                    }
                    v.iteration(*body)
                })?;
            }
            Stmt::ForEach {
                kind,
                target,
                iterable,
                body,
            } => {
                if *kind == ForKind::AwaitOf && !self.ctx.await_allowed {
                    return Err(self.error(span, "For-await requires an async context"));
                }
                self.scope(ScopeKind::Block, |v| {
                    match target {
                        ForTarget::Pattern(pattern) => v.pattern(*pattern, None, true, false)?,
                        ForTarget::Declaration(decl) => v.declaration(decl, Some(*kind), span)?,
                    }
                    v.expression(*iterable)?;
                    v.iteration(*body)
                })?;
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.expression(*discriminant)?;
                self.ctx.switch_depth += 1;
                self.scope(ScopeKind::Block, |v| {
                    for c in cases {
                        if let Some(test) = c.test {
                            v.expression(test)?;
                        }
                        v.list(&c.body)?;
                    }
                    Ok(())
                })?;
                self.ctx.switch_depth -= 1;
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.statement(*body)?;
                if let Some(catch) = catch {
                    self.scope(ScopeKind::Block, |v| {
                        if let Some(binding) = catch.binding {
                            let simple = v.annex_b()
                                && matches!(v.arena.pattern(binding).kind, Pattern::Identifier(_));
                            v.pattern(binding, Some(Binding::Catch), false, simple)?;
                        }
                        let Stmt::Block(body) = &v.arena.stmt(catch.body).kind else {
                            unreachable!()
                        };
                        v.list(body)
                    })?;
                }
                if let Some(id) = finally {
                    self.statement(*id)?;
                }
            }
            Stmt::With { object, body } => {
                if self.ctx.strict {
                    return Err(self.error(span, "'with' is forbidden in strict mode"));
                }
                self.expression(*object)?;
                self.statement(*body)?;
            }
            Stmt::Label { name, body } => {
                self.identifier(name, false)?;
                if self.labels[self.ctx.label_base..]
                    .iter()
                    .any(|(label, _)| *label == name.name)
                {
                    return Err(self.error(name.span, "Duplicate label"));
                }
                let mut end = *body;
                while let Stmt::Label { body, .. } = self.arena.stmt(end).kind {
                    end = body;
                }
                let iteration = matches!(
                    self.arena.stmt(end).kind,
                    Stmt::For { .. }
                        | Stmt::ForEach { .. }
                        | Stmt::While { .. }
                        | Stmt::DoWhile { .. }
                );
                self.labels.push((&name.name, iteration));
                self.statement(*body)?;
                self.labels.pop();
            }
            Stmt::Break(label) | Stmt::Continue(label) => {
                let is_break = matches!(node.kind, Stmt::Break(_));
                if let Some(label) = label {
                    self.identifier(label, false)?;
                    if !self.labels[self.ctx.label_base..]
                        .iter()
                        .any(|(name, iteration)| *name == label.name && (is_break || *iteration))
                    {
                        return Err(self.error(label.span, "Label does not name a valid target"));
                    }
                } else if self.ctx.loop_depth == 0 && (!is_break || self.ctx.switch_depth == 0) {
                    return Err(self.error(span, "Statement has no control-flow target"));
                }
            }
            Stmt::Return(value) => {
                if !self.ctx.return_allowed {
                    return Err(self.error(span, "'return' requires a function body"));
                }
                if let Some(id) = value {
                    self.expression(*id)?;
                }
            }
            Stmt::Import(import) => {
                self.module_request(&import.request, span)?;
                for binding in &import.bindings {
                    let local = match binding {
                        ImportBinding::Default(local)
                        | ImportBinding::Namespace(local)
                        | ImportBinding::Named { local, .. } => local,
                    };
                    self.identifier(local, true)?;
                    self.lex(local, LexKind::Normal)?;
                }
            }
            Stmt::Export(export) => self.export(export, span)?,
        }
        Ok(())
    }

    fn branch(&mut self, id: StmtId) -> Result {
        if matches!(self.arena.stmt(id).kind, Stmt::Function(_)) {
            self.scope(ScopeKind::Block, |v| v.statement(id))
        } else {
            self.statement(id)
        }
    }
    fn iteration(&mut self, id: StmtId) -> Result {
        self.ctx.loop_depth += 1;
        let result = self.statement(id);
        self.ctx.loop_depth -= 1;
        result
    }
    fn annex_b(&self) -> bool {
        self.opts.annex_b && !self.ctx.strict
    }
    fn var(&mut self, id: &'a Ident) -> Result {
        for scope in self.scopes.iter_mut().rev() {
            if scope
                .lexical
                .get(id.name.as_str())
                .is_some_and(|k| *k != LexKind::SimpleCatch)
            {
                return Err(self.error(id.span, "Variable conflicts with a lexical binding"));
            }
            scope.var.insert(&id.name);
            if scope.kind != ScopeKind::Block {
                break;
            }
        }
        Ok(())
    }
    fn lex(&mut self, id: &'a Ident, kind: LexKind) -> Result {
        let annex_b = self.annex_b();
        let scope = self.scopes.last_mut().unwrap();
        let name = id.name.as_str();
        if scope.var.contains(name)
            || scope.params.contains(name)
            || scope.lexical.get(name).is_some_and(|old| {
                !(annex_b
                    && scope.kind == ScopeKind::Block
                    && *old == LexKind::OrdinaryFunction
                    && kind == LexKind::OrdinaryFunction)
            })
        {
            return Err(self.error(id.span, "Duplicate lexical binding"));
        }
        scope.lexical.insert(name, kind);
        Ok(())
    }
    fn declaration(
        &mut self,
        decl: &'a Declaration,
        loop_kind: Option<ForKind>,
        span: Span,
    ) -> Result {
        let using = matches!(decl.kind, DeclKind::Using | DeclKind::AwaitUsing);
        if decl.kind == DeclKind::AwaitUsing && !self.ctx.await_allowed {
            return Err(self.error(span, "'await using' requires an async context"));
        }
        if loop_kind.is_some() && decl.entries.len() != 1 {
            return Err(self.error(span, "For-in and for-of require one binding"));
        }
        for entry in &decl.entries {
            let binding = self.arena.pattern(entry.binding);
            let simple = matches!(binding.kind, Pattern::Identifier(_));
            if using && (!simple || loop_kind == Some(ForKind::In)) {
                return Err(self.error(binding.span, "Invalid 'using' binding"));
            }
            if let Some(init) = entry.initializer {
                if let Some(kind) = loop_kind
                    && !(kind == ForKind::In
                        && simple
                        && decl.kind == DeclKind::Var
                        && self.annex_b())
                {
                    return Err(self.error(
                        binding.span,
                        "For-in and for-of bindings cannot have initializers",
                    ));
                }
                self.expression(init)?;
            } else if loop_kind.is_none() && (!simple || using || decl.kind == DeclKind::Const) {
                return Err(self.error(binding.span, "Binding requires an initializer"));
            }
            self.pattern(
                entry.binding,
                Some(if decl.kind == DeclKind::Var {
                    Binding::Var
                } else {
                    Binding::Lexical
                }),
                false,
                false,
            )?;
        }
        Ok(())
    }

    fn identifier(&self, id: &Ident, binding: bool) -> Result {
        let name = id.name.as_str();
        let reserved = matches!(
            name,
            "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "import"
                | "in"
                | "instanceof"
                | "new"
                | "null"
                | "return"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
        ) || (self.ctx.await_reserved && name == "await")
            || (self.ctx.yield_reserved && name == "yield")
            || (self.ctx.strict
                && matches!(
                    name,
                    "implements"
                        | "interface"
                        | "let"
                        | "package"
                        | "private"
                        | "protected"
                        | "public"
                        | "static"
                        | "yield"
                ));
        if reserved {
            return Err(self.error(id.span, "Reserved word cannot be used as an identifier"));
        }
        if binding {
            self.restricted_assignment(id)?;
        }
        Ok(())
    }
    fn restricted_assignment(&self, id: &Ident) -> Result {
        if self.ctx.strict && matches!(id.name.as_str(), "eval" | "arguments") {
            Err(self.error(id.span, "Cannot bind or assign this name in strict mode"))
        } else {
            Ok(())
        }
    }
    fn reference(&mut self, id: ExprId, annex_call: bool) -> Result {
        let node = self.arena.expr(id);
        self.nest(node.span, |v| match &node.kind {
            Expr::Identifier(ident) => {
                v.restricted_assignment(ident)?;
                v.identifier_expr(ident)
            }
            Expr::Parenthesized(inner) => v.reference(*inner, annex_call),
            Expr::Member { .. } => v.expression_inner(node),
            Expr::Call { .. } if annex_call && v.annex_b() => v.expression_inner(node),
            _ => Err(v.error(node.span, "Invalid assignment target")),
        })
    }
    fn identifier_expr(&self, id: &Ident) -> Result {
        self.identifier(id, false)?;
        if self.ctx.no_arguments && id.name == "arguments" {
            return Err(self.error(id.span, "'arguments' is forbidden in a class initializer"));
        }
        Ok(())
    }
    fn pattern(
        &mut self,
        id: PatternId,
        binding: Option<Binding>,
        annex_call: bool,
        exceptional: bool,
    ) -> Result {
        let node = self.arena.pattern(id);
        self.nest(node.span, |v| match &node.kind {
            Pattern::Identifier(id) => {
                let Some(binding) = binding else {
                    v.restricted_assignment(id)?;
                    return v.identifier_expr(id);
                };
                v.identifier(id, true)?;
                match binding {
                    Binding::Var => v.var(id),
                    Binding::Lexical => {
                        if id.name == "let" { return Err(v.error(id.span, "Lexical binding cannot be named 'let'")); }
                        v.lex(id, LexKind::Normal)
                    }
                    Binding::Parameter => {
                        if !v.scopes.last_mut().unwrap().params.insert(&id.name) && !exceptional {
                            return Err(v.error(id.span, "Duplicate parameter"));
                        }
                        Ok(())
                    }
                    Binding::Catch => v.lex(id, if exceptional { LexKind::SimpleCatch } else { LexKind::Normal }),
                }
            }
            Pattern::Reference(expr) if binding.is_none() => v.reference(*expr, annex_call),
            Pattern::Reference(_) => Err(v.error(node.span, "Invalid binding target")),
            Pattern::Object { properties, rest } => {
                for prop in properties {
                    v.name(&prop.key, node.span)?;
                    v.pattern(prop.target, binding, false, exceptional)?;
                }
                if let Some(rest) = rest {
                    let target = v.arena.pattern(*rest);
                    if !matches!(target.kind, Pattern::Identifier(_) | Pattern::Reference(_))
                        || matches!(target.kind, Pattern::Reference(id) if matches!(v.arena.expr(id).kind, Expr::Parenthesized(_)))
                    { return Err(v.error(target.span, "Invalid object rest target")); }
                    v.pattern(*rest, binding, false, exceptional)?;
                }
                Ok(())
            }
            Pattern::Array { items, rest } => {
                for &item in items.iter().flatten() { v.pattern(item, binding, false, exceptional)?; }
                if let Some(rest) = rest { v.pattern(*rest, binding, false, exceptional)?; }
                Ok(())
            }
            Pattern::Default { target, value } => {
                v.pattern(*target, binding, false, exceptional)?;
                v.expression(*value)
            }
            Pattern::Rest(target) => v.pattern(*target, binding, false, exceptional),
        })
    }

    fn expression(&mut self, id: ExprId) -> Result {
        let node = self.arena.expr(id);
        self.nest(node.span, |v| v.expression_inner(node))
    }
    fn expression_inner(&mut self, node: &'a Node<Expr>) -> Result {
        let span = node.span;
        match &node.kind {
            Expr::Identifier(id) => self.identifier_expr(id)?,
            Expr::PrivateIdentifier(_) => {
                return Err(self.error(span, "Private identifier requires 'in'"));
            }
            Expr::This | Expr::Null | Expr::Boolean(_) | Expr::RegExp(_) => (),
            Expr::Super => return Err(self.error(span, "Expected a super property or call")),
            Expr::Number(number) => self.number(number, span)?,
            Expr::String(string) => self.string(string, span)?,
            Expr::Array { items, .. } => self.items(items.iter().flatten())?,
            Expr::Object { properties, .. } => {
                let mut proto = false;
                for property in properties {
                    match property {
                        Property::Value { key, value } => {
                            if crate::parser::name_is(key, "__proto__") && self.opts.annex_b {
                                if proto {
                                    return Err(self.error(span, "Duplicate '__proto__' property"));
                                }
                                proto = true;
                            }
                            self.name(key, span)?;
                            self.expression(*value)?;
                        }
                        Property::Shorthand(id) => self.identifier_expr(id)?,
                        Property::CoverInitialized { key, .. } => {
                            return Err(self.error(
                                key.span,
                                "Property initializer requires a destructuring pattern",
                            ));
                        }
                        Property::Method {
                            key,
                            kind,
                            function,
                        } => {
                            self.name(key, span)?;
                            self.function(function, Some(*kind), false)?;
                        }
                        Property::Spread(id) => self.expression(*id)?,
                    }
                }
            }
            Expr::Function(function) => self.function(function, None, false)?,
            Expr::Class(class) => self.class(class, span)?,
            Expr::Template(template) => self.template(template, false)?,
            Expr::TaggedTemplate { tag, template } => {
                self.expression(*tag)?;
                self.template(template, true)?;
            }
            Expr::Parenthesized(id) | Expr::Chain(id) => self.expression(*id)?,
            Expr::Sequence(items) => {
                for &item in items {
                    self.expression(item)?;
                }
            }
            Expr::Unary { op, argument } => {
                if *op == UnaryOp::Await && !self.ctx.await_allowed {
                    return Err(self.error(span, "'await' requires an async context"));
                }
                if *op == UnaryOp::Delete {
                    let mut arg = self.arena.expr(*argument);
                    while let Expr::Parenthesized(id) | Expr::Chain(id) = arg.kind {
                        arg = self.arena.expr(id);
                    }
                    if self.ctx.strict && matches!(arg.kind, Expr::Identifier(_)) {
                        return Err(
                            self.error(arg.span, "Cannot delete an identifier in strict mode")
                        );
                    }
                    if matches!(
                        arg.kind,
                        Expr::Member {
                            key: Name::Private(_),
                            ..
                        }
                    ) {
                        return Err(self.error(arg.span, "Cannot delete a private field"));
                    }
                }
                self.expression(*argument)?;
            }
            Expr::Update { argument, .. } => self.reference(*argument, true)?,
            Expr::Binary { op, left, right } => {
                if *op == BinaryOp::In
                    && let Expr::PrivateIdentifier(private) = &self.arena.expr(*left).kind
                {
                    self.private_name(private)?;
                } else {
                    self.expression(*left)?;
                }
                self.expression(*right)?;
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.expression(*test)?;
                self.expression(*consequent)?;
                self.expression(*alternate)?;
            }
            Expr::Assignment { target, value, .. } => {
                self.pattern(*target, None, true, false)?;
                self.expression(*value)?;
            }
            Expr::Member {
                object,
                key,
                optional,
            } => {
                if matches!(self.arena.expr(*object).kind, Expr::Super) {
                    if !self.ctx.super_property || *optional || matches!(key, Name::Private(_)) {
                        return Err(self.error(span, "Invalid super property reference"));
                    }
                } else {
                    self.expression(*object)?;
                }
                self.name(key, span)?;
            }
            Expr::Call {
                callee,
                arguments,
                optional,
            } => {
                if matches!(self.arena.expr(*callee).kind, Expr::Super) {
                    if !self.ctx.super_call || *optional {
                        return Err(self.error(span, "Invalid super call"));
                    }
                } else {
                    self.expression(*callee)?;
                }
                self.items(arguments.iter())?;
            }
            Expr::New { callee, arguments } => {
                self.expression(*callee)?;
                self.items(arguments.iter())?;
            }
            Expr::NewTarget => {
                if !self.ctx.new_target {
                    return Err(self.error(
                        span,
                        "'new.target' requires a function or class initializer",
                    ));
                }
            }
            Expr::ImportMeta => {
                if self.opts.goal != ParseGoal::Module {
                    return Err(self.error(span, "'import.meta' requires a module"));
                }
            }
            Expr::ImportCall { specifier, options } => {
                self.expression(*specifier)?;
                if let Some(options) = options {
                    self.expression(*options)?;
                }
            }
            Expr::Yield { argument, .. } => {
                if !self.ctx.yield_allowed {
                    return Err(self.error(span, "'yield' requires a generator body"));
                }
                if let Some(argument) = argument {
                    self.expression(*argument)?;
                }
            }
        }
        Ok(())
    }
    fn items<'b>(&mut self, items: impl Iterator<Item = &'b Item>) -> Result {
        for &item in items {
            let (Item::Value(id) | Item::Spread(id)) = item;
            self.expression(id)?;
        }
        Ok(())
    }
    fn template(&mut self, template: &Template, tagged: bool) -> Result {
        if !tagged {
            for element in &template.elements {
                if element.cooked.is_none() {
                    return Err(self.error(element.span, "Invalid escape in untagged template"));
                }
            }
        }
        for &id in &template.substitutions {
            self.expression(id)?;
        }
        Ok(())
    }
    fn string(&self, string: &StringLiteral, span: Span) -> Result {
        if string.legacy_escape && self.ctx.strict {
            Err(self.error(span, "Legacy string escape in strict mode"))
        } else {
            Ok(())
        }
    }
    fn number(&self, number: &NumericLiteral, span: Span) -> Result {
        if self.ctx.strict
            && matches!(
                number.kind,
                NumericKind::LegacyOctal | NumericKind::LegacyDecimal
            )
        {
            Err(self.error(span, "Legacy numeric literal in strict mode"))
        } else {
            Ok(())
        }
    }
    fn name(&mut self, name: &Name, span: Span) -> Result {
        match name {
            Name::Number(lit) => self.number(lit, span),
            Name::String(lit) => self.string(lit, span),
            Name::Computed(id) => self.expression(*id),
            Name::Private(id) => self.private_name(id),
            Name::Ident(_) => Ok(()),
        }
    }
    fn private_name(&self, id: &Ident) -> Result {
        if !self
            .private
            .iter()
            .rev()
            .any(|scope| scope.contains_key(id.name.as_str()))
        {
            return Err(self.error(
                id.span,
                "Private name is not declared in an enclosing class",
            ));
        }
        Ok(())
    }

    fn function(
        &mut self,
        function: &'a Function,
        method: Option<MethodKind>,
        derived: bool,
    ) -> Result {
        let span = function.span;
        let simple = function
            .params
            .iter()
            .all(|&id| matches!(self.arena.pattern(id).kind, Pattern::Identifier(_)));
        if function.use_strict && !simple {
            return Err(self.error(span, "'use strict' requires simple parameters"));
        }
        if method == Some(MethodKind::Getter) && !function.params.is_empty() {
            return Err(self.error(span, "Getter cannot have parameters"));
        }
        if method == Some(MethodKind::Setter)
            && (function.params.len() != 1
                || matches!(
                    self.arena.pattern(function.params[0]).kind,
                    Pattern::Rest(_)
                ))
        {
            return Err(self.error(span, "Setter needs one non-rest parameter"));
        }
        let outer = self.ctx;
        let arrow = function.is_arrow;
        let module = self.opts.goal == ParseGoal::Module;
        self.ctx = Context {
            strict: function.strict,
            await_reserved: module || function.is_async || (arrow && outer.await_reserved),
            yield_reserved: function.is_generator || (arrow && outer.yield_reserved),
            await_allowed: false,
            yield_allowed: false,
            return_allowed: true,
            new_target: !arrow || outer.new_target,
            super_property: method.is_some() || (arrow && outer.super_property),
            super_call: (method == Some(MethodKind::Constructor) && derived)
                || (arrow && outer.super_call),
            no_arguments: arrow && outer.no_arguments,
            label_base: self.labels.len(),
            loop_depth: 0,
            switch_depth: 0,
        };
        if let Some(name) = &function.name {
            self.identifier(name, true)?;
        }
        let allow_duplicates = simple && !self.ctx.strict && !arrow && method.is_none();
        self.scope(ScopeKind::Var, |v| {
            for &param in &function.params {
                v.pattern(param, Some(Binding::Parameter), false, allow_duplicates)?;
            }
            v.ctx.await_reserved = module || function.is_async;
            v.ctx.yield_reserved = function.is_generator;
            v.ctx.await_allowed = function.is_async;
            v.ctx.yield_allowed = function.is_generator;
            match &function.body {
                FunctionBody::Block(body) => v.list(body),
                FunctionBody::Expression(id) => v.expression(*id),
            }
        })?;
        self.ctx = outer;
        Ok(())
    }

    fn class(&mut self, class: &'a Class, span: Span) -> Result {
        let outer = self.ctx;
        self.ctx.strict = true;
        if let Some(name) = &class.name {
            self.identifier(name, true)?;
        }
        if let Some(extends) = class.extends {
            self.expression(extends)?;
        }
        let mut private = HashMap::new();
        let mut constructor = false;
        for element in &class.elements {
            let (key, is_static, kind) = match element {
                ClassElement::Field { key, is_static, .. } => (key, *is_static, None),
                ClassElement::Method {
                    key,
                    is_static,
                    kind,
                    ..
                } => (key, *is_static, Some(*kind)),
                ClassElement::StaticBlock(_) => continue,
            };
            if kind == Some(MethodKind::Constructor) {
                if constructor {
                    return Err(self.error(span, "Duplicate constructor"));
                }
                constructor = true;
            }
            if is_static && crate::parser::name_is(key, "prototype")
                || kind.is_none() && crate::parser::name_is(key, "constructor")
            {
                return Err(self.error(span, "Invalid class element name"));
            }
            if let Name::Private(name) = key {
                if name.name == "constructor" {
                    return Err(self.error(name.span, "Private name cannot be '#constructor'"));
                }
                let flag = match kind {
                    Some(MethodKind::Getter) => 1,
                    Some(MethodKind::Setter) => 2,
                    _ => 3,
                };
                let old = private.entry(name.name.as_str()).or_insert((is_static, 0));
                if old.0 != is_static || old.1 & flag != 0 {
                    return Err(self.error(name.span, "Duplicate private name"));
                }
                old.1 |= flag;
            }
        }
        self.private.push(private);
        for element in &class.elements {
            match element {
                ClassElement::Field {
                    key, initializer, ..
                } => {
                    self.name(key, span)?;
                    if let Some(id) = initializer {
                        self.class_initializer(false, |v| v.expression(*id))?;
                    }
                }
                ClassElement::Method {
                    key,
                    function,
                    kind,
                    ..
                } => {
                    self.name(key, span)?;
                    self.function(function, Some(*kind), class.extends.is_some())?;
                }
                ClassElement::StaticBlock(body) => {
                    self.class_initializer(true, |v| v.scope(ScopeKind::Var, |v| v.list(body)))?
                }
            }
        }
        self.private.pop();
        self.ctx = outer;
        Ok(())
    }
    fn class_initializer(
        &mut self,
        static_block: bool,
        f: impl FnOnce(&mut Self) -> Result,
    ) -> Result {
        let outer = self.ctx;
        self.ctx = Context {
            strict: true,
            await_reserved: static_block || self.opts.goal == ParseGoal::Module,
            yield_reserved: true,
            await_allowed: false,
            yield_allowed: false,
            return_allowed: false,
            new_target: true,
            super_property: true,
            super_call: false,
            no_arguments: true,
            label_base: self.labels.len(),
            loop_depth: 0,
            switch_depth: 0,
        };
        let result = f(self);
        self.ctx = outer;
        result
    }

    fn module_request(&self, request: &ModuleRequest, span: Span) -> Result {
        let mut keys = HashSet::new();
        for attr in &request.attributes {
            if !keys.insert(module_units(&attr.key)) {
                return Err(self.error(span, "Duplicate import attribute"));
            }
        }
        Ok(())
    }
    fn export_name(&mut self, name: Vec<u16>, span: Span) -> Result {
        if !self.exports.insert(name) {
            return Err(self.error(span, "Duplicate export name"));
        }
        Ok(())
    }
    fn export(&mut self, export: &'a ExportDeclaration, span: Span) -> Result {
        match export {
            ExportDeclaration::Declaration(id) => {
                let node = self.arena.stmt(*id);
                match &node.kind {
                    Stmt::Function(f) => self.export_ident(f.name.as_ref().unwrap())?,
                    Stmt::Class(c) => self.export_ident(c.name.as_ref().unwrap())?,
                    Stmt::Declaration(decl) => {
                        for entry in &decl.entries {
                            self.export_pattern(entry.binding)?;
                        }
                    }
                    _ => unreachable!(),
                }
                self.statement(*id)?;
            }
            ExportDeclaration::DefaultDeclaration(id) => {
                self.export_name("default".encode_utf16().collect(), span)?;
                self.statement(*id)?;
            }
            ExportDeclaration::DefaultExpression(id) => {
                self.export_name("default".encode_utf16().collect(), span)?;
                self.expression(*id)?;
            }
            ExportDeclaration::Named {
                specifiers,
                request,
            } => {
                if let Some(request) = request {
                    self.module_request(request, span)?;
                }
                for spec in specifiers {
                    self.export_name(module_units(&spec.exported), span)?;
                    if request.is_none()
                        && let ModuleName::Identifier(name) = &spec.local
                    {
                        self.local_exports.push((name, span));
                    }
                }
            }
            ExportDeclaration::All { alias, request } => {
                self.module_request(request, span)?;
                if let Some(alias) = alias {
                    self.export_name(module_units(alias), span)?;
                }
            }
        }
        Ok(())
    }
    fn export_ident(&mut self, ident: &Ident) -> Result {
        self.export_name(ident.name.encode_utf16().collect(), ident.span)
    }
    fn export_pattern(&mut self, id: PatternId) -> Result {
        match &self.arena.pattern(id).kind {
            Pattern::Identifier(id) => self.export_ident(id)?,
            Pattern::Object { properties, rest } => {
                for prop in properties {
                    self.export_pattern(prop.target)?;
                }
                if let Some(id) = rest {
                    self.export_pattern(*id)?;
                }
            }
            Pattern::Array { items, rest } => {
                for &id in items.iter().flatten() {
                    self.export_pattern(id)?;
                }
                if let Some(id) = rest {
                    self.export_pattern(*id)?;
                }
            }
            Pattern::Default { target, .. } | Pattern::Rest(target) => {
                self.export_pattern(*target)?
            }
            Pattern::Reference(_) => (),
        }
        Ok(())
    }
}

fn module_units(name: &ModuleName) -> Vec<u16> {
    match name {
        ModuleName::Identifier(id) => id.encode_utf16().collect(),
        ModuleName::String(s) => s.units().collect(),
    }
}
