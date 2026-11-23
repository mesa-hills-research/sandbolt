use std::fmt::Write;

use crate::atom::Atom;
use crate::heap::Heap;
use crate::ledger::{AllocError, AllocResult, HeapLedger, TrackedBox, TrackedVec};
use crate::object::{ErrorFrame, ErrorKind, FrameSource, ObjectData, ObjectFlags};
use crate::ops::{call, convert, property};
use crate::root::{ExecCtx, Handle};
use crate::shape::PropFlags;
use crate::string::JsString;
use crate::value::*;
use crate::vm::CallFrame;

use super::utf16_lossy;

const MAX_FRAMES: usize = 64;

#[derive(Clone, Copy)]
struct ParseSite<'a> {
    file: &'a str,
    line: u32,
    column: u32,
}

impl<'s> ExecCtx<'_, 's> {
    pub fn capture_stack(&mut self, skip: usize) -> JsResult<Handle<'s>> {
        let frames = active_frames(self, skip)
            .take(MAX_FRAMES)
            .collect::<Vec<_>>();
        let stack = formatted_stack(self, &frames)?;
        self.root(stack)
    }

    pub fn capture_error_stack(&mut self, error: Handle<'s>) -> JsResult<()> {
        save_error_data(self, self.get(error), 0, None)
    }
}

pub(crate) fn throw_syntax_error(
    ctx: &mut ExecCtx<'_, '_>,
    message: &str,
    file: &str,
    line: u32,
    column: u32,
) -> Abrupt {
    let thrown = ctx.throw_error(ErrorKind::Syntax, message);
    if !ctx.is_oom() {
        let error = ctx.pending_exception().unwrap();
        if let Err(interrupted) =
            save_error_data(ctx, error, 0, Some(ParseSite { file, line, column }))
        {
            return interrupted;
        }
    }
    thrown
}

pub(crate) fn capture_constructor(ctx: &mut ExecCtx<'_, '_>, error: Value) -> JsResult<()> {
    save_error_data(ctx, error, 1, None)
}

pub(crate) fn capture_on_object<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    target: Handle<'s>,
    filter: Value,
) -> JsResult<()> {
    ctx.object_id(target)?;
    let skip = ctx
        .runtime
        .vm
        .frames
        .iter()
        .rev()
        .position(|frame| Value::object(frame.callee) == filter)
        .map_or(1, |index| index + 1);
    if let Some(stack) = build_stack(ctx, ctx.get(target), skip, None, None)? {
        let stack = ctx.root(stack)?;
        crate::builtins::define_data(
            ctx,
            target,
            Atom::get("stack"),
            stack,
            PropFlags::WRITABLE | PropFlags::CONFIGURABLE,
        )?;
    }
    Ok(())
}

fn save_error_data(
    ctx: &mut ExecCtx<'_, '_>,
    error: Value,
    skip: usize,
    parse: Option<ParseSite<'_>>,
) -> JsResult<()> {
    let Some(id) = error.as_object() else {
        return Ok(());
    };
    let object = ctx.heap().object(id);
    if !matches!(&object.data, ObjectData::Error(data) if data.stack == Value::UNDEFINED)
        || has_own_stack(ctx.heap(), id)
    {
        return Ok(());
    }
    if let Some(stack) = build_stack(ctx, error, skip, parse, Some(id))?
        && !has_own_stack(ctx.heap(), id)
    {
        let object = ctx.runtime.heap.objects.get_mut(id).unwrap();
        let ObjectData::Error(data) = &mut object.data else {
            unreachable!()
        };
        data.stack = stack;
    }
    Ok(())
}

fn has_own_stack(heap: &Heap, id: ObjId) -> bool {
    heap.shape(heap.object(id).shape)
        .find(Atom::get("stack"))
        .is_some()
}

fn build_stack(
    ctx: &mut ExecCtx<'_, '_>,
    target: Value,
    skip: usize,
    parse: Option<ParseSite<'_>>,
    save_frames: Option<ObjId>,
) -> JsResult<Option<Value>> {
    if ctx.runtime.vm.stack_capture.is_some() {
        return Ok(None);
    }
    // The callback may throw or collect while the original exception is suspended.
    let pending = ctx.take_exception();
    ctx.runtime.vm.stack_capture = Some((target, pending.map(|(value, _)| value)));
    let thrown_frames = std::mem::take(&mut ctx.runtime.vm.thrown_frames);
    let result = build_stack_inner(ctx, target, skip, parse, save_frames);
    ctx.take_exception();
    if result != Err(Abrupt::Interrupted)
        && let Some((value, oom)) = pending
    {
        ctx.throw_value(value, oom);
    }
    ctx.runtime.vm.stack_capture = None;
    ctx.runtime.vm.thrown_frames = thrown_frames;
    match result {
        Ok(stack) => Ok(Some(stack)),
        Err(Abrupt::Thrown) => Ok(Some(Value::NULL)),
        Err(Abrupt::Interrupted) => Err(Abrupt::Interrupted),
    }
}

