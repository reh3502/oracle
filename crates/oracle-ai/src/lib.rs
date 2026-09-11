//! Host-owned agent coordination and provider contracts. Models propose; host ports execute.
#![forbid(unsafe_code)]
pub mod budget;
pub mod catalog;
pub mod gemini;
pub mod provider;
pub mod spend;
pub mod state;
