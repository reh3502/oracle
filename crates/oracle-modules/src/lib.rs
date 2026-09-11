//! Trusted native module installation and runtime lifecycle management.
#![forbid(unsafe_code)]
mod admission;
pub mod dependencies;
mod generation;
mod manager;
pub mod package;
pub use manager::{
    ConfigurationPlan, ConfigurationPolicy, ConfigurationReceipt, ConfigurationStatus,
    ModuleManager,
};
mod effects;
pub use effects::{DispatchPermit, EchoTransport, Observation, SendTransport};

pub use manager::{
    EventDispatch, EventHealth, NotificationCheck, NotificationRequest, NotificationTransport,
};

pub use manager::ModuleCatalogEntry;

mod registry;
pub use registry::RegistryDispatchPermit;

pub use manager::ModuleCatalogSnapshot;

pub use manager::{ModuleAiCatalogEntry, ModuleAiCatalogSnapshot};
