macro_rules! payloads {
    ($( $name:ident { $( $field:ident : $ty:ty ),* } ),+ $(,)?) => {$(
        #[derive(Debug, Default)]
        pub struct $name { $(pub $field: $ty,)* }
        impl $crate::gc::Trace for $name {
            fn trace(&self, _tracer: &$crate::gc::Tracer<'_>) {
                $($crate::gc::Trace::trace(&self.$field, _tracer);)*
            }
        }
    )+};
}

mod array;
mod buffers;
mod collections;
mod errors;
mod functions;
mod host;
mod iterators;
mod numbers;
mod promise;
mod proxy;
mod text;
mod weak;

pub use array::*;
pub use buffers::*;
pub use collections::*;
pub use errors::*;
pub use functions::*;
pub use host::*;
pub use iterators::*;
pub use numbers::*;
pub use promise::*;
pub use proxy::*;
pub use text::*;
pub use weak::*;
