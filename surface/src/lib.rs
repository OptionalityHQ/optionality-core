//! The pricing model the venue quotes from.
//!
//! Kept out of `core` on purpose: this is f64 and the contract must never
//! evaluate a float, so the chain sees only the integer premium the engine
//! decided to ask for and checks it with integer bounds.

pub mod index;
pub mod pricing;
