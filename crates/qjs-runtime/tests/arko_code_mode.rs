//! Arko `code_mode` acceptance test suite.
//!
//! Mirrors Arko's `src/code_mode/{engine,host,prelude.js,engine_tests}` directly
//! against `qjs_runtime::api::JsEngine` with zero external crate dependencies.

#![deny(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use qjs_runtime::api::{EvalOptions, JsEngine};
use qjs_runtime::{EngineError, EngineLimits, Frame};

const MB: u64 = 1024 * 1024;
const HEAP_BYTES: u64 = 64 * MB;
const STACK_BYTES: u64 = 512 * 1024;
const THREAD_STACK_BYTES: usize = 2 * 1024 * 1024;
const MAX_CALLS: u32 = 500;
const MAX_CALL_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESULT_CHARS: usize = 30_000;
const MAX_MESSAGE_CHARS: usize = 4_000;
const MAX_UNREAD: usize = 20;
const MAX_LOG_BYTES: usize = 64 * 1024;

const PRELUDE_FILE: &str = "prelude.js";
const CODE_FILE: &str = "code.js";

const PRELUDE: &str = r#""use strict";
(() => {
  const host = globalThis.__host;
  delete globalThis.__host;
  const { stringify, parse } = JSON;
  const { freeze, defineProperty } = Object;
  const { apply } = Reflect;
  const then = Promise.prototype.then;
  const isArray = Array.isArray;
  const send = (message) => host(stringify(message));

  const pending = new Map();
  const failed = [];
  const read = new WeakSet();
  let next = 0;
  let started = false;
  let returned = null;
  let finished = false;
  let failure;

  class ToolCall extends Promise {
    static get [Symbol.species]() {
      return Promise;
    }
    then(onFulfilled, onRejected) {
      read.add(this);
      return apply(then, this, [onFulfilled, onRejected]);
    }
  }

  const render = (value) => {
    if (value === undefined) return null;
    if (typeof value === "string") return value;
    try {
      const text = stringify(value);
      if (text !== undefined) return text;
    } catch {}
    return String(value);
  };

  const fail = (entry, message, refused) => {
    entry.error.message = message;
    entry.error.tool = entry.name;
    if (refused) entry.error.refused = true;
    failed.push(entry);
    entry.reject(entry.error);
  };

  const call = (name, args) => {
    const entry = { name, error: new Error() };
    entry.promise = new ToolCall((resolve, reject) => {
      entry.resolve = resolve;
      entry.reject = reject;
    });
    if (args === undefined) args = {};
    if (args === null || typeof args !== "object" || isArray(args)) {
      fail(entry, `tools.${name} takes one object of arguments, such as { path }.`, false);
      return entry.promise;
    }
    const id = ++next;
    let text;
    try {
      text = stringify({ t: "call", id, tool: name, args });
    } catch (error) {
      fail(entry, `The arguments of tools.${name} aren't JSON: ${error.message}`, false);
      return entry.promise;
    }
    pending.set(id, entry);
    const answer = host(text);
    if (answer !== "") {
      pending.delete(id);
      fail(entry, answer, true);
    }
    return entry.promise;
  };

  const finish = () => {
    if (finished || returned === null || pending.size !== 0) return;
    finished = true;
    const unread = [];
    for (const entry of failed) {
      if (!read.has(entry.promise)) unread.push(`${entry.name}: ${entry.error.message}`);
    }
    send({ t: "done", value: returned.value, unread });
  };

  const fault = (error) => {
    failure = error;
    let refused = false;
    let tool = null;
    try {
      refused = error?.refused === true;
      tool = typeof error?.tool === "string" ? error.tool : null;
    } catch {}
    send({ t: "failed", refused, tool });
  };

  const run = (body) => {
    if (started) throw new TypeError("The code runs once.");
    started = true;
    apply(then, body(), [
      (value) => {
        try {
          returned = { value: render(value) };
          finish();
        } catch (error) {
          fault(error);
        }
      },
      fault,
    ]);
  };

  const settle = (text) => {
    const { id, ok, text: result, refused } = parse(text);
    const entry = pending.get(id);
    if (entry === undefined) return;
    pending.delete(id);
    if (ok) entry.resolve(result);
    else fail(entry, result, refused === true);
    finish();
  };

  const rethrow = () => {
    throw failure;
  };

  const log = (...args) => {
    let line = "";
    for (const arg of args) {
      const text = typeof arg === "string" ? arg : (render(arg) ?? "undefined");
      line = line === "" ? text : `${line} ${text}`;
    }
    send({ t: "log", line });
  };

  const fixed = (name, value) =>
    defineProperty(globalThis, name, { value, writable: false, enumerable: false, configurable: false });

  fixed("console", freeze({ log, info: log, warn: log, error: log, debug: log }));
  fixed("__run", run);
  fixed("__settle", settle);
  fixed("__rethrow", rethrow);
  defineProperty(globalThis, "__setup", {
    value: (text) => {
      delete globalThis.__setup;
      const tools = {};
      for (const name of parse(text)) tools[name] = (args) => call(name, args);
      fixed("tools", freeze(tools));
    },
    writable: false,
    enumerable: false,
    configurable: true,
  });
})();
"#;

