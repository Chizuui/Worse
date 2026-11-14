//! Harness Rust Host: an NDJSON JSON-RPC server over stdio that serves the
//! Phase 3a/3b services (`fs`, `search`, `git`, `watch`, `pty`) behind the TS Host's RPC
//! seam. The wire contract is `Worse/.work/phase3/contract.md`; results are the
//! exact TS service return values so the bridge can splice them into RPC
//! frames.
//!
//! Desktop-only: mobile surfaces never run a local Host.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod methods;
pub mod rpc;
pub mod server;

pub use server::{serve, ExitReason, MAX_LINE_BYTES};
