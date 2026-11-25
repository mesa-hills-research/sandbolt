//! Bytecode and compiled images for the QuickJS-ng v0.17.0 instruction set.

#![deny(unsafe_code)]

pub mod atoms;
pub mod codec;
pub mod disasm;
pub mod image;
pub mod ir;
pub mod opcode;
pub mod verify;

pub use atoms::*;
pub use codec::{CodecError, CodecErrorKind, decode_image, encode_image};
pub use disasm::{disassemble_function, disassemble_image};
pub use image::*;
pub use ir::*;
pub use opcode::*;
pub use verify::{ImageVerifyError, VerifyError, compute_stack_size, verify, verify_image};
