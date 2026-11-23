//! JavaScript embedding and host callbacks.

use std::sync::Arc;

use qjs_bytecode::ProgramImage;
use qjs_compiler::CompileError;
use qjs_syntax::{ParseGoal, ParseOptions, SourceText};

use crate::object::{ErrorKind, ObjectData, ObjectFlags};
use crate::ops::{call, convert, property};
use crate::root::{ExecCtx, Handle, Persistent};
use crate::value::*;
use crate::vm::exec;
use crate::{EngineError, EngineLimits, Frame, Runtime};

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub filename: String,
    pub strict: bool,
    pub module: bool,
    pub annex_b: bool,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self {
            filename: "<eval>".into(),
            strict: false,
            module: false,
            annex_b: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ResolvedModule {
    Source { name: String, source: String },
    Json { name: String, source: String },
    Text { name: String, text: String },
    Bytes { name: String, bytes: Vec<u8> },
}

pub trait ModuleLoader {
    fn resolve(
        &mut self,
        referrer: &str,
        specifier: &str,
        module_type: Option<&str>,
    ) -> Result<ResolvedModule, String>;
}

pub type HostCallback = Box<dyn FnMut(&str) -> String>;
pub type PrintCallback = Box<dyn FnMut(&str)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalPhase {
    Parse,
    Resolution,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalFailure {
    pub error: EngineError,
    pub phase: EvalPhase,
    pub error_kind: Option<ErrorKind>,
    pub error_name: Option<String>,
}

pub struct JsEngine {
    runtime: Runtime,
    print_installed: bool,
}

impl JsEngine {
    pub fn new(limits: EngineLimits) -> Result<Self, EngineError> {
        let mut runtime = Runtime::new(limits).map_err(|_| EngineError::OutOfMemory)?;
        let realm = runtime.default_realm();
        runtime
            .install_builtins(realm)
            .map_err(|error| failure(&mut runtime, error, EvalPhase::Runtime).error)?;
        Ok(Self {
            runtime,
            print_installed: false,
        })
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }
    pub fn runtime_mut(&mut self) -> &mut Runtime {
        &mut self.runtime
    }
    pub fn default_realm(&self) -> RealmId {
        self.runtime.default_realm()
    }

    pub fn set_interrupt_handler(&mut self, handler: Option<Box<dyn FnMut() -> bool>>) {
        self.runtime.set_interrupt_handler(handler);
    }

    pub fn set_host_callback(&mut self, callback: HostCallback) {
        self.runtime.vm.host_callback = Some(callback);
    }

    pub fn set_print_callback(&mut self, callback: PrintCallback) {
        self.runtime.vm.print_callback = Some(callback);
    }

    pub fn set_module_loader(&mut self, loader: Box<dyn ModuleLoader>) {
        self.runtime.vm.module_loader = Some(loader);
    }

    pub fn set_can_block(&mut self, can_block: bool) {
        self.runtime.set_can_block(can_block);
    }
    pub fn can_block(&self) -> bool {
        self.runtime.can_block()
    }
    pub fn host_callback_mut(&mut self) -> Option<&mut HostCallback> {
        self.runtime.vm.host_callback.as_mut()
    }
    pub fn print_callback_mut(&mut self) -> Option<&mut PrintCallback> {
        self.runtime.vm.print_callback.as_mut()
    }
    pub fn module_loader_mut(&mut self) -> Option<&mut Box<dyn ModuleLoader>> {
        self.runtime.vm.module_loader.as_mut()
    }
    pub fn heap_bytes(&self) -> u64 {
        self.runtime.heap().ledger.used_bytes() as u64
    }

    pub fn eval(&mut self, source: &str, options: EvalOptions) -> Result<(), EngineError> {
        self.eval_detailed(source, options).map_err(|e| e.error)
    }

    pub fn eval_detailed(&mut self, source: &str, options: EvalOptions) -> Result<(), EvalFailure> {
        self.eval_source(source, options, |_, _| Ok(()))
    }

    pub fn eval_to_string(
        &mut self,
        source: &str,
        options: EvalOptions,
    ) -> Result<String, EngineError> {
        self.eval_source(source, options, |ctx, value| {
            let value = ctx.root(value)?;
            display_value(ctx, value)
        })
        .map_err(|e| e.error)
    }

    fn eval_source<T>(
        &mut self,
        source: &str,
        options: EvalOptions,
        finish: impl FnOnce(&mut ExecCtx<'_, '_>, Value) -> JsResult<T>,
    ) -> Result<T, EvalFailure> {
        self.begin_eval()?;
        let parse = parse_options(&self.runtime, &options);
        if source.len() > parse.max_bytes {
            return Err(eval_failure(
                EngineError::OutOfMemory,
                EvalPhase::Parse,
                None,
                None,
            ));
        }
        let source = SourceText::from_utf8(options.filename, source);
        let image = qjs_compiler::compile_source(&source, parse)
            .map_err(|error| compile_failure(&source, error))?;
        let realm = self.default_realm();
        let result = self.runtime.with_scope(realm, |ctx| {
            let value = exec::execute_image(ctx, &image, Some(&source))?;
            finish(ctx, value)
        });
        self.runtime.clear_kept_objects();
        let phase = if self.runtime.vm.resolution_failed {
            EvalPhase::Resolution
        } else {
            EvalPhase::Runtime
        };
        result.map_err(|e| failure(&mut self.runtime, e, phase))
    }

    pub fn execute_image(
        &mut self,
        image: &ProgramImage,
        source: Option<&SourceText>,
    ) -> Result<(), EngineError> {
        self.begin_eval().map_err(|e| e.error)?;
        let realm = self.default_realm();
        let result = self
            .runtime
            .with_scope(realm, |ctx| exec::execute_image(ctx, image, source));
        self.runtime.clear_kept_objects();
        result
            .map(|_| ())
            .map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime).error)
    }

    fn begin_eval(&mut self) -> Result<(), EvalFailure> {
        self.runtime.vm.exception = None;
        self.runtime.vm.oom_thrown = false;
        self.runtime.vm.thrown_frames.clear();
        self.runtime.vm.resolution_failed = false;
        if self.runtime.vm.print_callback.is_some() && !self.print_installed {
            let realm = self.default_realm();
            self.runtime
                .with_scope(realm, crate::builtins::vm_internal::install_print)
                .map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime))?;
            self.print_installed = true;
        }
        let realm = self.default_realm();
        self.runtime
            .with_scope(realm, |ctx| ctx.check_interrupt())
            .map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime))
    }

    pub fn call_global(&mut self, function: &str, arg: &str) -> Result<(), EngineError> {
        self.begin_eval().map_err(|e| e.error)?;
        let realm = self.default_realm();
        let result = self.runtime.with_scope(realm, |ctx| {
            let global = ctx.root(Value::object(ctx.realm().global_object))?;
            let name = ctx.intern_str(function)?;
            let callee = property::get_property(ctx, global, name)?;
            let callee = ctx.root(callee)?;
            let this = ctx.root(Value::UNDEFINED)?;
            let arg = ctx.new_string(arg)?;
            let args = ctx.args(&[arg])?;
            call::call(ctx, callee, this, args)?;
            Ok(())
        });
        self.runtime.clear_kept_objects();
        result.map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime).error)
    }

    pub fn run_jobs(&mut self) -> Result<(), EngineError> {
        self.run_jobs_detailed().map_err(|e| e.error)
    }

    pub fn run_jobs_detailed(&mut self) -> Result<(), EvalFailure> {
        self.begin_eval()?;
        self.runtime
            .run_jobs()
            .map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime))?;
        let realm = self.default_realm();
        self.runtime
            .with_scope(realm, exec::modules::check_host_modules)
            .map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime))
    }

    pub fn install_host_bridge(&mut self, callback: HostCallback) -> Result<(), EngineError> {
        let realm = self.default_realm();
        self.runtime
            .with_scope(realm, crate::builtins::vm_internal::install_host)
            .map_err(|e| failure(&mut self.runtime, e, EvalPhase::Runtime).error)?;
        self.set_host_callback(callback);
        Ok(())
    }

    pub fn collect_garbage(&mut self) {
        self.runtime.collect();
    }

    pub fn new_shared_bytes(len: usize) -> Arc<Vec<std::sync::atomic::AtomicU8>> {
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push(std::sync::atomic::AtomicU8::new(0));
        }
        Arc::new(buf)
    }
}

