//! Contains [`Buffer`], an immutable container for all Arrow physical types (e.g. i32, f64).

mod immutable;

pub(crate) mod bytes;

pub use immutable::Buffer;
