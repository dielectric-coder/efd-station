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
