pub mod discover;
pub mod error;
pub mod parse;
pub mod poll;
pub mod serial;

pub use discover::discover_serial_device;
pub use error::CatError;
pub use poll::{spawn_cat_tasks, CatConfig};
pub use serial::SerialPort;
