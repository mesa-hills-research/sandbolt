//! Sandbolt is a pure-Rust port of QuickJS-ng v0.17.0, a small embeddable
//! JavaScript engine.
//!
//! Every crate builds under `#![deny(unsafe_code)]` with no external
//! dependencies. Use [`api::JsEngine`] with [`EngineLimits`] to cap heap and
//! stack usage, [`api::JsEngine::set_interrupt_handler`] for an uncatchable
//! stop, and [`api::JsEngine::install_host_bridge`] or
//! [`api::JsEngine::set_print_callback`] to expose host functions.
//!
//! ```
//! use sandbolt::api::{EvalOptions, JsEngine};
//! use sandbolt::{EngineError, EngineLimits};
//!
//! fn main() -> Result<(), EngineError> {
//!     let mut engine = JsEngine::new(EngineLimits {
//!         heap_bytes: 4 * 1024 * 1024,
//!         stack_bytes: 256 * 1024,
//!     })?;
//!
//!     let sum = engine.eval_to_string(
//!         "[1, 2, 3].reduce((a, b) => a + b, 0)",
//!         EvalOptions::default(),
//!     )?;
//!     assert_eq!(sum, "6");
//!
//!     let err = engine.eval_to_string(
//!         "throw new TypeError('bad input')",
//!         EvalOptions::default(),
//!     );
//!     assert!(matches!(
//!         err,
//!         Err(EngineError::Thrown { ref message, .. }) if message == "TypeError: bad input"
//!     ));
//!
//!     Ok(())
//! }
//! ```

pub use sandbolt_runtime::*;
