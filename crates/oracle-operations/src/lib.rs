//! Typed Discord operations shared by human commands and future agent tools.
#![forbid(unsafe_code)]
pub mod commands;
pub mod executor;
pub mod permissions;
pub mod published;
mod published_cards;
pub mod structure;

pub mod ingress;

pub mod shared_cards;
