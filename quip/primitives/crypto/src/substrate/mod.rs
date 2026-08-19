//! Substrate-facing crypto wrappers for hybrid signature suites.
//!
//! These types are meant to look like normal `sp_core` crypto modules so they
//! can be wrapped later with `sp_application_crypto::app_crypto!`.
//!
//! The module currently provides:
//! - [`ed25519_mldsa44`]: the legacy-migrated H1 GRANDPA wrapper, now backed
//!   by `pqhybridsign`
//! - [`ed25519_fndsa512`]: the active H2 GRANDPA wrapper
//! - [`sr25519_mldsa44`]: the legacy-migrated H3 BABE/VRF wrapper, now backed
//!   by `pqhybridsign`
//! - [`sr25519_fndsa512`]: the active H4 BABE/VRF wrapper

pub(crate) mod signature;
pub mod ed25519_fndsa512;
pub mod ed25519_mldsa44;
pub mod sr25519_fndsa512;
pub mod sr25519_mldsa44;