fn build_stack_inner(
    ctx: &mut ExecCtx<'_, '_>,
    target: Value,
    skip: usize,
    parse: Option<ParseSite<'_>>,
    save_frames: Option<ObjId>,
) -> JsResult<Value> {
    let value = ctx.realm().error_stack_limit;
    let overflow = target == Value::object(ctx.realm().stack_error.object);
    let limit = if let Some(n) = value.as_number() {
        n
    } else if overflow {
        0.0
    } else {
        match ctx.with_scope(|ctx| {
            let value = ctx.root(value)?;
            convert::to_number(ctx, value)
        }) {
            Ok(n) => n,
            Err(Abrupt::Thrown) => {
                ctx.take_exception();
                0.0
            }
            Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
        }
    } as usize;
    let limit = limit.min(MAX_FRAMES);
    let mut frames = active_frames(ctx, skip)
        .take(limit.saturating_sub(usize::from(parse.is_some())))
        .collect::<Vec<_>>();
    if let Some(id) = save_frames {
        let first = ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let mut saved = TrackedVec::new();
            let parse = parse.filter(|_| limit > 0);
            saved.try_reserve_exact(&heap.ledger, frames.len() + usize::from(parse.is_some()))?;
            let first = if let Some(site) = parse {
                let file = JsString::from_str(&heap.ledger, site.file)?;
                let file = heap.strings.insert(&heap.ledger, file)?;
                let frame = ErrorFrame {
                    source: FrameSource::Parse {
                        file,
                        line: site.line,
                        column: site.column,
                    },
                    function: None,
                    name: None,
                    constructor: false,
                };
                saved.push_reserved(frame);
                Some(frame)
            } else {
                None
            };
            // Only CallSites expose functions. A plain stack should not retain closures.
            for &frame in &frames {
                saved.push_reserved(ErrorFrame {
                    function: None,
                    ..frame
                });
            }
            let ObjectData::Error(data) = &mut heap.objects.get_mut(id).unwrap().data else {
                unreachable!()
            };
            data.frames = saved;
            Ok(first)
        })?;
        if let Some(first) = first {
            frames.insert(0, first);
        }
    }
    let prepare = ctx.realm().error_prepare_stack;
    if !overflow
        && prepare
            .as_object()
            .is_some_and(|id| call::is_callable(ctx.heap(), id))
    {
        return ctx.with_scope(|ctx| {
            let prepare = ctx.root(prepare)?;
            let target = ctx.root(target)?;
            let this = ctx.root(Value::object(ctx.realm().error_ctor))?;
            let sites = ctx.new_array()?;
            let proto = ctx.realm().callsite_proto;
            let array = ctx.get(sites);
            for (i, &frame) in frames.iter().enumerate() {
                ctx.with_scope(|ctx| {
                    let sites = ctx.root(array)?;
                    let site = ctx.make_object(Some(proto), ObjectFlags::EXTENSIBLE, |ledger| {
                        Ok(ObjectData::CallSite(TrackedBox::new(ledger, frame)?))
                    })?;
                    crate::builtins::define_data(
                        ctx,
                        sites,
                        crate::atom::PropKey::index(i as u32).unwrap(),
                        site,
                        PropFlags::DEFAULT_DATA,
                    )
                })?;
            }
            let args = ctx.args(&[target, sites])?;
            call::call(ctx, prepare, this, args)
        });
    }
    formatted_stack(ctx, &frames)
}

fn active_frames<'a>(
    ctx: &'a ExecCtx<'_, '_>,
    skip: usize,
) -> impl Iterator<Item = ErrorFrame> + 'a {
    let heap = ctx.heap();
    let mut barrier = false;
    ctx.runtime
        .vm
        .frames
        .iter()
        .rev()
        .skip(skip)
        .map_while(move |frame| {
            if barrier {
                return None;
            }
            barrier = frame
                .code
                .and_then(|code| heap.bytecode.get(code)?.compiled.as_ref())
                .is_some_and(|f| f.flags.backtrace_barrier);
            Some(active_frame(heap, frame))
        })
}