// --- Minimal zero-dependency JSON parser & serializer for the prelude protocol ---

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    fn encode(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(raw) => out.push_str(raw),
            Json::Str(text) => write_json_str(text, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Json::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_str(k, out);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Number(raw) => raw.parse().ok(),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Json::Object(m) => Some(m),
            _ => None,
        }
    }
}

fn write_json_str(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn parse(input: &'a str) -> Result<Json, String> {
        let mut parser = Self {
            bytes: input.as_bytes(),
            pos: 0,
        };
        let value = parser.value()?;
        parser.skip_ws();
        if parser.pos != parser.bytes.len() {
            return Err("trailing characters".into());
        }
        Ok(value)
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => {
                self.expect(b"null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.expect(b"true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.expect(b"false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => self.string().map(Json::Str),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            other => Err(format!("unexpected byte {other:?} at {}", self.pos)),
        }
    }

    fn expect(&mut self, lit: &[u8]) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(format!("expected literal at {}", self.pos))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        while let Some(&b) = self.bytes.get(self.pos) {
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = *self
                        .bytes
                        .get(self.pos)
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\x08'),
                        b'f' => out.push('\x0c'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&code)
                                && self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                self.pos += 2;
                                let low = self.hex4()?;
                                let scalar = 0x10000
                                    + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                                out.push(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
                            } else {
                                out.push(char::from_u32(code as u32).unwrap_or(char::REPLACEMENT_CHARACTER));
                            }
                        }
                        _ => return Err(format!("invalid escape \\{}", esc as char)),
                    }
                }
                _ if b < 0x80 => out.push(b as char),
                _ => {
                    self.pos -= 1;
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| "invalid utf-8".to_string())?;
                    let ch = rest.chars().next().unwrap();
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Err("unterminated string".into())
    }

    fn hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("truncated \\u escape".into());
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "invalid hex".to_string())?;
        self.pos += 4;
        u16::from_str_radix(s, 16).map_err(|_| "invalid hex".to_string())
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.pos += 1;
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid number".to_string())?;
        Ok(Json::Number(s.to_string()))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err("expected , or ]".into()),
            }
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.pos += 1;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected object key".into());
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("expected :".into());
            }
            self.pos += 1;
            let val = self.value()?;
            map.insert(key, val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err("expected , or }".into()),
            }
        }
    }
}

// --- Arko host loop & protocol types ---

#[derive(Debug, Clone, Copy)]
struct Limits {
    compute_ms: u64,
    heap_bytes: u64,
    stack_bytes: u64,
    calls: u32,
}

struct Start {
    code: String,
    tools: Vec<String>,
    limits: Limits,
}

struct Call {
    id: u64,
    tool: String,
    args: Json,
}

struct Answer {
    id: u64,
    ok: bool,
    text: String,
    refused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ended {
    Returned {
        value: Option<String>,
        unread: Vec<String>,
    },
    Threw {
        message: String,
        line: Option<u32>,
        compiling: bool,
        refused: bool,
        tool: Option<String>,
    },
    OutOfMemory {
        line: Option<u32>,
    },
    ComputeLimit,
    CallLimit,
    Unsettled,
    Failed {
        message: String,
    },
}

#[derive(Debug)]
struct Done {
    ended: Ended,
    log: Vec<String>,
    log_left_out: u64,
    calls: u32,
    heap_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    ComputeLimit,
    CallLimit,
    Asked,
}

#[derive(Default)]
struct State {
    log: Vec<String>,
    log_bytes: usize,
    log_left_out: u64,
    calls: u32,
    outstanding: u32,
    done: Option<Ended>,
    failed: Option<(bool, Option<String>)>,
    garbled: Option<String>,
}

struct Clock {
    limit: Duration,
    spent: Cell<Duration>,
    since: Cell<Option<Instant>>,
    stopped: Cell<Option<Stopped>>,
}

impl Clock {
    fn used(&self) -> Duration {
        self.spent.get()
            + self
                .since
                .get()
                .map_or(Duration::ZERO, |since| since.elapsed())
    }

