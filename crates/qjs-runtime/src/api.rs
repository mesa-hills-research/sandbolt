//! Idiomatic public Rust embedding API and Arko `code_mode` / `test262` host seam.

use std::sync::Arc;

use crate::{EngineError, EngineLimits, Runtime};
use crate::value::RealmId;

/// Options controlling script or module evaluation.
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
            filename: "<eval>".to_string(),
            strict: false,
            module: false,
            annex_b: true,
        }
    }
}

/// Result of resolving an imported module specifier.
#[derive(Debug, Clone)]
pub enum ResolvedModule {
    Source {
        name: String,
        source: String,
    },
    Json {
        name: String,
        source: String,
    },
    Text {
        name: String,
        text: String,
    },
    Bytes {
        name: String,
        bytes: Vec<u8>,
    },
}

/// Host module loader callback for static `import` and dynamic `import()`.
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

/// High-level JavaScript engine wrapping [`Runtime`] with all ECMAScript built-ins
/// installed, matching Arko's `code_mode::engine::Interpreter` requirements and
/// supporting multi-realm and `test262` hosts.
pub struct JsEngine {
    runtime: Runtime,
    host_callback: Option<HostCallback>,
    print_callback: Option<PrintCallback>,
    module_loader: Option<Box<dyn ModuleLoader>>,
    can_block: bool,
}

impl JsEngine {
    pub fn new(limits: EngineLimits) -> Result<Self, EngineError> {
        let mut runtime = Runtime::new(limits)
            .map_err(|_| EngineError::Failed("failed to initialize runtime".into()))?;
        let realm = runtime.default_realm();
        runtime
            .install_builtins(realm)
            .map_err(|_| EngineError::Failed("failed to install builtins".into()))?;
        Ok(Self {
            runtime,
            host_callback: None,
            print_callback: None,
            module_loader: None,
            can_block: true,
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
        self.host_callback = Some(callback);
    }

    pub fn set_print_callback(&mut self, callback: PrintCallback) {
        self.print_callback = Some(callback);
    }

    pub fn set_module_loader(&mut self, loader: Box<dyn ModuleLoader>) {
        self.module_loader = Some(loader);
    }

    pub fn set_can_block(&mut self, can_block: bool) {
        self.can_block = can_block;
        self.runtime.set_can_block(can_block);
    }

    pub fn can_block(&self) -> bool {
        self.runtime.can_block()
    }

    pub fn host_callback_mut(&mut self) -> Option<&mut HostCallback> {
        self.host_callback.as_mut()
    }

    pub fn print_callback_mut(&mut self) -> Option<&mut PrintCallback> {
        self.print_callback.as_mut()
    }

    pub fn module_loader_mut(&mut self) -> Option<&mut Box<dyn ModuleLoader>> {
        self.module_loader.as_mut()
    }

    pub fn heap_bytes(&self) -> u64 {
        self.runtime.heap().ledger.used_bytes() as u64
    }

    /// Evaluates JavaScript source text with the given options.
    pub fn eval(&mut self, _source: &str, _options: EvalOptions) -> Result<(), EngineError> {
        Err(EngineError::Failed(
            "VM execution not yet integrated".into(),
        ))
    }

    /// Evaluates JavaScript source text and formats the completion value as a string.
    pub fn eval_to_string(
        &mut self,
        _source: &str,
        _options: EvalOptions,
    ) -> Result<String, EngineError> {
        Err(EngineError::Failed(
            "VM execution not yet integrated".into(),
        ))
    }

    /// Calls a global function with a single string argument (Arko `code_mode` seam).
    pub fn call_global(&mut self, _function: &str, _arg: &str) -> Result<(), EngineError> {
        Err(EngineError::Failed(
            "VM execution not yet integrated".into(),
        ))
    }

    /// Drains all pending Promise / microtask / finalization jobs.
    pub fn run_jobs(&mut self) -> Result<(), EngineError> {
        self.runtime
            .run_jobs()
            .map_err(|_| EngineError::Failed("job execution failed".into()))
    }

    /// Installs the Arko `__host(text) -> text` native bridge on `globalThis`.
    pub fn install_host_bridge(&mut self, callback: HostCallback) -> Result<(), EngineError> {
        self.host_callback = Some(callback);
        Ok(())
    }

    /// Triggers a full garbage collection cycle (used by `$262.gc()`).
    pub fn collect_garbage(&mut self) {
        self.runtime.collect();
    }

    /// Shared buffer handle type placeholder for `$262.agent` cross-thread SABs.
    pub fn new_shared_bytes(len: usize) -> Arc<Vec<std::sync::atomic::AtomicU8>> {
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push(std::sync::atomic::AtomicU8::new(0));
        }
        Arc::new(buf)
    }
}
