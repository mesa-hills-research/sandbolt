use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use qjs_runtime::api::JsEngine;
use qjs_runtime::builtins::make_native_fn;
use qjs_runtime::ops::convert::{to_string, to_uint32};
use qjs_runtime::ops::property::{PropertyDescriptor, define_own_property};
use qjs_runtime::realm::NativeFn;
use qjs_runtime::{Args, ExecCtx, Handle, JsResult, NativeFnId, Value, ValueKind};

#[cfg(feature = "integrated-runtime")]
mod agents;

const COMPLETE: &str = "Test262:AsyncTestComplete";
const FAILURE: &str = "Test262:AsyncTestFailure";

#[derive(Default)]
struct Printed {
    completions: usize,
    failure: Option<String>,
    output: Vec<String>,
}

impl Printed {
    fn observe(&mut self, args: Vec<String>, capture: bool) {
        for value in &args {
            if value == COMPLETE {
                self.completions += 1;
            } else if value.starts_with(FAILURE) && self.failure.is_none() {
                self.failure = Some(value.clone());
            }
        }
        if capture {
            self.output.push(args.join(" "));
        }
    }
}

#[derive(Clone)]
pub(crate) struct Budget {
    pub start: Instant,
    pub timeout: Duration,
}

impl Budget {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            start: Instant::now(),
            timeout,
        }
    }

    fn remaining(&self) -> Duration {
        self.timeout.saturating_sub(self.start.elapsed())
    }

    pub(crate) fn expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

struct Host {
    budget: Budget,
    capture: bool,
    printed: RefCell<Printed>,
    #[cfg(feature = "integrated-runtime")]
    agents: agents::Group,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Host>>> = const { RefCell::new(None) };
}

// Native dispatch is a static function table. Each test owns its thread's host
// state for as long as its runtime can enter that table.
pub(crate) struct Session {
    host: Rc<Host>,
}

impl Session {
    pub(crate) fn new(budget: Budget, capture: bool) -> Self {
        Self::enter(Host {
            budget,
            capture,
            printed: RefCell::default(),
            #[cfg(feature = "integrated-runtime")]
            agents: agents::Group::main(),
        })
    }

    fn enter(host: Host) -> Self {
        let host = Rc::new(host);
        assert!(CURRENT.replace(Some(host.clone())).is_none());
        Self { host }
    }

    pub(crate) fn install(&self, engine: &mut JsEngine) -> JsResult<()> {
        let realm = engine.default_realm();
        let runtime = engine.runtime_mut();
        runtime.set_native_dispatch(&DISPATCH);
        runtime.with_scope(realm, install).map(|_| ())
    }

    pub(crate) fn reset_async(&self) {
        let mut printed = self.host.printed.borrow_mut();
        printed.completions = 0;
        printed.failure = None;
    }

    pub(crate) fn async_result(&self) -> Result<(), Option<String>> {
        let printed = self.host.printed.borrow();
        if printed.completions == 1 && printed.failure.is_none() {
            Ok(())
        } else {
            Err(printed.failure.clone())
        }
    }

    pub(crate) fn output(&self) -> Vec<String> {
        std::mem::take(&mut self.host.printed.borrow_mut().output)
    }

    pub(crate) fn note(&self, message: String) {
        self.host.printed.borrow_mut().output.push(message);
    }