fn active_frame(heap: &Heap, frame: &CallFrame) -> ErrorFrame {
    let function = heap.object(frame.callee);
    let name = heap
        .shape(function.shape)
        .find(Atom::get("name"))
        .and_then(|(index, _)| match function.props[index].decode() {
            ValueKind::String(id) if heap.string_units(id).next().is_some_and(|u| u != 0) => {
                Some(id)
            }
            _ => None,
        });
    ErrorFrame {
        source: frame
            .code
            .map_or(FrameSource::Native, |code| FrameSource::Bytecode {
                code,
                pc: frame.last_pc,
            }),
        function: Some(frame.callee),
        name,
        constructor: frame.new_target.is_some(),
    }
}

struct StackText<'a> {
    ledger: &'a HeapLedger,
    units: TrackedVec<u16>,
}

impl StackText<'_> {
    fn add(&mut self, units: impl Iterator<Item = u16>) -> AllocResult<()> {
        for unit in units {
            self.units.push(self.ledger, unit)?;
        }
        Ok(())
    }
}

impl Write for StackText<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.add(text.encode_utf16()).map_err(|_| std::fmt::Error)
    }
}

fn formatted_stack(ctx: &mut ExecCtx<'_, '_>, frames: &[ErrorFrame]) -> JsResult<Value> {
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let mut text = StackText {
            ledger: &heap.ledger,
            units: TrackedVec::new(),
        };
        for frame in frames {
            let write = |text: &mut StackText<'_>, s: &str| {
                text.write_str(s).map_err(|_| AllocError::Limit)
            };
            write(&mut text, "    at ")?;
            if !matches!(frame.source, FrameSource::Parse { .. }) {
                if let Some(name) = frame.name {
                    text.add(name_units(heap, name))?;
                } else {
                    write(&mut text, "<anonymous>")?;
                }
                write(&mut text, " (")?;
            }
            if let Some((file, line, column)) = frame_location(heap, frame.source) {
                match file {
                    FrameFile::Utf8(file) => write(&mut text, file)?,
                    FrameFile::Js(file) => text.add(heap.string_units(file))?,
                }
                write!(&mut text, ":{line}:{column}").map_err(|_| AllocError::Limit)?;
            } else {
                write(
                    &mut text,
                    if matches!(frame.source, FrameSource::Native) {
                        "native"
                    } else {
                        "missing"
                    },
                )?;
            }
            write(
                &mut text,
                if matches!(frame.source, FrameSource::Parse { .. }) {
                    "\n"
                } else {
                    ")\n"
                },
            )?;
        }
        let string = JsString::from_utf16(&heap.ledger, &text.units)?;
        Ok(Value::string(heap.strings.insert(&heap.ledger, string)?))
    })
}

fn name_units(heap: &Heap, name: StrId) -> impl Iterator<Item = u16> + '_ {
    heap.string_units(name).take_while(|&unit| unit != 0)
}

pub(crate) fn callsite_name(ctx: &mut ExecCtx<'_, '_>, name: Option<StrId>) -> JsResult<Value> {
    let Some(name) = name else {
        return Ok(Value::NULL);
    };
    let len = name_units(ctx.heap(), name).count();
    if len == ctx.heap().string(name).len() as usize {
        return Ok(Value::string(name));
    }
    ctx.allocate(|runtime| {
        let heap = &mut runtime.heap;
        let string = JsString::from_units(
            &heap.ledger,
            len,
            name_units(heap, name).any(|u| u > 255),
            name_units(heap, name),
        )?;
        Ok(Value::string(heap.strings.insert(&heap.ledger, string)?))
    })
}

enum FrameFile<'a> {
    Utf8(&'a str),
    Js(StrId),
}

fn frame_location(heap: &Heap, source: FrameSource) -> Option<(FrameFile<'_>, u32, u32)> {
    match source {
        FrameSource::Parse { file, line, column } => Some((FrameFile::Js(file), line, column)),
        FrameSource::Native => None,
        FrameSource::Bytecode { code, pc } => {
            let f = heap.bytecode.get(code)?.compiled.as_ref()?;
            let end = f.pc_map.partition_point(|p| p.pc <= pc);
            let byte = f.pc_map[..end]
                .last()
                .map_or(f.source_start, |p| p.byte_offset);
            let line = f
                .line_starts
                .partition_point(|&start| start <= byte)
                .saturating_sub(1);
            let column = byte.saturating_sub(f.line_starts[line]) + 1;
            let file = std::str::from_utf8(&f.filename).expect("UTF-8 filename");
            Some((FrameFile::Utf8(file), line as u32 + 1, column))
        }
    }
}

