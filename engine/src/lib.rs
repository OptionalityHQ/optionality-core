//! The engine's decision logic, away from the network.
//!
//! `ladder` decides what market exists; `quote` decides what it costs. Both are
//! pure, because every one of their outputs ends up inside a transaction the
//! chain will check: a series the venue lists, a premium a trader is charged, a
//! settlement print the oracle is allowed to accept.
//!
//! The binary in `main.rs` is the only thing that owns the process: it starts
//! [`feed`], which holds the three venue sockets, and answers HTTP. Prices are
//! decided in [`serve`] and [`quote`], never in the transport.

pub mod feed;
pub mod ladder;
pub mod quote;
pub mod serve;