    pub(crate) fn finish(&self) {
        #[cfg(feature = "integrated-runtime")]
        {
            let output = self.host.agents.finish(false);
            self.host.printed.borrow_mut().output.extend(output);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        #[cfg(feature = "integrated-runtime")]
        self.host.agents.finish(true);
        CURRENT.replace(None);
    }
}

#[derive(Default)]
pub(crate) struct Countdown {
    pub enabled: Cell<bool>,
    pub fired: Cell<bool>,
}

pub(crate) fn interrupt(engine: &mut JsEngine, budget: Budget, countdown: Rc<Countdown>) {
    let mut remaining_polls = 150u32;
    let mut clock_tick = 0u32;
    engine.set_interrupt_handler(Some(Box::new(move || {
        if clock_tick == 0 && budget.expired() {
            return true;
        }
        clock_tick = (clock_tick + 1) & 255;
        if countdown.enabled.get() {
            remaining_polls = remaining_polls.saturating_sub(1);
            countdown.fired.set(remaining_polls == 0);
            return countdown.fired.get();
        }
        false
    })));
}

fn current() -> Rc<Host> {
    CURRENT.with_borrow(|host| host.as_ref().expect("test262 host session").clone())
}

#[derive(Clone, Copy)]
#[repr(u16)]
enum Id {
    Print,
    Gc,
    CreateRealm,
    EvalScript,
    Detach,
    IsHtmlDda,
    CodePointRange,
    Start,
    Broadcast,
    GetReport,
    Sleep,
    MonotonicNow,
    Leaving,
    ReceiveBroadcast,
    Report,
}

static DISPATCH: [NativeFn; 15] = [
    print,
    gc,
    create_realm,
    eval_script,
    detach,
    is_html_dda,
    code_point_range,
    agents::start,
    agents::broadcast,
    agents::get_report,
    sleep,
    monotonic_now,
    agents::leaving,
    agents::receive_broadcast,
    agents::report,
];

const AGENT_METHODS: &[(&str, Id, u32)] = &[
    ("start", Id::Start, 1),
    ("broadcast", Id::Broadcast, 2),
    ("getReport", Id::GetReport, 0),
    ("sleep", Id::Sleep, 1),
    ("monotonicNow", Id::MonotonicNow, 0),
    ("leaving", Id::Leaving, 0),
    ("receiveBroadcast", Id::ReceiveBroadcast, 1),
    ("report", Id::Report, 1),
];

fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<Value> {
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let host = ctx.new_object()?;
    let agent = ctx.new_object()?;
    for &(name, id, len) in AGENT_METHODS {
        method(ctx, agent, name, id, len, false)?;
    }
    for (name, id, len) in [
        ("createRealm", Id::CreateRealm, 0),
        ("evalScript", Id::EvalScript, 1),
        ("detachArrayBuffer", Id::Detach, 1),
        ("gc", Id::Gc, 0),
        ("codePointRange", Id::CodePointRange, 2),
    ] {
        method(ctx, host, name, id, len, false)?;
    }
    let dda = method(ctx, host, "IsHTMLDDA", Id::IsHtmlDda, 0, true)?;
    #[cfg(feature = "integrated-runtime")]
    ctx.set_is_html_dda(dda)?;
    #[cfg(not(feature = "integrated-runtime"))]
    let _ = dda;
    property(ctx, host, "agent", agent, true)?;
    property(ctx, host, "global", global, true)?;
    method(ctx, global, "print", Id::Print, 1, true)?;
    method(ctx, global, "gc", Id::Gc, 0, true)?;
    property(ctx, global, "$262", host, true)?;
    Ok(ctx.get(host))
}

fn method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    id: Id,
    length: u32,
    enumerable: bool,
) -> JsResult<Handle<'s>> {
    let value = make_native_fn(ctx, NativeFnId(id as u16), name, length, false)?;
    property(ctx, object, name, value, enumerable)?;
    Ok(value)
}

fn property<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    object: Handle<'s>,
    name: &str,
    value: Handle<'s>,
    enumerable: bool,
) -> JsResult<()> {
    let key = ctx.intern_str(name)?;
    define_own_property(
        ctx,
        object,
        key,
        PropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(enumerable),
            configurable: Some(true),
            ..PropertyDescriptor::default()
        },
    )?;
    Ok(())
}

fn argument<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, index: usize) -> JsResult<Handle<'s>> {
    args.get(index)
        .map_or_else(|| ctx.root(Value::UNDEFINED), Ok)
}

fn string_arg<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<String> {
    Ok(String::from_utf16_lossy(&units_arg(ctx, value)?))
}

fn units_arg<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<Vec<u16>> {
    let value = to_string(ctx, value)?;
    let ValueKind::String(string) = ctx.get(value).decode() else {
        unreachable!()
    };
    Ok(ctx.heap().string_units(string).collect())
}

fn print<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let host = current();
    let mut values = Vec::with_capacity(args.len());
    for index in 0..args.len() {
        values.push(string_arg(ctx, args.get(index).unwrap())?);
    }
    host.printed.borrow_mut().observe(values, host.capture);
    Ok(Value::UNDEFINED)
}

fn gc<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    ctx.collect();
    Ok(Value::UNDEFINED)
}

#[cfg(feature = "integrated-runtime")]
fn create_realm<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    let realm = ctx.create_realm_with_builtins()?;
    ctx.with_realm(realm, install)
}

#[cfg(feature = "integrated-runtime")]
fn eval_script<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let arg = argument(ctx, args, 0)?;
    let source = string_arg(ctx, arg)?;
    qjs_runtime::api::eval_script(
        ctx,
        &source,
        qjs_runtime::api::EvalOptions {
            filename: "<evalScript>".into(),
            ..Default::default()
        },
    )
}