impl<'s> ExecCtx<'_, 's> {
    pub fn create_realm_with_builtins(&mut self) -> JsResult<RealmId> {
        let realm = self.runtime.create_realm().map_err(|_| self.throw_oom())?;
        self.runtime.install_builtins(realm)?;
        if self.runtime.vm.print_callback.is_some() {
            self.runtime
                .with_scope(realm, crate::builtins::vm_internal::install_print)?;
        }
        Ok(realm)
    }

    pub fn with_realm<R>(
        &mut self,
        realm: RealmId,
        run: impl for<'a> FnOnce(&mut ExecCtx<'_, 'a>) -> R,
    ) -> R {
        self.runtime.with_scope(realm, run)
    }

    pub fn set_is_html_dda(&mut self, object: Handle<'s>) -> JsResult<()> {
        let id = self.object_id(object)?;
        self.runtime.heap.objects.get_mut(id).unwrap().flags |= ObjectFlags::IS_HTML_DDA;
        Ok(())
    }

    pub fn release_persistent(&mut self, root: Persistent) -> Option<Value> {
        self.runtime.release_persistent(root)
    }
}

pub fn eval_script(
    ctx: &mut ExecCtx<'_, '_>,
    source: &str,
    options: EvalOptions,
) -> JsResult<Value> {
    let parse = parse_options(ctx.runtime, &options);
    if source.len() > parse.max_bytes {
        return Err(ctx.throw_oom());
    }
    let source = SourceText::from_utf8(options.filename, source);
    eval_text(ctx, &source, parse)
}

