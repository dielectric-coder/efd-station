pub mod discover;
pub mod error;
pub mod parse;
pub mod poll;
pub mod responder;
pub mod serial;

pub use discover::discover_serial_device;
pub use error::CatError;
pub use poll::{spawn_cat_tasks, CatConfig};
pub use responder::{spawn_responder, Backend, ResponderConfig};
pub use serial::SerialPort;

// Re-export AgcMode so internal parsers can reference it via crate path
// without taking a hard dep on efd_proto from each module.
pub(crate) use efd_proto::AgcMode;
