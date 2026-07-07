//! lit-bridge-rs library surface — exposes the parser and session modules so
//! the binary, examples (parity harness), and tests can share them.

pub mod diag;
pub mod jsonl;
pub mod parser;
pub mod reflow;
pub mod session;