    fn computing<T>(&self, f: impl FnOnce() -> T) -> T {
        self.since.set(Some(Instant::now()));
        let result = f();
        self.spent.set(self.used());
        self.since.set(None);
        result
    }

    fn waiting<T>(&self, f: impl FnOnce() -> T) -> T {
        let computing = self.since.get().is_some();
        if computing {
            self.spent.set(self.used());
            self.since.set(None);
        }
        let result = f();
        if computing {
            self.since.set(Some(Instant::now()));
        }
        result
    }
}

struct Echo {
    calls: mpsc::Sender<Call>,
    answers: mpsc::Receiver<Answer>,
}

fn line_in(frames: &[Frame], file: &str) -> Option<u32> {
    frames
        .iter()
        .find(|frame| frame.file == file)
        .map(|frame| frame.line)
}

fn eval_strict(engine: &mut JsEngine, file: &str, source: &str) -> Result<(), EngineError> {
    engine.eval(
        source,
        EvalOptions {
            filename: file.into(),
            strict: true,
            module: false,
            annex_b: true,
        },
    )
}

fn run_host(start: &Start, calls: Echo, stop: Arc<AtomicBool>) -> Done {
    let limits = start.limits;
    let state = Rc::new(RefCell::new(State::default()));
    let clock = Rc::new(Clock {
        limit: Duration::from_millis(limits.compute_ms),
        spent: Cell::new(Duration::ZERO),
        since: Cell::new(None),
        stopped: Cell::new(None),
    });
    let calls = Rc::new(RefCell::new(calls));
    let polled = {
        let clock = clock.clone();
        move || {
            if clock.stopped.get().is_some() {
                return true;
            }
            let why = if stop.load(Ordering::Relaxed) {
                Some(Stopped::Asked)
            } else if clock.used() > clock.limit {
                Some(Stopped::ComputeLimit)
            } else {
                None
            };
            clock.stopped.set(why);
            why.is_some()
        }
    };
    let to_host = {
        let (state, clock, calls) = (state.clone(), clock.clone(), calls.clone());
        move |text: &str| hear(text, &state, &clock, &calls, limits)
    };
    let engine_limits = EngineLimits {
        heap_bytes: usize::try_from(limits.heap_bytes).unwrap_or(usize::MAX),
        stack_bytes: usize::try_from(limits.stack_bytes).unwrap_or(usize::MAX),
    };
    let (ended, heap_bytes) = match JsEngine::new(engine_limits).and_then(|mut engine| {
        engine.set_interrupt_handler(Some(Box::new(polled)));
        engine.install_host_bridge(Box::new(to_host))?;
        Ok(engine)
    }) {
        Ok(mut engine) => {
            let ended = drive(&mut engine, start, &state, &clock, &calls);
            let heap = engine.heap_bytes();
            (ended, heap)
        }
        Err(error) => (
            Ended::Failed {
                message: format!("The engine didn't start: {error:?}"),
            },
            0,
        ),
    };
    let state = state.take();
    Done {
        ended,
        log: state.log,
        log_left_out: state.log_left_out,
        calls: state.calls,
        heap_bytes,
    }
}

fn drive(
    engine: &mut JsEngine,
    start: &Start,
    state: &RefCell<State>,
    clock: &Clock,
    calls: &RefCell<Echo>,
) -> Ended {
    let names = Json::Array(start.tools.iter().cloned().map(Json::Str).collect()).encode();
    let setup = eval_strict(engine, PRELUDE_FILE, PRELUDE)
        .and_then(|()| engine.call_global("__setup", &names));
    if let Err(error) = setup {
        return Ended::Failed {
            message: format!("The prelude failed: {error:?}"),
        };
    }
    let wrapped = format!("__run(async () => {{{}\n}});", start.code);
    if let Err(error) = clock.computing(|| eval_strict(engine, CODE_FILE, &wrapped)) {
        return ended_by(error, true, clock);
    }
    loop {
        if let Err(error) = clock.computing(|| engine.run_jobs()) {
            return ended_by(error, false, clock);
        }
        if let Some(stopped) = clock.stopped.get() {
            return limit_ended(stopped);
        }
        let mut current = state.borrow_mut();
        if let Some(message) = current.garbled.take() {
            return Ended::Failed { message };
        }
        if let Some(done) = current.done.take() {
            return done;
        }
        if let Some((refused, tool)) = current.failed.take() {
            drop(current);
            return match clock.computing(|| engine.call_global("__rethrow", "")) {
                Err(EngineError::Thrown {
                    message: text,
                    frames,
                }) => Ended::Threw {
                    line: line_in(&frames, CODE_FILE),
                    message: message(text),
                    compiling: false,
                    refused,
                    tool,
                },
                Err(error) => ended_by(error, false, clock),
                Ok(()) => Ended::Failed {
                    message: "The code failed with nothing thrown.".into(),
                },
            };
        }
        if current.outstanding == 0 {
            return Ended::Unsettled;
        }
        drop(current);
        let Some(answer) = calls.borrow_mut().answers.recv().ok() else {
            return Ended::Failed {
                message: "Arko stopped answering.".into(),
            };
        };
        state.borrow_mut().outstanding -= 1;
        let mut map = BTreeMap::new();
        map.insert("id".into(), Json::Number(answer.id.to_string()));
        map.insert("ok".into(), Json::Bool(answer.ok));
        map.insert("text".into(), Json::Str(answer.text));
        map.insert("refused".into(), Json::Bool(answer.refused));
        let text = Json::Object(map).encode();
        if let Err(error) = clock.computing(|| engine.call_global("__settle", &text)) {
            return ended_by(error, false, clock);
        }
    }
}

fn hear(
    text: &str,
    state: &RefCell<State>,
    clock: &Clock,
    calls: &RefCell<Echo>,
    limits: Limits,
) -> String {
    let too_long = text.len() > MAX_CALL_BYTES;
    let parsed = match JsonParser::parse(text) {
        Ok(v) => v,
        Err(error) => {
            state.borrow_mut().garbled =
                Some(format!("The prelude said something unreadable ({error})."));
            return String::new();
        }
    };
    let Some(obj) = parsed.as_object() else {
        state.borrow_mut().garbled = Some("expected object from prelude".into());
        return String::new();
    };
    let tag = obj.get("t").and_then(Json::as_str).unwrap_or("");
    let mut state = state.borrow_mut();
    match tag {
        "call" => {
            let id = obj.get("id").and_then(Json::as_u64).unwrap_or(0);
            let tool = obj
                .get("tool")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            let args = obj.get("args").cloned().unwrap_or(Json::Null);
            if state.calls >= limits.calls {
                clock.stopped.set(Some(Stopped::CallLimit));
                return format!(
                    "The code made {} tool calls, the most a run may make.",
                    limits.calls
                );
            }
            if too_long {
                return format!(
                    "The arguments of tools.{tool} are {} MB, over the {} MB a call may send.",
                    (text.len() + 512 * 1024) / (1024 * 1024),
                    MAX_CALL_BYTES / (1024 * 1024)
                );
            }
            state.calls += 1;
            if clock
                .waiting(|| calls.borrow_mut().calls.send(Call { id, tool, args }))
                .is_err()
            {
                return "Arko stopped taking calls.".into();
            }
            state.outstanding += 1;
        }
        "log" => {
            let line = obj
                .get("line")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            if state.log_bytes + line.len() <= MAX_LOG_BYTES {
                state.log_bytes += line.len();
                state.log.push(line);
            } else {
                state.log_left_out += 1;
            }
        }
        "done" => {
            let value = obj.get("value").and_then(Json::as_str).map(|s| cut(s, MAX_RESULT_CHARS).0);
            let unread = obj
                .get("unread")
                .and_then(Json::as_array)
                .map(|arr| {
                    arr.iter()
                        .take(MAX_UNREAD)
                        .filter_map(Json::as_str)
                        .map(|line| cut(line, MAX_MESSAGE_CHARS).0)
                        .collect()
                })
                .unwrap_or_default();
            state.done = Some(Ended::Returned { value, unread });
        }
        "failed" => {
            let refused = obj.get("refused").and_then(Json::as_bool).unwrap_or(false);
            let tool = obj.get("tool").and_then(Json::as_str).map(String::from);
            state.failed = Some((refused, tool));
        }
        other => {
            state.garbled = Some(format!("unknown message tag {other}"));
        }
    }
    String::new()
}

fn ended_by(error: EngineError, compiling: bool, clock: &Clock) -> Ended {
    if let Some(stopped) = clock.stopped.get() {
        return limit_ended(stopped);
    }
    match error {
        EngineError::Thrown {
            message: text,
            frames,
        } => Ended::Threw {
            line: line_in(&frames, CODE_FILE),
            message: message(text),
            compiling,
            refused: false,
            tool: None,
        },
        EngineError::OutOfMemory => Ended::OutOfMemory { line: None },
        EngineError::Interrupted => Ended::Failed {
            message: "The engine stopped the code without a reason.".into(),
        },
        EngineError::Failed(message) => Ended::Failed { message },
    }
}

fn cut(text: &str, most: usize) -> (String, u64) {
    match text.char_indices().nth(most) {
        Some((end, _)) => (text[..end].to_string(), text[end..].chars().count() as u64),
        None => (text.to_string(), 0),
    }
}

fn message(text: String) -> String {
    match cut(&text, MAX_MESSAGE_CHARS) {
        (kept, 0) => kept,
        (kept, left_out) => format!("{kept} [{left_out} more characters left out]"),
    }
}

fn limit_ended(stopped: Stopped) -> Ended {
    match stopped {
        Stopped::ComputeLimit => Ended::ComputeLimit,
        Stopped::CallLimit => Ended::CallLimit,
        Stopped::Asked => Ended::Failed {
            message: "The run was stopped.".into(),
        },
    }
}

struct Ran {
    done: Done,
    took: Duration,
    calls: u32,
}

fn limits(compute: Duration) -> Limits {
    Limits {
        compute_ms: compute.as_millis() as u64,
        heap_bytes: HEAP_BYTES,
        stack_bytes: STACK_BYTES,
        calls: MAX_CALLS,
    }
}

fn run_with(code: &str, limits: Limits, stop: Arc<AtomicBool>) -> Ran {
    let (calls, incoming) = mpsc::channel::<Call>();
    let (answer, answers) = mpsc::channel();
    let start = Start {
        code: code.into(),
        tools: ["echo", "slow", "fail", "refuse"]
            .map(String::from)
            .to_vec(),
        limits,
    };
    let sent = Arc::new(AtomicU32::new(0));
    let host = std::thread::spawn({
        let sent = sent.clone();
        move || {
            for call in incoming {
                sent.fetch_add(1, Ordering::SeqCst);
                let text = call.args.encode();
                let reply = match call.tool.as_str() {
                    "fail" => Answer {
                        id: call.id,
                        ok: false,
                        text: "the tool broke".into(),
                        refused: false,
                    },
                    "refuse" => Answer {
                        id: call.id,
                        ok: false,
                        text: "Refused: not here.".into(),
                        refused: true,
                    },
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(300));
                        Answer {
                            id: call.id,
                            ok: true,
                            text,
                            refused: false,
                        }
                    }
                    _ => Answer {
                        id: call.id,
                        ok: true,
                        text,
                        refused: false,
                    },
                };
                if answer.send(reply).is_err() {
                    break;
                }
            }
        }
    });
    let began = Instant::now();
    let done = std::thread::Builder::new()
        .name("arko-code".into())
        .stack_size(THREAD_STACK_BYTES)
        .spawn(move || run_host(&start, Echo { calls, answers }, stop))
        .unwrap()
        .join()
        .unwrap();
    let took = began.elapsed();
    host.join().unwrap();
    Ran {
        done,
        took,
        calls: sent.load(Ordering::SeqCst),
    }
}

