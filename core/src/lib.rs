//! The arithmetic of the venue, with no I/O.
//!
//! This crate is compiled twice: to `wasm32` inside the NEAR contract, and
//! natively inside the engine that quotes and takes trades. One implementation
//! of the money means the off-chain simulation cannot drift from what the chain
//! will actually do — which is the failure mode the previous JavaScript mirror
//! needed a parity test suite to hold shut.
//!
//! Rules for anything that lands here:
//! - integers only, overflow rejected rather than wrapped;
//! - no `near_sdk`, no networking, no filesystem, no clock;
//! - every function total on its inputs, so a bad argument returns `None`
//!   instead of a number the contract then has to second-guess.

pub mod accounting;
pub mod consolation;
pub mod fees;
pub mod oracle;
pub mod pool;