#[cfg(feature = "integrated-runtime")]
fn detach<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    use qjs_runtime::object::ObjectData;
    let value = argument(ctx, args, 0)?;
    if let Some(id) = ctx.get(value).as_object()
        && matches!(ctx.heap().object(id).data, ObjectData::ArrayBuffer(_))
    {
        qjs_runtime::builtins::typed_atomics::detach_array_buffer(ctx, id)?;
    }
    Ok(Value::UNDEFINED)
}

fn is_html_dda<'s>(_: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    Ok(Value::NULL)
}

fn code_point_range<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let start = argument(ctx, args, 0)?;
    let start = to_uint32(ctx, start)?;
    let end = argument(ctx, args, 1)?;
    let end = to_uint32(ctx, end)?.min(0x11_0000);
    let mut units = Vec::new();
    for code in start.min(end)..end {
        if code < 0x1_0000 {
            units.push(code as u16);
        } else {
            let code = code - 0x1_0000;
            units.push(0xd800 | (code >> 10) as u16);
            units.push(0xdc00 | (code & 0x3ff) as u16);
        }
    }
    let string = ctx.new_utf16(&units)?;
    Ok(ctx.get(string))
}

fn sleep<'s>(ctx: &mut ExecCtx<'_, 's>, _: Handle<'s>, args: Args<'s>) -> JsResult<Value> {
    let millis = argument(ctx, args, 0)?;
    let delay = Duration::from_millis(u64::from(to_uint32(ctx, millis)?));
    let remaining = current().budget.remaining();
    std::thread::sleep(delay.min(remaining));
    ctx.check_interrupt()?;
    Ok(Value::UNDEFINED)
}

fn monotonic_now<'s>(_: &mut ExecCtx<'_, 's>, _: Handle<'s>, _: Args<'s>) -> JsResult<Value> {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    Ok(Value::number(
        EPOCH.get_or_init(Instant::now).elapsed().as_millis() as f64,
    ))
}

#[cfg(not(feature = "integrated-runtime"))]
mod agents {
    use super::*;

    pub(super) fn unavailable<'s>(
        ctx: &mut ExecCtx<'_, 's>,
        _: Handle<'s>,
        _: Args<'s>,
    ) -> JsResult<Value> {
        Err(ctx.throw_type_error("test262 needs the integrated runtime"))
    }

    pub(super) use unavailable as broadcast;
    pub(super) use unavailable as get_report;
    pub(super) use unavailable as leaving;
    pub(super) use unavailable as receive_broadcast;
    pub(super) use unavailable as report;
    pub(super) use unavailable as start;
}

#[cfg(not(feature = "integrated-runtime"))]
use agents::unavailable as create_realm;
#[cfg(not(feature = "integrated-runtime"))]
use agents::unavailable as detach;
#[cfg(not(feature = "integrated-runtime"))]
use agents::unavailable as eval_script;

#[cfg(test)]
mod tests {
    use super::*;
    use qjs_runtime::ops::{call::call, property::get_property};

