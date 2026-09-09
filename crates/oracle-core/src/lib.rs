//! Framework policy and orchestration; concrete database/Discord adapters live elsewhere.
#![forbid(unsafe_code)]
pub use oracle_contracts::*;
pub mod tasks;

mod module_repository;
mod repository;
mod service;
mod workflows;

pub use module_repository::ModuleRepository;
pub use repository::Repository;
pub use service::*;
pub use workflows::*;
