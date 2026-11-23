mod cover;
mod expression;
mod module;
mod statement;

pub(crate) use statement::name_is;

use std::mem;

use crate::*;

type Result<T> = std::result::Result<T, ParseError>;

#[derive(Clone, Copy)]
struct Context {
    strict: bool,
    await_expr: bool,
    yield_expr: bool,
}

pub(crate) struct Parser<'a> {
    source: &'a SourceText,
    opts: ParseOptions,
    lexer: Lexer<'a>,
    current: Token,
    end: u32,
    depth: usize,
    remaining: usize,
    ctx: Context,
    allow_in_arrow: bool,
    arena: Arena,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(source: &'a SourceText, opts: ParseOptions) -> Result<Self> {
        let module = opts.goal == ParseGoal::Module;
        let script = matches!(opts.goal, ParseGoal::Script | ParseGoal::DirectEval { .. });
        let remaining = opts
            .max_bytes
            .checked_sub(source.as_bytes().len())
            .filter(|_| u32::try_from(source.as_bytes().len()).is_ok())
            .ok_or_else(|| {
                ParseError::new(
                    source,
                    ParseErrorKind::OutOfMemory,
                    Span::default(),
                    "Source exceeds the parse byte limit",
                )
            })?;
        let mut lexer = Lexer::new(
            source,
            script && opts.annex_b,
            matches!(opts.goal, ParseGoal::Script | ParseGoal::Module),
        );
        let current = lexer.next(LexicalGoal::Div)?;
        Ok(Self {
            source,
            opts,
            lexer,
            current,
            remaining,
            end: 0,
            depth: 0,
            allow_in_arrow: true,
            ctx: Context {
                strict: opts.strict || module,
                await_expr: module
                    || matches!(opts.goal, ParseGoal::FunctionBody { is_async: true, .. }),
                yield_expr: matches!(
                    opts.goal,
                    ParseGoal::FunctionBody {
                        is_generator: true,
                        ..
                    }
                ),
            },
            arena: Arena::default(),
        })
    }

    pub(crate) fn program(mut self) -> Result<ParsedProgram> {
        let (body, _) = self.stmt_list(false, true)?;
        Ok(ParsedProgram {
            strict: self.ctx.strict,
            span: Span::new(0, self.current.span.end),
            body,
            arena: self.arena,
        })
    }

    fn start(&self) -> u32 {
        self.current.span.start
    }
    fn span(&self, start: u32) -> Span {
        Span::new(start, self.end.max(start))
    }
    fn error(&self, message: &str) -> ParseError {
        self.error_at(self.current.span, message)
    }
    fn error_at(&self, span: Span, message: &str) -> ParseError {
        ParseError::new(self.source, ParseErrorKind::Syntax, span, message)
    }
    fn oom(&self) -> ParseError {
        ParseError::new(
            self.source,
            ParseErrorKind::OutOfMemory,
            self.current.span,
            "Syntax tree exceeds the parse byte limit",
        )
    }
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| self.oom())?;
        Ok(())
    }
    fn push<T>(&mut self, list: &mut Vec<T>, value: T) -> Result<()> {
        self.charge(mem::size_of::<T>())?;
        list.try_reserve(1).map_err(|_| self.oom())?;
        list.push(value);
        Ok(())
    }
    fn nest<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= self.opts.max_depth {
            return Err(ParseError::new(
                self.source,
                ParseErrorKind::StackOverflow,
                self.current.span,
                "Syntax nesting exceeds the parse depth limit",
            ));
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }
    fn expr(&mut self, start: u32, kind: Expr) -> Result<ExprId> {
        self.charge(mem::size_of::<Node<Expr>>())?;
        self.arena
            .expressions
            .try_reserve(1)
            .map_err(|_| self.oom())?;
        let id = ExprId(self.arena.expressions.len() as u32);
        self.arena.expressions.push(Node {
            span: self.span(start),
            kind,
        });
        Ok(id)
    }
    fn stmt(&mut self, start: u32, kind: Stmt) -> Result<StmtId> {
        self.charge(mem::size_of::<Node<Stmt>>())?;
        self.arena
            .statements
            .try_reserve(1)
            .map_err(|_| self.oom())?;
        let id = StmtId(self.arena.statements.len() as u32);
        self.arena.statements.push(Node {
            span: self.span(start),
            kind,
        });
        Ok(id)
    }
    fn pattern(&mut self, span: Span, kind: Pattern) -> Result<PatternId> {
        self.charge(mem::size_of::<Node<Pattern>>())?;
        self.arena.patterns.try_reserve(1).map_err(|_| self.oom())?;
        let id = PatternId(self.arena.patterns.len() as u32);
        self.arena.patterns.push(Node { span, kind });
        Ok(id)
    }
    fn eof(&self) -> bool {
        self.current.kind == TokenKind::Eof
    }
    fn punct(&self, punct: &str) -> bool {
        matches!(self.current.kind, TokenKind::Punct(p) if p == punct)
    }
    fn word(&self, word: &str) -> bool {
        word_is(&self.current, word)
    }
    fn next(&mut self) -> Result<Token> {
        let next = self.lexer.next(LexicalGoal::Div)?;
        let token = mem::replace(&mut self.current, next);
        self.end = token.span.end;
        Ok(token)
    }
    fn peek(&self) -> Result<Token> {
        let mut look = self.lexer;
        look.next(LexicalGoal::Div)
    }
    fn eat(&mut self, punct: &str) -> Result<bool> {
        if self.punct(punct) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn eat_word(&mut self, word: &str) -> Result<bool> {
        if self.word(word) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn expect(&mut self, punct: &str) -> Result<()> {
        if self.eat(punct)? {
            Ok(())
        } else {
            Err(self.error(&format!("Expected '{punct}'")))
        }
    }
    fn expect_word(&mut self, word: &str) -> Result<()> {
        if self.eat_word(word)? {
            Ok(())
        } else {
            Err(self.error(&format!("Expected '{word}'")))
        }
    }
    fn ident(&mut self) -> Result<Ident> {
        if !matches!(self.current.kind, TokenKind::Word(_)) {
            return Err(self.error("Expected an identifier"));
        }
        match self.next()?.kind {
            TokenKind::Word(ident) => Ok(ident),
            _ => unreachable!(),
        }
    }
    fn private(&mut self) -> Result<Ident> {
        match self.next()?.kind {
            TokenKind::Private(ident) => Ok(ident),
            _ => unreachable!(),
        }
    }
    fn asi(&mut self) -> Result<()> {
        if self.eat(";")? || self.current.line_before || self.eof() || self.punct("}") {
            Ok(())
        } else {
            Err(self.error("Expected ';'"))
        }
    }
}

fn word_is(token: &Token, name: &str) -> bool {
    matches!(&token.kind, TokenKind::Word(ident) if !ident.escaped && ident.name == name)
}

fn word_is_one(token: &Token, names: &[&str]) -> bool {
    matches!(&token.kind, TokenKind::Word(ident) if !ident.escaped && names.contains(&ident.name.as_str()))
}

fn property_name_start(token: &Token, private: bool) -> bool {
    matches!(
        token.kind,
        TokenKind::Word(_) | TokenKind::String(_) | TokenKind::Number(_) | TokenKind::Punct("[")
    ) || (private && matches!(token.kind, TokenKind::Private(_)))
}
