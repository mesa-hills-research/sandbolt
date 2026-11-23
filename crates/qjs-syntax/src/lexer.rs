use crate::source::{decode, is_line, push_utf16};
use crate::{
    Ident, JsStrLit, NumericKind, NumericLiteral, ParseError, ParseErrorKind, RegExpLiteral,
    SourceText, Span, StringLiteral, TemplateElement,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalGoal {
    Div,
    RegExp,
    TemplateTail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Eof,
    Word(Ident),
    Private(Ident),
    Punct(&'static str),
    Number(NumericLiteral),
    String(StringLiteral),
    RegExp(RegExpLiteral),
    Template {
        element: TemplateElement,
        tail: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub line_before: bool,
}

#[derive(Clone, Copy)]
pub struct Lexer<'a> {
    source: &'a SourceText,
    pos: usize,
    line_head: bool,
    html_comments: bool,
    hashbang: bool,
}

const ID_START: u8 = 1;
const ID_CONTINUE: u8 = 2;
const SPACE: u8 = 4;
const ASCII: [u8; 128] = {
    let mut table = [0; 128];
    let mut i = 0;
    while i < 128 {
        let c = i as u8;
        table[i] = match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => ID_START | ID_CONTINUE,
            b'0'..=b'9' => ID_CONTINUE,
            b' ' | b'\t' | 0x0b | 0x0c => SPACE,
            _ => 0,
        };
        i += 1;
    }
    table
};

#[inline]
fn id_start(c: u32) -> bool {
    if c < 128 {
        ASCII[c as usize] & ID_START != 0
    } else {
        !(0xd800..=0xdfff).contains(&c) && qjs_unicode::is_js_ident_start(c)
    }
}
#[inline]
fn id_continue(c: u32) -> bool {
    if c < 128 {
        ASCII[c as usize] & ID_CONTINUE != 0
    } else {
        !(0xd800..=0xdfff).contains(&c) && qjs_unicode::is_js_ident_continue(c)
    }
}
#[inline]
fn space(c: u32) -> bool {
    if c < 128 {
        ASCII[c as usize] & SPACE != 0
    } else {
        !(0xd800..=0xdfff).contains(&c) && qjs_unicode::is_space(c)
    }
}

impl<'a> Lexer<'a> {
    pub const fn new(source: &'a SourceText, html_comments: bool, hashbang: bool) -> Self {
        Self {
            source,
            pos: 0,
            line_head: true,
            html_comments,
            hashbang,
        }
    }

    fn bytes(&self) -> &'a [u8] {
        self.source.as_bytes()
    }
    fn at(&self, offset: usize) -> Option<u8> {
        self.bytes().get(self.pos + offset).copied()
    }
    fn starts(&self, pattern: &[u8]) -> bool {
        self.bytes()[self.pos..].starts_with(pattern)
    }
    fn cp(&self) -> Option<(u32, usize)> {
        let bytes = self.bytes();
        if self.pos == bytes.len() {
            return None;
        }
        let (c, n) = decode(bytes, self.pos);
        if (0xd800..=0xdbff).contains(&c) && bytes.get(self.pos + n) == Some(&0xed) {
            let (d, m) = decode(bytes, self.pos + n);
            if (0xdc00..=0xdfff).contains(&d) {
                return Some((0x10000 + ((c - 0xd800) << 10) + d - 0xdc00, n + m));
            }
        }
        Some((c, n))
    }
    fn span(&self, start: usize) -> Span {
        Span::new(start as u32, self.pos as u32)
    }
    fn error(&self, start: usize, message: &str) -> ParseError {
        ParseError::new(
            self.source,
            ParseErrorKind::Syntax,
            self.span(start),
            message,
        )
    }

    fn trivia(&mut self) -> Result<bool, ParseError> {
        let mut line = false;
        while let Some((c, n)) = self.cp() {
            if is_line(c) {
                self.pos += n;
                line = true;
                self.line_head = true;
            } else if self.starts(b"//")
                || (self.hashbang && self.pos == 0 && self.starts(b"#!"))
                || (self.html_comments
                    && (self.starts(b"<!--") || (self.line_head && self.starts(b"-->"))))
            {
                while let Some((c, n)) = self.cp() {
                    if is_line(c) {
                        break;
                    }
                    self.pos += n;
                }
            } else if self.starts(b"/*") {
                let start = self.pos;
                self.pos += 2;
                while !self.starts(b"*/") {
                    let (c, n) = self
                        .cp()
                        .ok_or_else(|| self.error(start, "Unterminated comment"))?;
                    self.pos += n;
                    if is_line(c) {
                        line = true;
                        self.line_head = true;
                    }
                }
                self.pos += 2;
            } else if space(c) {
                self.pos += n;
            } else {
                break;
            }
        }
        Ok(line)
    }

    pub fn next(&mut self, goal: LexicalGoal) -> Result<Token, ParseError> {
        let line_before = self.trivia()?;
        let start = self.pos;
        let kind = match self.at(0) {
            None => TokenKind::Eof,
            Some(b'}') if goal == LexicalGoal::TemplateTail => self.template()?,
            _ if goal == LexicalGoal::TemplateTail => {
                return Err(self.error(start, "Expected a template continuation"));
            }
            Some(b'`') => self.template()?,
            Some(b'\'' | b'"') => self.string()?,
            Some(b'/') if goal == LexicalGoal::RegExp => self.regexp()?,
            Some(b'0'..=b'9') => TokenKind::Number(self.number()?),
            Some(b'.') if self.at(1).is_some_and(|b| b.is_ascii_digit()) => {
                TokenKind::Number(self.number()?)
            }
            Some(b'#') => {
                self.pos += 1;
                TokenKind::Private(self.identifier(start)?)
            }
            Some(b'\\') => TokenKind::Word(self.identifier(start)?),
            _ if self.cp().is_some_and(|(c, _)| id_start(c)) => {
                TokenKind::Word(self.identifier(start)?)
            }
            _ => self
                .punctuation()
                .ok_or_else(|| self.error(start, "Unexpected character"))?,
        };
        self.line_head = false;
        Ok(Token {
            kind,
            span: self.span(start),
            line_before,
        })
    }

    pub(crate) fn at_token(&mut self, span: Span, goal: LexicalGoal) -> Result<Token, ParseError> {
        self.pos = span.start as usize;
        self.next(goal)
    }

    fn identifier(&mut self, start: usize) -> Result<Ident, ParseError> {
        let mut name = String::new();
        let mut escaped = false;
        while let Some((c, n)) = self.cp() {
            let first = name.is_empty();
            let check = if first { id_start } else { id_continue };
            let c = if c == u32::from(b'\\') {
                self.pos += 1;
                if self.at(0) != Some(b'u') {
                    return Err(self.error(start, "Expected a Unicode escape in identifier"));
                }
                self.pos += 1;
                escaped = true;
                self.unicode_escape()
                    .filter(|&c| check(c))
                    .ok_or_else(|| self.error(start, "Invalid Unicode escape in identifier"))?
            } else if check(c) {
                self.pos += n;
                c
            } else {
                break;
            };
            name.push(char::from_u32(c).expect("identifier code point"));
        }
        if name.is_empty() {
            return Err(self.error(start, "Expected an identifier"));
        }
        Ok(Ident {
            span: self.span(start),
            name,
            escaped,
        })
    }

    fn punctuation(&mut self) -> Option<TokenKind> {
        let candidates: &[&str] = match self.at(0)? {
            b'>' => &[">>>=", ">>>", ">>=", ">>", ">=", ">"],
            b'<' => &["<<=", "<<", "<=", "<"],
            b'=' => &["===", "==", "=>", "="],
            b'!' => &["!==", "!=", "!"],
            b'?' => &["??=", "??", "?.", "?"],
            b'*' => &["**=", "**", "*=", "*"],
            b'&' => &["&&=", "&&", "&=", "&"],
            b'|' => &["||=", "||", "|=", "|"],
            b'^' => &["^=", "^"],
            b'+' => &["++", "+=", "+"],
            b'-' => &["--", "-=", "-"],
            b'/' => &["/=", "/"],
            b'%' => &["%=", "%"],
            b'.' => &["...", "."],
            b'{' => &["{"],
            b'}' => &["}"],
            b'(' => &["("],
            b')' => &[")"],
            b'[' => &["["],
            b']' => &["]"],
            b';' => &[";"],
            b',' => &[","],
            b':' => &[":"],
            b'~' => &["~"],
            _ => return None,
        };
        for &p in candidates {
            if self.starts(p.as_bytes())
                && !(p == "?." && self.at(2).is_some_and(|b| b.is_ascii_digit()))
            {
                self.pos += p.len();
                return Some(TokenKind::Punct(p));
            }
        }
        None
    }

    fn digits(&mut self, radix: u32, separators: bool) -> Option<usize> {
        let mut count = 0;
        let mut digit = false;
        while let Some(b) = self.at(0) {
            if char::from(b).is_digit(radix) {
                count += 1;
                digit = true;
            } else if b == b'_' {
                if !digit || !separators {
                    return None;
                }
                digit = false;
            } else {
                break;
            }
            self.pos += 1;
        }
        if count != 0 && !digit {
            None
        } else {
            Some(count)
        }
    }

    fn number(&mut self) -> Result<NumericLiteral, ParseError> {
        let start = self.pos;
        self.scan_number()
            .ok_or_else(|| self.error(start, "invalid number literal"))
    }

    fn scan_number(&mut self) -> Option<NumericLiteral> {
        let start = self.pos;
        let mut kind = NumericKind::Decimal;
        let mut radix = 10;
        let mut integer = true;
        if self.at(0) == Some(b'0') {
            match self.at(1) {
                Some(b'x' | b'X') => {
                    radix = 16;
                    kind = NumericKind::Hex;
                }
                Some(b'o' | b'O') => {
                    radix = 8;
                    kind = NumericKind::Octal;
                }
                Some(b'b' | b'B') => {
                    radix = 2;
                    kind = NumericKind::Binary;
                }
                Some(b'0'..=b'9') => kind = NumericKind::LegacyOctal,
                Some(b'_') => return None,
                _ => (),
            }
        }
        if radix != 10 {
            self.pos += 2;
            if !matches!(self.digits(radix, true), Some(1..)) {
                return None;
            }
        } else {
            let legacy = kind == NumericKind::LegacyOctal;
            self.digits(10, !legacy)?;
            if legacy && self.bytes()[start..self.pos].iter().any(|&b| b >= b'8') {
                kind = NumericKind::LegacyDecimal;
            }
            if kind != NumericKind::LegacyOctal {
                if self.at(0) == Some(b'.') {
                    integer = false;
                    self.pos += 1;
                    self.digits(10, true)?;
                }
                if matches!(self.at(0), Some(b'e' | b'E')) {
                    integer = false;
                    self.pos += 1;
                    if matches!(self.at(0), Some(b'+' | b'-')) {
                        self.pos += 1;
                    }
                    if !matches!(self.digits(10, true), Some(1..)) {
                        return None;
                    }
                }
            }
        }
        if self.at(0) == Some(b'n')
            && integer
            && !matches!(kind, NumericKind::LegacyOctal | NumericKind::LegacyDecimal)
        {
            self.pos += 1;
            kind = NumericKind::BigInt { radix: radix as u8 };
        }
        if self
            .cp()
            .is_some_and(|(c, _)| id_start(c) || c == u32::from(b'\\') || (48..=57).contains(&c))
        {
            return None;
        }
        let raw = std::str::from_utf8(&self.bytes()[start..self.pos])
            .expect("ASCII numeric literal")
            .into();
        Some(NumericLiteral { raw, kind })
    }

    fn fixed_hex(&mut self, len: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..len {
            value = value * 16 + char::from(self.at(0)?).to_digit(16)?;
            self.pos += 1;
        }
        Some(value)
    }

    fn unicode_escape(&mut self) -> Option<u32> {
        if self.at(0) != Some(b'{') {
            return self.fixed_hex(4);
        }
        self.pos += 1;
        let mut value = 0;
        let start = self.pos;
        while let Some(d) = self.at(0).and_then(|b| char::from(b).to_digit(16)) {
            value = value * 16 + d;
            if value > 0x10ffff {
                return None;
            }
            self.pos += 1;
        }
        if self.pos == start || self.at(0) != Some(b'}') {
            return None;
        }
        self.pos += 1;
        Some(value)
    }

    fn escape(&mut self, out: &mut Vec<u16>, legacy: &mut bool) -> Option<()> {
        let (c, n) = self.cp()?;
        self.pos += n;
        let value = match c {
            13 => {
                if self.at(0) == Some(b'\n') {
                    self.pos += 1;
                }
                return Some(());
            }
            10 | 0x2028 | 0x2029 => return Some(()),
            98 => 8,
            102 => 12,
            110 => 10,
            114 => 13,
            116 => 9,
            118 => 11,
            120 => self.fixed_hex(2)?,
            117 => self.unicode_escape()?,
            48 if !self.at(0).is_some_and(|b| b.is_ascii_digit()) => 0,
            48..=55 => {
                *legacy = true;
                let mut value = c - 48;
                for _ in 0..if value < 4 { 2 } else { 1 } {
                    let Some(b @ b'0'..=b'7') = self.at(0) else {
                        break;
                    };
                    value = value * 8 + u32::from(b - b'0');
                    self.pos += 1;
                }
                value
            }
            56 | 57 => {
                *legacy = true;
                c
            }
            _ => c,
        };
        push_utf16(out, value);
        Some(())
    }

    fn string(&mut self) -> Result<TokenKind, ParseError> {
        let start = self.pos;
        let quote = u32::from(self.at(0).expect("quote"));
        self.pos += 1;
        let mut value = Vec::new();
        let mut escaped = false;
        let mut legacy_escape = false;
        loop {
            let (c, n) = self
                .cp()
                .ok_or_else(|| self.error(start, "Unterminated string literal"))?;
            self.pos += n;
            match c {
                c if c == quote => break,
                10 | 13 => return Err(self.error(start, "Line terminator in string literal")),
                92 => {
                    escaped = true;
                    self.escape(&mut value, &mut legacy_escape)
                        .ok_or_else(|| self.error(start, "Invalid string escape"))?;
                }
                c => push_utf16(&mut value, c),
            }
        }
        Ok(TokenKind::String(StringLiteral {
            value: JsStrLit::from_units(value),
            escaped,
            legacy_escape,
        }))
    }

    fn template(&mut self) -> Result<TokenKind, ParseError> {
        let token_start = self.pos;
        self.pos += 1;
        let start = self.pos;
        let mut raw = Vec::new();
        loop {
            let end = self.pos;
            let (c, n) = self
                .cp()
                .ok_or_else(|| self.error(token_start, "Unterminated template literal"))?;
            self.pos += n;
            let tail = c == u32::from(b'`');
            if tail || (c == u32::from(b'$') && self.at(0) == Some(b'{')) {
                let span = Span::new(start as u32, end as u32);
                let cooked = self.cook_template(start, end);
                if !tail {
                    self.pos += 1;
                }
                return Ok(TokenKind::Template {
                    element: TemplateElement {
                        span,
                        raw: JsStrLit::from_units(raw),
                        cooked,
                    },
                    tail,
                });
            }
            if c == u32::from(b'\\') {
                raw.push(b'\\' as u16);
                let (escaped, n) = self
                    .cp()
                    .ok_or_else(|| self.error(token_start, "Unterminated template escape"))?;
                self.pos += n;
                self.template_raw(&mut raw, escaped);
            } else {
                self.template_raw(&mut raw, c);
            }
        }
    }

    fn template_raw(&mut self, raw: &mut Vec<u16>, c: u32) {
        if c == 13 {
            if self.at(0) == Some(b'\n') {
                self.pos += 1;
            }
            raw.push(10);
        } else {
            push_utf16(raw, c);
        }
    }

    fn cook_template(&self, start: usize, end: usize) -> Option<JsStrLit> {
        let mut scan = *self;
        scan.pos = start;
        let mut cooked = Vec::new();
        let mut legacy = false;
        while scan.pos < end {
            let (c, n) = scan.cp()?;
            scan.pos += n;
            if c == u32::from(b'\\') {
                scan.escape(&mut cooked, &mut legacy)?;
                if legacy || scan.pos > end {
                    return None;
                }
            } else {
                scan.template_raw(&mut cooked, c);
            }
        }
        Some(JsStrLit::from_units(cooked))
    }

    fn regexp(&mut self) -> Result<TokenKind, ParseError> {
        let start = self.pos;
        self.pos += 1;
        let mut pattern = Vec::new();
        let mut in_class = false;
        let mut escaped = false;
        loop {
            let (c, n) = self
                .cp()
                .ok_or_else(|| self.error(start, "Unterminated regular expression"))?;
            self.pos += n;
            if is_line(c) {
                return Err(self.error(start, "Line terminator in regular expression"));
            }
            if !escaped {
                match c {
                    47 if !in_class => break,
                    91 => in_class = true,
                    93 => in_class = false,
                    _ => (),
                }
            }
            escaped = c == u32::from(b'\\') && !escaped;
            push_utf16(&mut pattern, c);
        }
        let flags_start = self.pos;
        let mut seen = 0u16;
        while let Some((c, n)) = self.cp() {
            if !(id_continue(c) || c == u32::from(b'\\')) {
                break;
            }
            self.pos += n;
            let flag = "dgimsuvy"
                .find(char::from_u32(c).unwrap_or('\0'))
                .ok_or_else(|| self.error(start, "Invalid regular expression flag"))?;
            let bit = 1 << flag;
            if seen & bit != 0 {
                return Err(self.error(start, "Duplicate regular expression flag"));
            }
            seen |= bit;
        }
        let flags: String = std::str::from_utf8(&self.bytes()[flags_start..self.pos])
            .expect("ASCII regexp flags")
            .into();
        if flags.contains('u') && flags.contains('v') {
            return Err(self.error(start, "Regular expression cannot use both u and v flags"));
        }
        Ok(TokenKind::RegExp(RegExpLiteral {
            pattern: JsStrLit::from_units(pattern),
            flags,
        }))
    }
}
