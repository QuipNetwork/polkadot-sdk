//! Concrete hybrid signature suite definitions.
//!
//! Each submodule in this directory defines one fully assembled hybrid suite:
//! wrapper types, serialization layout, suite label, and a
//! [`crate::HybridSignatureScheme`] implementation.

pub mod ed25519_fndsa512;
pub mod ed25519_mldsa44;
mod fndsa512;
mod mldsa44;
pub mod sr25519_fndsa512;
pub mod sr25519_mldsa44;
