//! Trusted native module installation and runtime lifecycle management.
#![forbid(unsafe_code)]
mod admission;
pub mod dependencies;
mod generation;
mod manager;
pub mod package;
pub use manager::ModuleManager;
mod effects;
pub use effects::{DispatchPermit, EchoTransport, Observation, SendTransport};
