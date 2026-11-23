use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Parse,
    Resolution,
    Runtime,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Parse => "parse",
            Self::Resolution => "resolution",
            Self::Runtime => "runtime",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Negative {
    pub phase: Phase,
    pub error_type: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub description: Option<String>,
    pub esid: Option<String>,
    pub flags: Vec<String>,
    pub includes: Vec<String>,
    pub features: Vec<String>,
    pub negative: Option<Negative>,
    pub locale: Vec<String>,
}

impl Metadata {
    pub(crate) fn parse(source: &str) -> Result<Self, String> {
        let Some(body) = source.match_indices("/*---").find_map(|(start, marker)| {
            let body = &source[start + marker.len()..];
            body.starts_with(char::is_whitespace).then_some(body)
        }) else {
            return Ok(Self::default());
        };
        let (body, _) = body.split_once("---*/").ok_or("unclosed test metadata")?;
        let lines: Vec<_> = body.trim_end_matches([' ', '\t']).lines().collect();
        let fields = fields(&lines)?;
        let mut result = Self::default();
        for field in fields {
            let Field {
                key,
                value,
                rest,
                indent,
            } = field;
            match key {
                "description" => result.description = Some(block_text(value, rest, indent)?),
                "esid" => result.esid = Some(block_text(value, rest, indent)?),
                "flags" => result.flags = sequence(value, rest)?,
                "includes" => result.includes = sequence(value, rest)?,
                "features" => result.features = sequence(value, rest)?,
                "locale" => result.locale = sequence(value, rest)?,
                "negative" => result.negative = Some(negative(value, rest)?),
                _ => {}
            }
        }
        Ok(result)
    }

    pub(crate) fn flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|item| item == flag)
    }
}

