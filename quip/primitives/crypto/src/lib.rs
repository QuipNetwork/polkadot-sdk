//! Substrate-facing hybrid signature primitives for the Quip protocol.
//!
//! The pure, `sp`-free hybrid signature engine — the fixed-size suites
//! ([`Sr25519MlDsa44`], [`Ed25519MlDsa44`], [`Sr25519FnDsa512`],
//! [`Ed25519FnDsa512`]), their wrapper types, the
//! [`HybridSignatureScheme`]/[`HybridVrf`] traits, and the shared
//! [`seed`]/[`domain`] helpers — lives in `quip-crypto-primitives-core` and is
//! re-exported here unchanged. This crate adds the [`substrate`] wrappers that
//! depend on `sp-core`/`sp-io`, keeping all Substrate-bound code isolated from
//! the core so the latter can be reused by `no_std`/wasm signers.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// Re-export the entire sp-free core so existing import paths
// (`quip_crypto_primitives::{Sr25519MlDsa44, HybridSignatureScheme, seed, …}`)
// and `crate::…` references inside `substrate/*` keep resolving unchanged.
pub use quip_crypto_primitives_core::*;

pub mod substrate;