pub(crate) fn callsite_position(heap: &Heap, source: FrameSource) -> Option<(u32, u32)> {
    frame_location(heap, source).map(|(_, line, column)| (line, column))
}

pub(crate) fn callsite_file(ctx: &mut ExecCtx<'_, '_>, source: FrameSource) -> JsResult<Value> {
    match source {
        FrameSource::Native => Ok(Value::NULL),
        FrameSource::Parse { file, .. } => Ok(Value::string(file)),
        FrameSource::Bytecode { .. } => ctx.allocate(|runtime| {
            let heap = &mut runtime.heap;
            let Some((FrameFile::Utf8(file), _, _)) = frame_location(heap, source) else {
                return Ok(Value::NULL);
            };
            let string = JsString::from_str(&heap.ledger, file)?;
            Ok(Value::string(heap.strings.insert(&heap.ledger, string)?))
        }),
    }
}

fn host_frame(heap: &Heap, frame: &ErrorFrame) -> Option<crate::Frame> {
    let (file, line, column) = frame_location(heap, frame.source)?;
    let file = match file {
        FrameFile::Utf8(file) => file.to_owned(),
        FrameFile::Js(file) => utf16_lossy(heap, file),
    };
    let function = frame.name.map_or_else(String::new, |id| {
        String::from_utf16_lossy(&name_units(heap, id).collect::<Vec<_>>())
    });
    Some(crate::Frame {
        file,
        function,
        line,
        column,
    })
}

pub(crate) fn saved_frames(ctx: &ExecCtx<'_, '_>, error: Value) -> Option<Vec<crate::Frame>> {
    let id = error.as_object()?;
    let ObjectData::Error(data) = &ctx.heap().object(id).data else {
        return None;
    };
    Some(
        data.frames
            .iter()
            .filter_map(|frame| host_frame(ctx.heap(), frame))
            .collect(),
    )
}

pub(crate) fn native_stack(ctx: &mut ExecCtx<'_, '_>, value: Value) -> JsResult<Value> {
    if let Some(id) = value.as_object()
        && matches!(&ctx.heap().object(id).data, ObjectData::DomException(_))
    {
        ctx.with_scope(|ctx| {
            let error = ctx.root(value)?;
            save_dom_stack(ctx, error, 1)
        })?;
    }
    Ok(value)
}

fn save_dom_stack<'s>(ctx: &mut ExecCtx<'_, 's>, error: Handle<'s>, skip: usize) -> JsResult<()> {
    let current = property::get_own_property_descriptor(ctx, error, Atom::get("stack"))?;
    if current.is_some_and(|d| matches!(d.value.map(Value::decode), Some(ValueKind::String(s)) if ctx.heap().string(s).is_empty())) {
        let stack = ctx.capture_stack(skip)?;
        property::define_own_property(ctx, error, Atom::get("stack"), property::PropertyDescriptor { value: Some(stack), ..Default::default() })?;
    }
    Ok(())
}

pub(crate) fn capture_frames(ctx: &ExecCtx<'_, '_>) -> Vec<crate::Frame> {
    active_frames(ctx, 0)
        .filter_map(|frame| host_frame(ctx.heap(), &frame))
        .collect()
}

pub(crate) fn capture_throw(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    if ctx.runtime.vm.stack_capture.is_some() {
        return Ok(());
    }
    if ctx.runtime.vm.thrown_frames.is_empty() {
        ctx.runtime.vm.thrown_frames = capture_frames(ctx);
    }
    let Some(value) = ctx.pending_exception().filter(|_| !ctx.is_oom()) else {
        return Ok(());
    };
    let Some(id) = value.as_object() else {
        return Ok(());
    };
    match &ctx.heap().object(id).data {
        ObjectData::Error(_) => save_error_data(ctx, value, 0, None)?,
        ObjectData::DomException(_) => {
            let _ = ctx.with_scope(|ctx| {
                let root = ctx.root(value)?;
                save_dom_stack(ctx, root, 0)
            });
            ctx.throw_value(value, false);
        }
        _ => {}
    }
    Ok(())
}