struct Field<'a> {
    key: &'a str,
    value: &'a str,
    rest: &'a [&'a str],
    indent: usize,
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn fields<'a>(lines: &'a [&'a str]) -> Result<Vec<Field<'a>>, String> {
    let significant = |line: &str| !line.trim().is_empty() && !line.trim_start().starts_with('#');
    let Some(base) = lines
        .iter()
        .find(|line| significant(line))
        .map(|line| indentation(line))
    else {
        return Ok(Vec::new());
    };
    let mut positions = Vec::new();
    let mut flow = Flow::default();
    for (index, line) in lines.iter().enumerate() {
        if !significant(line) {
            continue;
        }
        if flow.active() {
            flow.scan(line);
        } else if indentation(line) == base && !line.trim_start().starts_with("- ") {
            let (key, value) = pair(line.trim_start())?;
            positions.push((index, key, value));
            if value.starts_with(['[', '{', '\'', '"']) {
                flow.scan(value);
            }
        }
    }
    let mut result = Vec::with_capacity(positions.len());
    for (i, &(index, key, value)) in positions.iter().enumerate() {
        let end = positions.get(i + 1).map_or(lines.len(), |next| next.0);
        result.push(Field {
            key,
            value,
            rest: &lines[index + 1..end],
            indent: base,
        });
    }
    Ok(result)
}

fn pair(text: &str) -> Result<(&str, &str), String> {
    text.split_once(':')
        .map(|(key, value)| (key.trim(), value.trim_start()))
        .ok_or_else(|| format!("expected a metadata field, got {text:?}"))
}

#[derive(Default)]
struct Flow {
    quote: Option<char>,
    depth: usize,
    escaped: bool,
}

impl Flow {
    fn active(&self) -> bool {
        self.quote.is_some() || self.depth > 0
    }

    // Return the unquoted, uncommented separators at depth zero.
    fn separators(&mut self, text: &str, separator: char) -> Vec<usize> {
        let mut result = Vec::new();
        let mut chars = text.char_indices().peekable();
        let mut space = true;
        while let Some((index, ch)) = chars.next() {
            if let Some(quote) = self.quote {
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' && quote == '"' {
                    self.escaped = true;
                } else if ch == quote {
                    if quote == '\'' && chars.peek().is_some_and(|&(_, c)| c == quote) {
                        chars.next();
                    } else {
                        self.quote = None;
                    }
                }
            } else if ch == '#' && space {
                break;
            } else {
                if ch == separator && self.depth == 0 {
                    result.push(index);
                }
                match ch {
                    '\'' | '"' if space => self.quote = Some(ch),
                    '[' | '{' => self.depth += 1,
                    ']' | '}' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
            }
            space = ch.is_whitespace() || matches!(ch, '[' | '{' | ',' | ':');
        }
        result
    }

    fn scan(&mut self, text: &str) {
        self.separators(text, '\0');
    }
}

fn strip_comment(value: &str) -> &str {
    let mut quote = None;
    let mut chars = value.char_indices().peekable();
    let mut space = true;
    while let Some((offset, ch)) = chars.next() {
        match quote {
            Some('\'') if ch == '\'' => {
                if chars.peek().is_some_and(|&(_, ch)| ch == '\'') {
                    chars.next();
                } else {
                    quote = None;
                }
            }
            Some('"') if ch == '\\' => {
                chars.next();
            }
            Some(open) if ch == open => quote = None,
            None if ch == '#' && space => return value[..offset].trim_end(),
            None if matches!(ch, '\'' | '"') && space => quote = Some(ch),
            _ => {}
        }
        space = ch.is_whitespace() || matches!(ch, '[' | '{' | ',' | ':');
    }
    value.trim_end()
}

fn flow_text(head: &str, rest: &[&str]) -> String {
    std::iter::once(head)
        .chain(rest.iter().copied())
        .map(|line| strip_comment(line.trim()))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn flow_parts(text: &str, start: char, end: char) -> Result<Vec<&str>, String> {
    let body = text
        .strip_prefix(start)
        .and_then(|text| text.strip_suffix(end))
        .ok_or_else(|| format!("unclosed metadata {start}{end} value"))?;
    let mut offsets = Flow::default().separators(body, ',');
    offsets.push(body.len());
    let mut start = 0;
    let mut result = Vec::new();
    for end in offsets {
        let item = body[start..end].trim();
        if !item.is_empty() {
            result.push(item);
        }
        start = end + 1;
    }
    Ok(result)
}

fn sequence(head: &str, rest: &[&str]) -> Result<Vec<String>, String> {
    let head = strip_comment(head);
    if head.starts_with('[') {
        let text = flow_text(head, rest);
        return flow_parts(&text, '[', ']')?
            .into_iter()
            .map(scalar)
            .collect();
    }
    if !head.is_empty() {
        return Err(format!("expected a metadata list, got {head:?}"));
    }
    rest.iter()
        .filter_map(|line| {
            let line = strip_comment(line.trim());
            (!line.is_empty()).then(|| {
                line.strip_prefix('-')
                    .and_then(|line| line.strip_prefix(' '))
                    .ok_or_else(|| format!("expected a list item, got {line:?}"))
                    .and_then(|line| scalar(line.trim_start()))
            })
        })
        .collect()
}

fn scalar(value: &str) -> Result<String, String> {
    if let Some(value) = value.strip_prefix('\'') {
        return value
            .strip_suffix('\'')
            .map(|text| text.replace("''", "'"))
            .ok_or_else(|| "unclosed single-quoted metadata string".into());
    }
    let Some(value) = value.strip_prefix('"') else {
        return Ok(value.into());
    };
    let value = value
        .strip_suffix('"')
        .ok_or("unclosed double-quoted metadata string")?;
    let mut chars = value.chars();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let escape = chars.next().ok_or("incomplete metadata escape")?;
        output.push(match escape {
            '0' => '\0',
            'a' => '\x07',
            'b' => '\x08',
            't' => '\t',
            'n' => '\n',
            'v' => '\x0b',
            'f' => '\x0c',
            'r' => '\r',
            'e' => '\x1b',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            ' ' | '"' | '/' | '\\' => escape,
            'x' | 'u' | 'U' => {
                let count = match escape {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let mut code = 0;
                for _ in 0..count {
                    let digit = chars
                        .next()
                        .and_then(|ch| ch.to_digit(16))
                        .ok_or("invalid metadata hex escape")?;
                    code = code * 16 + digit;
                }
                char::from_u32(code).ok_or("invalid metadata code point")?
            }
            _ => return Err(format!("unknown metadata escape {escape:?}")),
        });
    }
    Ok(output)
}

fn block_text(head: &str, rest: &[&str], indent: usize) -> Result<String, String> {
    let head = strip_comment(head);
    if let Some(style @ ('|' | '>')) = head.chars().next() {
        let explicit_indent = head.chars().skip(1).find_map(|ch| ch.to_digit(10));
        let base = explicit_indent
            .map(|value| indent + value as usize)
            .unwrap_or_else(|| {
                rest.iter()
                    .find(|line| !line.trim().is_empty())
                    .map_or(0, |line| indentation(line))
            });
        let mut output = String::new();
        let mut blank = 0;
        let mut previous = None;
        for line in rest {
            if line.trim().is_empty() {
                blank += 1;
                continue;
            }
            let line = line.get(base..).unwrap_or(line);
            let indented = line.starts_with([' ', '\t']);
            match previous {
                Some(false) if style == '>' && !indented && blank == 0 => output.push(' '),
                Some(false) if style == '>' && !indented => {
                    output.extend(std::iter::repeat_n('\n', blank))
                }
                Some(_) => output.extend(std::iter::repeat_n('\n', blank + 1)),
                None => output.extend(std::iter::repeat_n('\n', blank)),
            }
            blank = 0;
            previous = Some(indented);
            output.push_str(line);
        }
        output.extend(std::iter::repeat_n(
            '\n',
            blank + usize::from(previous.is_some()),
        ));
        if !head.contains('+') {
            let len = output.trim_end_matches('\n').len();
            output.truncate(len);
            if !head.contains('-') && !output.is_empty() {
                output.push('\n');
            }
        }
        return Ok(output);
    }
    scalar(&flow_text(head, rest))
}

fn negative(head: &str, rest: &[&str]) -> Result<Negative, String> {
    let head = strip_comment(head);
    let flow;
    let entries = if head.is_empty() {
        fields(rest)?
            .into_iter()
            .map(|f| (f.key, strip_comment(f.value)))
            .collect::<Vec<_>>()
    } else {
        flow = flow_text(head, rest);
        flow_parts(&flow, '{', '}')?
            .into_iter()
            .map(pair)
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut phase = None;
    let mut error_type = None;
    for (key, value) in entries {
        match scalar(key)?.as_str() {
            "phase" => {
                phase = Some(match scalar(value)?.as_str() {
                    "parse" => Phase::Parse,
                    "resolution" => Phase::Resolution,
                    "runtime" => Phase::Runtime,
                    other => return Err(format!("unknown negative phase {other:?}")),
                });
            }
            "type" => error_type = Some(scalar(value)?),
            _ => {}
        }
    }
    Ok(Negative {
        phase: phase.ok_or("negative metadata needs a phase")?,
        error_type: error_type
            .filter(|name| !name.is_empty())
            .ok_or("negative metadata needs an error type")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_and_flow_lists() {
        let metadata = Metadata::parse(
            r##"// license
/*---
description: >-
  Test with nested includes: and flags:
  in the description.
esid: sec-host-hooks
flags: [module, async, raw, generated, CanBlockIsFalse, CanBlockIsTrue, non-deterministic,
onlyStrict, noStrict] # trailing comment
features:
  - cross-realm
  - host-gc-required
includes:
- 'contains,comma.js'
- "double\u002ejs"
locale: [en-US, de, "#comment"]
negative:
  type: ReferenceError # comment
  phase: resolution
info: |
  includes: [ignored.js]
---*/
$DONOTEVALUATE();"##,
        )
        .unwrap();
        assert_eq!(
            metadata.description.as_deref(),
            Some("Test with nested includes: and flags: in the description.")
        );
        assert_eq!(metadata.esid.as_deref(), Some("sec-host-hooks"));
        assert_eq!(metadata.flags.len(), 9);
        for flag in [
            "module",
            "raw",
            "async",
            "CanBlockIsFalse",
            "non-deterministic",
        ] {
            assert!(metadata.flag(flag));
        }
        assert_eq!(metadata.includes, ["contains,comma.js", "double.js"]);
        assert_eq!(metadata.features, ["cross-realm", "host-gc-required"]);
        assert_eq!(metadata.locale, ["en-US", "de", "#comment"]);
        assert_eq!(
            metadata.negative,
            Some(Negative {
                phase: Phase::Resolution,
                error_type: "ReferenceError".into()
            })
        );
    }

    #[test]
    fn literals_quotes_and_negative_flow() {
        let metadata = Metadata::parse("/*---\r\ndescription: |\r\n  line # one\r\n  line two\r\nesid: 'there''s: a quote'\r\nflags: []\r\nnegative: { phase: 'parse', type: SyntaxError }\r\n---*/").unwrap();
        assert_eq!(
            metadata.description.as_deref(),
            Some("line # one\nline two\n")
        );
        assert_eq!(metadata.esid.as_deref(), Some("there's: a quote"));
        assert!(metadata.flags.is_empty());
        assert_eq!(metadata.negative.unwrap().phase, Phase::Parse);
        let metadata =
            Metadata::parse("/*---\nnegative: {\n phase: runtime,\n type: TypeError,\n}\n---*/")
                .unwrap();
        assert_eq!(metadata.negative.unwrap().phase, Phase::Runtime);
    }

    #[test]
    fn folded_paragraphs_and_explicit_indentation() {
        let source = "/*---\n  description: >2-\n    one\n\n    two\n      code\n    three\n---*/";
        assert_eq!(
            Metadata::parse(source).unwrap().description.as_deref(),
            Some("one\ntwo\n  code\nthree")
        );
    }

    #[test]
    fn dashed_comment_is_not_metadata() {
        let metadata = Metadata::parse("/*----------------*/ var answer = 42;").unwrap();
        assert_eq!(metadata, Metadata::default());
    }

    #[test]
    fn invalid_metadata_is_reported() {
        for bad in [
            "negative: {phase: early, type: Error}",
            "negative: {phase: parse}",
            "negative: {type: Error}",
            "flags: [module",
            "includes: not-a-list",
        ] {
            assert!(
                Metadata::parse(&format!("/*---\n{bad}\n---*/")).is_err(),
                "{bad}"
            );
        }
        assert!(Metadata::parse("/*---\nflags: []").is_err());
        assert_eq!(
            Metadata::parse("print('no metadata')").unwrap(),
            Metadata::default()
        );
    }
}
