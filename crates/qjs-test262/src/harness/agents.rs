use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use qjs_runtime::api::EvalOptions;
use qjs_runtime::builtins::typed_atomics::create_shared_array_buffer_from_arc;
use qjs_runtime::object::{ObjectData, SharedBufferBacking};
use qjs_runtime::ops::{
    call::{call, is_callable},
    convert::to_int32,
};
use qjs_runtime::value::RealmId;
use qjs_runtime::{Abrupt, Persistent};

use super::*;

pub(super) enum Group {
    Main(Main),
    Worker(Worker),
}

pub(super) struct Main {
    children: RefCell<Vec<Child>>,
    reports: Reports,
    cancelled: Arc<AtomicBool>,
}

pub(super) struct Worker {
    messages: Receiver<Broadcast>,
    reports: Reports,
    callback: RefCell<Option<(RealmId, Persistent)>>,
}

#[derive(Clone)]
struct Reports {
    send: Sender<Vec<u16>>,
    receive: Arc<Mutex<Receiver<Vec<u16>>>>,
}

struct Child {
    messages: Sender<Broadcast>,
    thread: JoinHandle<Result<Vec<String>, String>>,
}

struct Broadcast {
    backing: Arc<SharedBufferBacking>,
    value: i32,
    received: Sender<()>,
}

impl Group {
    fn reports(&self) -> &Reports {
        match self {
            Self::Main(main) => &main.reports,
            Self::Worker(worker) => &worker.reports,
        }
    }

    pub(super) fn main() -> Self {
        let (send, receive) = mpsc::channel();
        Self::Main(Main {
            children: RefCell::default(),
            reports: Reports {
                send,
                receive: Arc::new(Mutex::new(receive)),
            },
            cancelled: Arc::default(),
        })
    }

    pub(super) fn finish(&self, cancel: bool) -> Vec<String> {
        let Self::Main(main) = self else {
            return Vec::new();
        };
        if cancel {
            main.cancelled.store(true, Ordering::Relaxed);
        }
        let children = main.children.take();
        let mut threads = Vec::with_capacity(children.len());
        for Child { messages, thread } in children {
            drop(messages);
            threads.push(thread);
        }
        let mut output = Vec::new();
        for thread in threads {
            match thread.join() {
                Ok(Ok(lines)) => output.extend(lines),
                Ok(Err(message)) => output.push(message),
                Err(_) => output.push("agent panicked".into()),
            }
        }
        output
    }
}

fn worker_run(host: Host, source: &str, cancelled: Arc<AtomicBool>) -> Result<Vec<String>, String> {
    let session = Session::enter(host);
    let budget = session.host.budget.clone();
    let mut engine =
        JsEngine::new(Default::default()).map_err(|error| format!("agent: {error:?}"))?;
    engine.set_can_block(true);
    engine.set_interrupt_handler(Some(Box::new(move || {
        budget.expired() || cancelled.load(Ordering::Relaxed)
    })));
    session
        .install(&mut engine)
        .map_err(|error| format!("agent setup: {error:?}"))?;
    engine
        .eval(
            source,
            EvalOptions {
                filename: "<evalScript>".into(),
                ..Default::default()
            },
        )
        .map_err(|error| format!("agent script: {error:?}"))?;
    let Group::Worker(worker) = &session.host.agents else {
        unreachable!()
    };
    loop {
        engine
            .run_jobs()
            .map_err(|error| format!("agent job: {error:?}"))?;
        let Some((realm, callback)) = worker.callback.take() else {
            break;
        };
        let Ok(message) = worker
            .messages
            .recv_timeout(session.host.budget.remaining())
        else {
            engine.runtime_mut().release_persistent(callback);
            break;
        };
        let _ = message.received.send(());
        let runtime = engine.runtime_mut();
        let result = runtime.with_scope(realm, |ctx| {
            let function = ctx
                .get_persistent(&callback)?
                .expect("rooted agent callback");
            let sab = create_shared_array_buffer_from_arc(ctx, message.backing)?;
            let value = ctx.root(Value::int32(message.value))?;
            let this = ctx.root(Value::UNDEFINED)?;
            let args = ctx.args(&[sab, value])?;
            call(ctx, function, this, args)
        });
        runtime.release_persistent(callback);
        result.map_err(|error| format!("agent callback: {error:?}"))?;
    }
    Ok(session.output())
}

