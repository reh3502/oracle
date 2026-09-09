//! Linux-only, trusted-process experiment for Oracle's Stage 0 P1 gate.
//! Not the production module SDK or an untrusted-code sandbox.
#![forbid(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("P1 process cleanup is qualified only on Linux");

pub mod harness;
pub mod rpc;
pub mod runtime;