    #[test]
    fn installs_global_and_captures_async_prints() {
        let mut engine = JsEngine::new(Default::default()).unwrap();
        let session = Session::new(Budget::new(Duration::from_secs(1)), true);
        session.install(&mut engine).unwrap();
        let realm = engine.default_realm();
        engine.runtime_mut().with_scope(realm, |ctx| {
            fn property<'s>(ctx: &mut ExecCtx<'_, 's>, object: Handle<'s>, key: &str) -> Value {
                let key = ctx.intern_str(key).unwrap();
                get_property(ctx, object, key).unwrap()
            }
            let global = ctx.root(Value::object(ctx.realm().global_object)).unwrap();
            let host = property(ctx, global, "$262");
            let host = ctx.root(host).unwrap();
            assert_eq!(property(ctx, host, "global"), ctx.get(global));
            let print = property(ctx, global, "print");
            let print = ctx.root(print).unwrap();
            let label = ctx.new_string("label").unwrap();
            let complete = ctx.new_string(COMPLETE).unwrap();
            let failure = ctx.new_string("Test262:AsyncTestFailure:Oops").unwrap();
            let args = ctx.args(&[label, complete]).unwrap();
            assert_eq!(call(ctx, print, global, args), Ok(Value::UNDEFINED));
            assert_eq!(session.async_result(), Ok(()));
            assert_eq!(call(ctx, print, global, args), Ok(Value::UNDEFINED));
            assert_eq!(session.async_result(), Err(None));
            session.reset_async();
            let args = ctx.args(&[failure, complete]).unwrap();
            assert_eq!(call(ctx, print, global, args), Ok(Value::UNDEFINED));
            assert_eq!(
                session.async_result(),
                Err(Some("Test262:AsyncTestFailure:Oops".into()))
            );
        });
        assert_eq!(
            session.output(),
            [
                "label Test262:AsyncTestComplete",
                "label Test262:AsyncTestComplete",
                "Test262:AsyncTestFailure:Oops Test262:AsyncTestComplete"
            ]
        );
    }

    #[test]
    fn host_interrupts_and_range() {
        let mut engine = JsEngine::new(Default::default()).unwrap();
        let budget = Budget::new(Duration::from_secs(1));
        let countdown = Rc::new(Countdown::default());
        let session = Session::new(budget.clone(), false);
        session.install(&mut engine).unwrap();
        interrupt(&mut engine, budget, countdown.clone());
        let realm = engine.default_realm();
        engine.runtime_mut().with_scope(realm, |ctx| {
            let this = ctx.root(Value::UNDEFINED).unwrap();
            let start = ctx.root(Value::int32(0x1f600)).unwrap();
            let end = ctx.root(Value::int32(0x1f602)).unwrap();
            let args = ctx.args(&[start, end]).unwrap();
            let range = code_point_range(ctx, this, args).unwrap();
            let ValueKind::String(id) = range.decode() else {
                panic!("not a string")
            };
            let units: Vec<_> = ctx.heap().string_units(id).collect();
            assert_eq!(String::from_utf16(&units).unwrap(), "😀😁");
            let args = ctx.args(&[end, start]).unwrap();
            let empty = code_point_range(ctx, this, args).unwrap();
            let ValueKind::String(id) = empty.decode() else {
                panic!("not a string")
            };
            assert_eq!(ctx.heap().string_units(id).count(), 0);
            for _ in 0..151 {
                ctx.check_interrupt().unwrap();
            }
            countdown.enabled.set(true);
            for _ in 0..149 {
                ctx.check_interrupt().unwrap();
            }
            assert_eq!(ctx.check_interrupt(), Err(qjs_runtime::Abrupt::Interrupted));
            assert!(countdown.fired.get());
        });
    }

    #[cfg(feature = "integrated-runtime")]
    fn run_script(source: &str) {
        let mut engine = JsEngine::new(Default::default()).unwrap();
        let budget = Budget::new(Duration::from_secs(5));
        let session = Session::new(budget.clone(), true);
        session.install(&mut engine).unwrap();
        interrupt(&mut engine, budget, Rc::default());
        engine.eval(source, Default::default()).unwrap();
        session.finish();
        assert!(session.output().is_empty());
    }

    #[cfg(feature = "integrated-runtime")]
    #[test]
    fn realms_html_dda_and_detach() {
        run_script(
            r#"
            const other = $262.createRealm();
            if (other.global === $262.global || other.global.$262 !== other) throw 'realm';
            if (other.evalScript('var onlyThere = 3; onlyThere + 4') !== 7) throw 'eval';
            if (other.global.onlyThere !== 3 || 'onlyThere' in $262.global) throw 'global';
            if (typeof $262.IsHTMLDDA !== 'undefined' || $262.IsHTMLDDA != null) throw 'type';
            if ($262.IsHTMLDDA() !== null || $262.IsHTMLDDA) throw 'call';
            const ab = new ArrayBuffer(9);
            $262.detachArrayBuffer(ab);
            $262.detachArrayBuffer(ab);
            if (ab.byteLength !== 0) throw 'detach';
            $262.gc();
        "#,
        );
    }

    #[cfg(feature = "integrated-runtime")]
    #[test]
    fn shares_a_buffer_with_four_agents() {
        run_script(
            r#"
            const agent = $262.agent;
            const buffer = new SharedArrayBuffer(4);
            const view = new Int32Array(buffer);
            for (let i = 0; i < 4; ++i) {
                agent.start(`$262.agent.receiveBroadcast((shared, n) => {
                    const view = new Int32Array(shared);
                    for (let i = 0; i < n; ++i) Atomics.add(view, 0, 1);
                    $262.agent.report('done');
                    $262.agent.leaving();
                });`);
            }
            if (agent.getReport() !== null) throw 'early report';
            agent.broadcast(buffer, 128);
            let reports = 0;
            while (reports !== 4) {
                const report = agent.getReport();
                if (report === null) agent.sleep(1);
                else if (report === 'done') ++reports;
                else throw 'unexpected report';
            }
            if (Atomics.load(view, 0) !== 512 || agent.getReport() !== null) throw 'sharing';
        "#,
        );
    }
}
