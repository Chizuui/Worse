//! Harness git service: a read-only port of `ZCode`'s `GitCliRepo` and
//! `GitService` (status, numstat, diff, log graph, branches, identity,
//! ignored paths).
//!
//! # gix vs git CLI
//!
//! Every operation shells out to the `git` CLI with an argv vector (never a
//! shell string), using the exact arguments and environment of the TS code
//! (`gitCliRepo.ts`, `providers/gitEnvironmentProvider.ts`). The results must
//! be drop-in identical to TS, including porcelain-v2 status semantics,
//! rename detection, `.gitattributes`/filters, `--show-origin` config output
//! and git's own error messages that the TS code classifies; reproducing
//! those through `gix` would be a re-implementation with parity risk and no
//! measurable win for these read paths (each is one or a few git processes).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod diff;
mod error;
pub mod parse;
mod repo;
pub mod runner;
mod service;
pub mod types;
pub mod util;

#[cfg(test)]
mod tests;

pub use diff::DiffQuery;
pub use error::GitError;
pub use repo::{GitRepo, Resolution, StatusSnapshot};
pub use runner::{never_cancelled, CancelFn, ExecResult, GitRunner};
pub use service::{changes_for_source, ChangeSource, GitService};