pub(crate) fn eval_text(
    ctx: &mut ExecCtx<'_, '_>,
    source: &SourceText,
    parse: ParseOptions,
) -> JsResult<Value> {
    ctx.check_interrupt()?;
    let image = compile_text(ctx, source, parse)?;
    exec::execute_image(ctx, &image, Some(source))
}

pub(crate) fn compile_text(
    ctx: &mut ExecCtx<'_, '_>,
    source: &SourceText,
    parse: ParseOptions,
) -> JsResult<ProgramImage> {
    qjs_compiler::compile_source(source, parse).map_err(|error| match error {
        CompileError::Syntax { message, .. } => ctx.throw_error(ErrorKind::Syntax, &message),
        CompileError::OutOfMemory => ctx.throw_oom(),
        CompileError::StackOverflow => ctx.stack_overflow(),
        #[allow(unreachable_patterns)]
        other => ctx.throw_error(ErrorKind::Internal, &format!("{other:?}")),
    })
}

pub(crate) fn parse_options(runtime: &Runtime, options: &EvalOptions) -> ParseOptions<'static> {
    ParseOptions {
        goal: if options.module {
            ParseGoal::Module
        } else {
            ParseGoal::Script
        },
        strict: options.strict,
        annex_b: options.annex_b,
        enclosing_private_names: &[],
        max_depth: (runtime.vm.limits.stack_bytes / 1024).clamp(1, 512),
        max_bytes: runtime
            .heap()
            .ledger
            .limit_bytes()
            .saturating_sub(runtime.heap().ledger.used_bytes()),
    }
}

fn compile_failure(source: &SourceText, error: CompileError) -> EvalFailure {
    let (error, kind, name) = match error {
        CompileError::Syntax { message, line, col } => (
            EngineError::Thrown {
                message: format!("SyntaxError: {message}"),
                frames: vec![Frame {
                    file: source.filename.clone(),
                    function: String::new(),
                    line,
                    column: col,
                }],
            },
            Some(ErrorKind::Syntax),
            Some("SyntaxError".into()),
        ),
        CompileError::OutOfMemory => (EngineError::OutOfMemory, None, None),
        CompileError::StackOverflow => (
            EngineError::Thrown {
                message: "RangeError: Maximum call stack size exceeded".into(),
                frames: Vec::new(),
            },
            Some(ErrorKind::Range),
            Some("RangeError".into()),
        ),
        #[allow(unreachable_patterns)]
        other => (EngineError::Failed(format!("{other:?}")), None, None),
    };
    eval_failure(error, EvalPhase::Parse, kind, name)
}

fn eval_failure(
    error: EngineError,
    phase: EvalPhase,
    error_kind: Option<ErrorKind>,
    error_name: Option<String>,
) -> EvalFailure {
    EvalFailure {
        error,
        phase,
        error_kind,
        error_name,
    }
}