fn run(code: &str) -> Ran {
    run_with(code, limits(Duration::from_secs(5)), Arc::default())
}

fn run_for(code: &str, compute: Duration) -> Ran {
    run_with(code, limits(compute), Arc::default())
}

fn returned(ran: &Ran) -> String {
    match &ran.done.ended {
        Ended::Returned { value, .. } => value.clone().unwrap_or_default(),
        other => panic!("{other:?}"),
    }
}

#[cfg(feature = "gc_stress")]
#[test]
fn code_mode_survives_collection_across_host_turns() {
    for fail in [false, true] {
        let messages = Rc::new(RefCell::new(Vec::new()));
        let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
        engine
            .install_host_bridge(Box::new({
                let messages = messages.clone();
                move |text| {
                    messages.borrow_mut().push(text.to_owned());
                    String::new()
                }
            }))
            .unwrap();
        eval_strict(&mut engine, PRELUDE_FILE, PRELUDE).unwrap();
        engine.call_global("__setup", r#"["timer"]"#).unwrap();
        engine.collect_garbage();
        eval_strict(
            &mut engine,
            CODE_FILE,
            r#"__run(async () => {
                if (['__host', '__setup', 'setTimeout', 'fetch', 'process'].some(k => k in globalThis)) {
                    throw new Error('unexpected global');
                }
                const live = new Map([['answer', {value: 42}]]);
                await new Promise(resolve => queueMicrotask(resolve));
                const tick = await tools.timer({delay: 0});
                console.log(tick);
                return live.get('answer');
            });"#,
        )
        .unwrap();
        engine.run_jobs().unwrap();
        assert_eq!(
            messages.borrow().as_slice(),
            [r#"{"t":"call","id":1,"tool":"timer","args":{"delay":0}}"#]
        );

        // The host advances its timer by delivering the reply on a later turn.
        engine.collect_garbage();
        let answer = if fail {
            r#"{"id":1,"ok":false,"text":"timer failed","refused":true}"#
        } else {
            r#"{"id":1,"ok":true,"text":"tick","refused":false}"#
        };
        engine.call_global("__settle", answer).unwrap();
        engine.collect_garbage();
        engine.run_jobs().unwrap();
        engine.collect_garbage();

        if fail {
            assert_eq!(
                &messages.borrow()[1..],
                [r#"{"t":"failed","refused":true,"tool":"timer"}"#]
            );
            assert!(matches!(
                engine.call_global("__rethrow", ""),
                Err(EngineError::Thrown { message, .. }) if message == "Error: timer failed"
            ));
        } else {
            assert_eq!(
                &messages.borrow()[1..],
                [r#"{"t":"log","line":"tick"}"#, r#"{"t":"done","value":"{\"value\":42}","unread":[]}"#]
            );
        }
    }
}

#[test]
fn the_code_returns_a_value() {
    assert_eq!(returned(&run("return 6 * 7;")), "42");
    assert_eq!(returned(&run("return 'two\\nlines';")), "two\nlines");
    assert_eq!(returned(&run("return 10n;")), "10");
    let ran = run("return { m: new Map([[1, 2]]), d: new Date(0), u: undefined };");
    assert_eq!(returned(&ran), r#"{"m":{},"d":"1970-01-01T00:00:00.000Z"}"#);
    assert!(matches!(
        run("const x = 1;").done.ended,
        Ended::Returned { value: None, .. }
    ));
}

#[test]
fn a_thrown_error_names_its_line_in_the_code() {
    let ran = run("const a = 1;\nconst b = 2;\nthrow new Error('boom');");
    assert_eq!(
        ran.done.ended,
        Ended::Threw {
            message: "Error: boom".into(),
            line: Some(3),
            compiling: false,
            refused: false,
            tool: None,
        }
    );
    for (thrown, message) in [
        ("'a string'", "a string"),
        ("null", "null"),
        ("{ n: 1 }", r#"{"n":1}"#),
    ] {
        let ran = run(&format!("throw {thrown};"));
        assert!(
            matches!(&ran.done.ended, Ended::Threw { message: got, .. } if got == message),
            "{thrown}: {:?}",
            ran.done.ended
        );
    }
}

#[test]
fn a_syntax_error_and_typescript_fail_to_compile() {
    let ran = run("const n = 1;\nconst m: number = 2;\nreturn n;");
    match ran.done.ended {
        Ended::Threw {
            message,
            line,
            compiling,
            ..
        } => {
            assert!(message.starts_with("SyntaxError"), "{message}");
            assert_eq!(line, Some(2));
            assert!(compiling);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_deadline_stops_a_loop_inside_try_in_a_regex_and_after_an_await() {
    for code in [
        "while (true) {}",
        "try { while (true) {} } catch (e) { return 'caught'; } finally { return 'finally'; }",
        "const r = /^(a+)+$/.test('a'.repeat(40) + 'b'); return r;",
        "await tools.echo({}); while (true) {}",
        "await Promise.resolve(); for (;;) { try { while (true) {} } catch {} }",
    ] {
        let ran = run_for(code, Duration::from_millis(200));
        assert_eq!(ran.done.ended, Ended::ComputeLimit, "{code}");
        assert!(ran.took < Duration::from_secs(2), "{code}: {:?}", ran.took);
    }
}

#[test]
fn waiting_for_a_tool_isnt_computing() {
    let ran = run_for(
        "await tools.slow({}); await tools.slow({}); return 'waited';",
        Duration::from_millis(200),
    );
    assert_eq!(returned(&ran), "waited");
    assert!(ran.took >= Duration::from_millis(600));
}

#[test]
fn the_heap_cap_ends_a_run_and_code_may_catch_it() {
    let mut small = limits(Duration::from_secs(20));
    small.heap_bytes = 32 * MB;
    let ran = run_with(
        "const all = []; while (true) { all.push(new Array(100000).fill(7)); }",
        small,
        Arc::default(),
    );
    assert!(
        matches!(&ran.done.ended, Ended::OutOfMemory { .. })
            || matches!(&ran.done.ended, Ended::Threw { message, .. } if message == "null"),
        "{:?}",
        ran.done.ended
    );
    assert!(ran.done.heap_bytes <= 32 * MB);
    let ran = run_with(
        "try { return new Array(1e9).fill(0).length; } catch (e) { return 'threw ' + e.message; }",
        small,
        Arc::default(),
    );
    assert_eq!(returned(&ran), "threw out of memory");
}

#[test]
fn the_stack_cap_ends_endless_recursion() {
    let ran = run("function f(n) { return f(n + 1) + 1; }\nreturn f(0);");
    match ran.done.ended {
        Ended::Threw { message, line, .. } => {
            assert_eq!(message, "RangeError: Maximum call stack size exceeded");
            assert_eq!(line, Some(1));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_promise_nothing_settles_ends_the_run() {
    let ran = run("await new Promise(() => {}); return 1;");
    assert_eq!(ran.done.ended, Ended::Unsettled);
}

#[test]
fn the_global_object_holds_only_the_allowed_names() {
    let names = returned(&run(
        "return Object.getOwnPropertyNames(globalThis).sort().join(' ');",
    ));
    assert_eq!(
        names,
        "AggregateError Array ArrayBuffer AsyncDisposableStack Atomics BigInt BigInt64Array \
         BigUint64Array Boolean DOMException DataView Date DisposableStack Error EvalError \
         FinalizationRegistry Float16Array Float32Array Float64Array Function Infinity \
         Int16Array Int32Array Int8Array InternalError Iterator JSON Map Math NaN Number Object \
         Promise Proxy RangeError ReferenceError Reflect RegExp Set SharedArrayBuffer String \
         SuppressedError Symbol SyntaxError TypeError URIError Uint16Array Uint32Array \
         Uint8Array Uint8ClampedArray WeakMap WeakRef WeakSet __rethrow __run __settle atob btoa \
         console decodeURI decodeURIComponent encodeURI encodeURIComponent escape eval \
         globalThis isFinite isNaN parseFloat parseInt performance queueMicrotask tools \
         undefined unescape"
    );
    let probes = returned(&run(
        "return [typeof require, typeof fetch, typeof std, typeof os, typeof process, typeof setTimeout, typeof print, typeof WebAssembly, typeof __host, typeof __setup].join(' ');",
    ));
    assert_eq!(probes, "undefined ".repeat(10).trim_end());
}

#[test]
fn import_rejects_and_atomics_wait_throws() {
    let ran = run("try { await import('fs'); return 'loaded'; } catch (e) { return 'rejected'; }");
    assert_eq!(returned(&ran), "rejected");
    let ran = run(
        "try { return Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0); } catch (e) { return 'threw'; }",
    );
    assert_eq!(returned(&ran), "threw");
}

#[test]
fn the_entry_points_and_tools_cant_be_replaced() {
    let ran = run(
        "const hits = [];\n\
         for (const replace of [\n\
           () => { globalThis.__settle = 5; },\n\
           () => { delete globalThis.__run; },\n\
           () => { globalThis.tools = {}; },\n\
           () => { tools.echo = () => 1; },\n\
           () => { console.log = () => 1; },\n\
           () => { Object.defineProperty(globalThis, '__rethrow', { value: 1 }); },\n\
         ]) {\n\
           try { replace(); hits.push('replaced'); } catch (e) { hits.push(e.constructor.name); }\n\
         }\n\
         hits.push(await tools.echo({ a: 1 }));\n\
         return hits.join(' ');",
    );
    assert_eq!(
        returned(&ran),
        r#"TypeError TypeError TypeError TypeError TypeError TypeError {"a":1}"#
    );
}

#[test]
fn the_call_cap_counts_calls_as_they_are_made() {
    let ran = run("for (let i = 0; i < 600; i++) tools.echo({ i }); return 'done';");
    assert_eq!(ran.done.ended, Ended::CallLimit);
    assert_eq!(ran.calls, 500);
    assert_eq!(ran.done.calls, 500);
}

#[test]
fn a_failed_call_rejects_with_the_tools_message_and_can_be_caught() {
    let ran = run(
        "const seen = [];\n\
         for (const name of ['fail', 'refuse']) {\n\
           try { await tools[name]({}); } catch (e) { seen.push(`${e.message} ${e.refused === true} ${e.tool}`); }\n\
         }\n\
         return seen.join(' | ');",
    );
    assert_eq!(
        returned(&ran),
        "the tool broke false fail | Refused: not here. true refuse"
    );
}

#[test]
fn an_uncaught_failed_call_names_the_line_of_the_call() {
    let ran = run("const x = 1;\nawait tools.refuse({ x });");
    assert_eq!(
        ran.done.ended,
        Ended::Threw {
            message: "Error: Refused: not here.".into(),
            line: Some(2),
            compiling: false,
            refused: true,
            tool: Some("refuse".into()),
        }
    );
}

#[test]
fn calls_not_awaited_finish_first_and_their_failures_are_reported() {
    let ran = run(
        "tools.slow({ n: 1 });\n\
         tools.fail({});\n\
         try { await tools.fail({}); } catch {}\n\
         return 'went on';",
    );
    assert_eq!(ran.calls, 3);
    assert_eq!(
        ran.done.ended,
        Ended::Returned {
            value: Some("went on".into()),
            unread: vec!["fail: the tool broke".into()],
        }
    );
    let ran = run("try { await tools.echo('src'); } catch (e) { return e.message; }");
    assert_eq!(
        returned(&ran),
        "tools.echo takes one object of arguments, such as { path }."
    );
}

#[test]
fn console_lines_come_before_the_value_and_survive_a_failure() {
    let ran =
        run("console.log('a', 1, { b: 2 }, [3], undefined);\nconsole.error('e');\nreturn [1, 2];");
    assert_eq!(ran.done.log, ["a 1 {\"b\":2} [3] undefined", "e"]);
    assert_eq!(returned(&ran), "[1,2]");
    let ran = run_for(
        "console.log('before');\nwhile (true) {}",
        Duration::from_millis(100),
    );
    assert_eq!(ran.done.ended, Ended::ComputeLimit);
    assert_eq!(ran.done.log, ["before"]);
    let ran = run_for(
        "for (let i = 0; i < 100000; i++) console.log('x'.repeat(10));\nreturn 1;",
        Duration::from_secs(120),
    );
    assert_eq!(returned(&ran), "1");
    assert_eq!(ran.done.log.len(), MAX_LOG_BYTES / 10);
    assert_eq!(ran.done.log_left_out, 100_000 - (MAX_LOG_BYTES / 10) as u64);
}

#[test]
fn a_stop_from_outside_ends_a_run() {
    let stop = Arc::new(AtomicBool::new(false));
    let setter = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop.store(true, Ordering::SeqCst);
        })
    };
    let ran = run_with("while (true) {}", limits(Duration::from_secs(20)), stop);
    setter.join().unwrap();
    assert!(
        matches!(ran.done.ended, Ended::Failed { ref message } if message == "The run was stopped."),
        "{:?}",
        ran.done.ended
    );
    assert!(ran.took < Duration::from_secs(2));
}

#[test]
fn many_calls_one_after_another() {
    let ran = run(
        "let n = 0; for (let i = 0; i < 400; i++) { n += JSON.parse(await tools.echo({ i })).i; } return n;",
    );
    assert_eq!(returned(&ran), "79800");
    assert_eq!(ran.calls, 400);
}