pub(super) fn start<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let host = current();
    let Group::Main(main) = &host.agents else {
        return Err(ctx.throw_type_error("cannot be called inside an agent"));
    };
    let source = argument(ctx, args, 0)?;
    let source = string_arg(ctx, source)?;
    let (messages, receiver) = mpsc::channel();
    let budget = host.budget.clone();
    let capture = host.capture;
    let local = host.local;
    let reports = main.reports.clone();
    let cancelled = main.cancelled.clone();
    let thread = thread::Builder::new()
        .name("test262-agent".into())
        .spawn(move || {
            let worker = Host {
                budget,
                capture,
                local,
                printed: RefCell::default(),
                agents: Group::Worker(Worker {
                    messages: receiver,
                    reports,
                    callback: RefCell::default(),
                }),
            };
            worker_run(worker, &source, cancelled)
        })
        .map_err(|error| ctx.throw_type_error(&format!("could not start agent: {error}")))?;
    main.children.borrow_mut().push(Child { messages, thread });
    Ok(Value::UNDEFINED)
}

pub(super) fn broadcast<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let host = current();
    let Group::Main(main) = &host.agents else {
        return Err(ctx.throw_type_error("cannot be called inside an agent"));
    };
    let buffer = argument(ctx, args, 0)?;
    let Some(buffer) = ctx.get(buffer).as_object() else {
        return Err(ctx.throw_type_error("expected SharedArrayBuffer"));
    };
    let ObjectData::SharedArrayBuffer(data) = &ctx.heap().object(buffer).data else {
        return Err(ctx.throw_type_error("expected SharedArrayBuffer"));
    };
    let backing = data.backing.clone();
    let value = argument(ctx, args, 1)?;
    let value = to_int32(ctx, value)?;
    let channels: Vec<_> = main
        .children
        .borrow()
        .iter()
        .map(|child| child.messages.clone())
        .collect();
    let mut acknowledgements = Vec::with_capacity(channels.len());
    for channel in channels {
        let (received, ack) = mpsc::channel();
        channel
            .send(Broadcast {
                backing: backing.clone(),
                value,
                received,
            })
            .map_err(|_| ctx.throw_type_error("agent left before broadcast"))?;
        acknowledgements.push(ack);
    }
    for ack in acknowledgements {
        match ack.recv_timeout(host.budget.remaining()) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(Abrupt::Interrupted),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ctx.throw_type_error("agent left before receiving broadcast"));
            }
        }
    }
    Ok(Value::UNDEFINED)
}

pub(super) fn receive_broadcast<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let host = current();
    let Group::Worker(worker) = &host.agents else {
        return Err(ctx.throw_type_error("must be called inside an agent"));
    };
    let function = argument(ctx, args, 0)?;
    if !ctx
        .get(function)
        .as_object()
        .is_some_and(|id| is_callable(ctx.heap(), id))
    {
        return Err(ctx.throw_type_error("expecting function"));
    }
    let callback = ctx.persist(function)?;
    let old = worker.callback.replace(Some((ctx.realm_id(), callback)));
    if let Some((_, old)) = old {
        ctx.release_persistent(old);
    }
    Ok(Value::UNDEFINED)
}

pub(super) fn leaving<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    if matches!(current().agents, Group::Main(_)) {
        return Err(ctx.throw_type_error("must be called inside an agent"));
    }
    Ok(Value::UNDEFINED)
}

pub(super) fn get_report<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    _: Args<'s>,
) -> JsResult<Value> {
    let host = current();
    let report = host.agents.reports().receive.lock().unwrap().try_recv();
    let Ok(report) = report else {
        return Ok(Value::NULL);
    };
    let report = ctx.new_utf16(&report)?;
    Ok(ctx.get(report))
}

pub(super) fn report<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    _: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    let host = current();
    let value = argument(ctx, args, 0)?;
    let value = units_arg(ctx, value)?;
    let _ = host.agents.reports().send.send(value);
    Ok(Value::UNDEFINED)
}