fn failure(runtime: &mut Runtime, error: Abrupt, phase: EvalPhase) -> EvalFailure {
    if error == Abrupt::Interrupted {
        return eval_failure(EngineError::Interrupted, phase, None, None);
    }
    if runtime.is_oom() {
        return eval_failure(
            EngineError::OutOfMemory,
            phase,
            Some(ErrorKind::Internal),
            Some("InternalError".into()),
        );
    }
    let value = runtime.pending_exception().unwrap_or(Value::UNDEFINED);
    let mut frames = std::mem::take(&mut runtime.vm.thrown_frames);
    let result = runtime.with_scope(runtime.default_realm(), |ctx| {
        let handle = ctx.root(value)?;
        if let Some(origin) = exec::saved_frames(ctx, value)
            && !origin.is_empty()
        {
            frames = origin;
        }
        let kind = value
            .as_object()
            .and_then(|id| match &ctx.heap().object(id).data {
                ObjectData::Error(error) => Some(error.kind),
                _ => None,
            });
        let name = if value.as_object().is_some() {
            let key = crate::atom::PropKey::atom(crate::atom::Atom::NAME);
            match property::get_property(ctx, handle, key) {
                Ok(value) => match value.decode() {
                    ValueKind::String(id) => Some(exec::utf16_lossy(ctx.heap(), id)),
                    _ => None,
                },
                Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
                Err(Abrupt::Thrown) => None,
            }
        } else {
            None
        };
        let message = if let Some(kind) = kind {
            let name = name.as_deref().unwrap_or(kind.name());
            let value = property::get_property(
                ctx,
                handle,
                crate::atom::PropKey::atom(crate::atom::Atom::MESSAGE),
            )?;
            let text = if value == Value::UNDEFINED {
                String::new()
            } else {
                let value = ctx.root(value)?;
                display_value(ctx, value)?
            };
            if name.is_empty() {
                text
            } else if text.is_empty() {
                name.into()
            } else {
                format!("{name}: {text}")
            }
        } else {
            display_exception(ctx, handle)?
        };
        let class = if let Some(kind) = kind {
            Some(kind.name().into())
        } else if value.as_object().is_some() {
            let key = crate::atom::Atom::get("constructor");
            match property::get_property(ctx, handle, key) {
                Ok(constructor) if constructor.as_object().is_some() => {
                    let constructor = ctx.root(constructor)?;
                    match property::get_property(ctx, constructor, crate::atom::Atom::get("name")) {
                        Ok(value) => match value.decode() {
                            ValueKind::String(id) => Some(exec::utf16_lossy(ctx.heap(), id)),
                            _ => name,
                        },
                        Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
                        Err(Abrupt::Thrown) => name,
                    }
                }
                Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
                _ => name,
            }
        } else {
            None
        };
        Ok((message, kind, class))
    });
    runtime.vm.exception = None;
    runtime.vm.oom_thrown = false;
    match result {
        Ok((message, kind, name)) => {
            eval_failure(EngineError::Thrown { message, frames }, phase, kind, name)
        }
        Err(Abrupt::Interrupted) => eval_failure(EngineError::Interrupted, phase, None, None),
        Err(Abrupt::Thrown) => eval_failure(
            EngineError::Thrown {
                message: "JavaScript exception".into(),
                frames,
            },
            phase,
            None,
            None,
        ),
    }
}

fn display_exception<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<String> {
    if ctx.get(value).as_object().is_none() {
        return display_value(ctx, value);
    }
    let fallback = match display_value(ctx, value) {
        Ok(text) if !text.starts_with("[object ") => return Ok(text),
        Ok(text) => Some(text),
        Err(Abrupt::Interrupted) => return Err(Abrupt::Interrupted),
        Err(Abrupt::Thrown) => {
            ctx.take_exception();
            None
        }
    };
    let this = ctx.root(Value::UNDEFINED)?;
    let args = ctx.args(&[value])?;
    let id = crate::NativeFnId(crate::builtins::PROMISE_JSON_EXTRAS_BASE + 0x81);
    match crate::builtins::promise_json_extras::dispatch(id, ctx, this, args) {
        Ok(value) => Ok(match value.decode() {
            ValueKind::String(id) => exec::utf16_lossy(ctx.heap(), id),
            _ => fallback.unwrap_or_else(|| "JavaScript exception".into()),
        }),
        Err(Abrupt::Interrupted) => Err(Abrupt::Interrupted),
        Err(Abrupt::Thrown) => {
            ctx.take_exception();
            Ok(fallback.unwrap_or_else(|| "JavaScript exception".into()))
        }
    }
}

pub(crate) fn display_value<'s>(ctx: &mut ExecCtx<'_, 's>, value: Handle<'s>) -> JsResult<String> {
    let value = convert::to_string(ctx, value)?;
    let ValueKind::String(id) = ctx.get(value).decode() else {
        unreachable!()
    };
    Ok(exec::utf16_lossy(ctx.heap(), id))
}
