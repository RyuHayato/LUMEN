//! Windows-specific audio output. Only compiled on Windows.

pub mod device_notifications;
pub mod wasapi;

pub use device_notifications::{DeviceEvent, Notifications};
pub use wasapi::WasapiBackend;
