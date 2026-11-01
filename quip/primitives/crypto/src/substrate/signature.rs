//! Shared Substrate-facing wrapper core for fixed-size hybrid signature suites.
//!
//! This module contains the reusable `sp_core`/`sp_application_crypto`
//! integration for hybrid suites that only need normal public-key and signature
//! functionality:
//! - `Public`
//! - `Signature`
//! - `Pair`
//! - `RuntimePublic`
//! - proof-of-possession helpers
//!
//! BABE-specific VRF types and helpers stay in the concrete H3 wrapper.

use alloc::vec::Vec;
use core::{fmt, marker::PhantomData};

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::{build::Fields, Path, Type, TypeInfo};
use sp_application_crypto::RuntimePublic;
use sp_core::crypto::{
    ByteArray, CryptoType, CryptoTypeId, Derive, DeriveError, DeriveJunction, PublicBytes,
    SecretStringError, SignatureBytes,
};
use sp_core::proof_of_possession::{NonAggregatable, ProofOfPossessionVerifier};
use sp_core::Pair as _;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::MASTER_SEED_LEN;
use crate::HybridSignatureScheme;

/// Domain separator for Substrate's ordinary `Pair::sign` surface.
///
/// Hybrid VRF bindings use pqhybridsign's suite label and binding-message
/// construction instead. Keeping this context non-empty and distinct makes
/// an ordinary signature unusable as a VRF binding even when a caller signs
/// the binding digest verbatim.
pub(crate) const PAIR_SIGNATURE_CONTEXT: &[u8] = b"quip/substrate-pair-signature/v1";

/// Wrapper-specific behavior needed by the shared Substrate glue.
pub trait SubstrateSignatureScheme {
    /// Hybrid suite exposed through this wrapper.
    type Suite: HybridSignatureScheme;

    /// Substrate crypto identifier for the wrapped scheme.
    const CRYPTO_ID: CryptoTypeId;

    /// Applies Substrate derivation semantics to the 32-byte master seed.
    fn derive_seed<Iter: Iterator<Item = DeriveJunction>>(
        seed: [u8; MASTER_SEED_LEN],
        path: Iter,
    ) -> Result<[u8; MASTER_SEED_LEN], DeriveError>;
}

#[doc(hidden)]
pub struct PublicTag<W>(PhantomData<fn() -> W>);

#[doc(hidden)]
pub struct SignatureTag<W>(PhantomData<fn() -> W>);

type InnerPublic<W, const LEN: usize> = PublicBytes<LEN, PublicTag<W>>;
type InnerSignature<W, const LEN: usize> = SignatureBytes<LEN, SignatureTag<W>>;

/// Generic Substrate-style encoded hybrid public key.
#[derive(Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo)]
#[scale_info(skip_type_params(W))]
pub struct Public<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize>(
    InnerPublic<W, PUBLIC_LEN>,
);

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Clone
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> PartialEq
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Eq
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> PartialOrd
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Ord
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> core::hash::Hash
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> CryptoType
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    type Pair = Pair<W, PUBLIC_LEN, SIGNATURE_LEN>;
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> AsRef<[u8]>
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> AsMut<[u8]>
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut()
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::Wraps
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    type Inner = InnerPublic<W, PUBLIC_LEN>;
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> From<InnerPublic<W, PUBLIC_LEN>>
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn from(inner: InnerPublic<W, PUBLIC_LEN>) -> Self {
        Self(inner)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> From<Public<W, PUBLIC_LEN, SIGNATURE_LEN>>
    for InnerPublic<W, PUBLIC_LEN>
{
    fn from(outer: Public<W, PUBLIC_LEN, SIGNATURE_LEN>) -> Self {
        outer.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> AsRef<InnerPublic<W, PUBLIC_LEN>>
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_ref(&self) -> &InnerPublic<W, PUBLIC_LEN> {
        &self.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> AsMut<InnerPublic<W, PUBLIC_LEN>>
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_mut(&mut self) -> &mut InnerPublic<W, PUBLIC_LEN> {
        &mut self.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::ByteArray
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    const LEN: usize = <InnerPublic<W, PUBLIC_LEN> as sp_core::crypto::ByteArray>::LEN;
}

impl<'a, W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> TryFrom<&'a [u8]>
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    type Error = ();

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        InnerPublic::<W, PUBLIC_LEN>::try_from(data).map(Self)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Derive
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::Public
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> fmt::Debug
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Public")
            .field(&<Self as AsRef<[u8]>>::as_ref(self))
            .finish()
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Public<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    /// Wraps a validated suite public key into the Substrate-facing type.
    pub fn from_suite_public(public: <W::Suite as HybridSignatureScheme>::PublicKey) -> Self {
        let mut bytes = [0u8; PUBLIC_LEN];
        bytes.copy_from_slice(public.as_ref());
        Self(InnerPublic::from(bytes))
    }

    /// Parses the wrapper bytes back into the suite public key type.
    pub fn to_suite_public(&self) -> Result<<W::Suite as HybridSignatureScheme>::PublicKey, ()> {
        W::Suite::public_key_from_bytes(<Self as AsRef<[u8]>>::as_ref(self)).map_err(|_| ())
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> RuntimePublic
    for Public<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    type Signature = Signature<W, PUBLIC_LEN, SIGNATURE_LEN>;
    type ProofOfPossession = Signature<W, PUBLIC_LEN, SIGNATURE_LEN>;

    fn all(key_type: sp_application_crypto::KeyTypeId) -> alloc::vec::Vec<Self> {
        sp_io::crypto::crypto_public_keys(key_type, W::CRYPTO_ID.0)
            .into_iter()
            .filter_map(|public| <Self as ByteArray>::from_slice(&public).ok())
            .collect()
    }

    fn generate_pair(
        key_type: sp_application_crypto::KeyTypeId,
        seed: Option<alloc::vec::Vec<u8>>,
    ) -> Self {
        let public = sp_io::crypto::crypto_generate(key_type, W::CRYPTO_ID.0, seed);
        <Self as ByteArray>::from_slice(&public)
            .expect("crypto host returned a valid hybrid public key")
    }

    fn sign<M: AsRef<[u8]>>(
        &self,
        key_type: sp_application_crypto::KeyTypeId,
        msg: &M,
    ) -> Option<Self::Signature> {
        sp_io::crypto::crypto_sign_with(
            key_type,
            W::CRYPTO_ID.0,
            <Self as AsRef<[u8]>>::as_ref(self),
            msg.as_ref(),
        )
        .and_then(|signature| <Self::Signature as ByteArray>::from_slice(&signature).ok())
    }

    fn verify<M: AsRef<[u8]>>(&self, msg: &M, signature: &Self::Signature) -> bool {
        Pair::<W, PUBLIC_LEN, SIGNATURE_LEN>::verify(signature, msg, self)
    }

    fn generate_proof_of_possession(
        &mut self,
        key_type: sp_application_crypto::KeyTypeId,
        owner: &[u8],
    ) -> Option<Self::ProofOfPossession> {
        let statement =
            <Pair<W, PUBLIC_LEN, SIGNATURE_LEN> as NonAggregatable>::proof_of_possession_statement(
                owner,
            );
        sp_io::crypto::crypto_sign_with(
            key_type,
            W::CRYPTO_ID.0,
            <Self as AsRef<[u8]>>::as_ref(self),
            &statement,
        )
        .and_then(|signature| <Self::Signature as ByteArray>::from_slice(&signature).ok())
    }

    fn verify_proof_of_possession(&self, owner: &[u8], pop: &Self::ProofOfPossession) -> bool {
        Pair::<W, PUBLIC_LEN, SIGNATURE_LEN>::verify_proof_of_possession(owner, pop, self)
    }

    fn to_raw_vec(&self) -> alloc::vec::Vec<u8> {
        sp_core::crypto::ByteArray::to_raw_vec(self)
    }
}

#[derive(TypeInfo)]
#[allow(dead_code)]
struct SignatureMetadata2484([u8; 2048], [u8; 436]);

#[derive(TypeInfo)]
#[allow(dead_code)]
struct SignatureMetadata731([u8; 512], [u8; 219]);

/// Generic Substrate-style encoded hybrid signature.
#[derive(Encode, Decode, DecodeWithMemTracking)]
pub struct Signature<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize>(
    InnerSignature<W, SIGNATURE_LEN>,
);

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Clone
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> PartialEq
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Eq
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> core::hash::Hash
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> CryptoType
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    type Pair = Pair<W, PUBLIC_LEN, SIGNATURE_LEN>;
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> TypeInfo
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme + 'static,
{
    type Identity = Self;

    fn type_info() -> Type {
        let fields = match SIGNATURE_LEN {
            2484 => Fields::unnamed().field(|f| f.ty::<SignatureMetadata2484>()),
            731 => Fields::unnamed().field(|f| f.ty::<SignatureMetadata731>()),
            _ => Fields::unnamed().field(|f| f.ty::<InnerSignature<W, SIGNATURE_LEN>>()),
        };

        Type::builder()
            .path(Path::new("Signature", module_path!()))
            .composite(fields)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> AsRef<[u8]>
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> AsMut<[u8]>
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut()
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::Wraps
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    type Inner = InnerSignature<W, SIGNATURE_LEN>;
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> From<InnerSignature<W, SIGNATURE_LEN>>
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn from(inner: InnerSignature<W, SIGNATURE_LEN>) -> Self {
        Self(inner)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize>
    From<Signature<W, PUBLIC_LEN, SIGNATURE_LEN>> for InnerSignature<W, SIGNATURE_LEN>
{
    fn from(outer: Signature<W, PUBLIC_LEN, SIGNATURE_LEN>) -> Self {
        outer.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize>
    AsRef<InnerSignature<W, SIGNATURE_LEN>> for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_ref(&self) -> &InnerSignature<W, SIGNATURE_LEN> {
        &self.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize>
    AsMut<InnerSignature<W, SIGNATURE_LEN>> for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn as_mut(&mut self) -> &mut InnerSignature<W, SIGNATURE_LEN> {
        &mut self.0
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::ByteArray
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    const LEN: usize = <InnerSignature<W, SIGNATURE_LEN> as sp_core::crypto::ByteArray>::LEN;
}

impl<'a, W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> TryFrom<&'a [u8]>
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    type Error = ();

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        InnerSignature::<W, SIGNATURE_LEN>::try_from(data).map(Self)
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::Signature
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> fmt::Debug
    for Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Signature")
            .field(&<Self as AsRef<[u8]>>::as_ref(self))
            .finish()
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Signature<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    /// Wraps a validated suite signature into the Substrate-facing type.
    pub fn from_suite_signature(signature: <W::Suite as HybridSignatureScheme>::Signature) -> Self {
        let mut bytes = [0u8; SIGNATURE_LEN];
        bytes.copy_from_slice(signature.as_ref());
        Self(InnerSignature::from(bytes))
    }

    /// Parses the wrapper bytes back into the suite signature type.
    pub fn to_suite_signature(&self) -> Result<<W::Suite as HybridSignatureScheme>::Signature, ()> {
        W::Suite::signature_from_bytes(<Self as AsRef<[u8]>>::as_ref(self)).map_err(|_| ())
    }
}

/// Generic hybrid keypair backed by the suite master seed.
///
/// The expanded suite secret key is computed once at construction and cached:
/// re-deriving it from the seed costs a full post-quantum keygen (~7x the
/// price of a signature), which is unacceptable on the BABE/GRANDPA signing
/// hot paths.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Pair<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize>
where
    W: SubstrateSignatureScheme,
{
    seed: [u8; MASTER_SEED_LEN],
    secret: <W::Suite as HybridSignatureScheme>::SecretKey,
    #[zeroize(skip)]
    public: Public<W, PUBLIC_LEN, SIGNATURE_LEN>,
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Clone
    for Pair<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
    <W::Suite as HybridSignatureScheme>::SecretKey: Clone,
{
    fn clone(&self) -> Self {
        Self {
            seed: self.seed,
            secret: self.secret.clone(),
            public: self.public.clone(),
        }
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> CryptoType
    for Pair<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    type Pair = Self;
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> NonAggregatable
    for Pair<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> Pair<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    /// Constructs the pair from a validated master seed.
    pub(crate) fn from_master_seed(seed: [u8; MASTER_SEED_LEN]) -> Result<Self, SecretStringError> {
        let (secret, public) =
            W::Suite::from_seed_slice(&seed).map_err(|_| SecretStringError::InvalidSeed)?;

        Ok(Self {
            seed,
            secret,
            public: Public::from_suite_public(public),
        })
    }

    /// Returns the cached suite secret key, expanded once at construction.
    #[cfg(any(feature = "std", feature = "full_crypto"))]
    pub(crate) fn secret(&self) -> &<W::Suite as HybridSignatureScheme>::SecretKey {
        &self.secret
    }
}

impl<W, const PUBLIC_LEN: usize, const SIGNATURE_LEN: usize> sp_core::crypto::Pair
    for Pair<W, PUBLIC_LEN, SIGNATURE_LEN>
where
    W: SubstrateSignatureScheme,
{
    type Public = Public<W, PUBLIC_LEN, SIGNATURE_LEN>;
    type Seed = [u8; MASTER_SEED_LEN];
    type Signature = Signature<W, PUBLIC_LEN, SIGNATURE_LEN>;
    type ProofOfPossession = Signature<W, PUBLIC_LEN, SIGNATURE_LEN>;

    fn derive<Iter: Iterator<Item = DeriveJunction>>(
        &self,
        path: Iter,
        seed: Option<Self::Seed>,
    ) -> Result<(Self, Option<Self::Seed>), DeriveError> {
        let base_seed = seed.unwrap_or(self.seed);
        let derived_seed = W::derive_seed(base_seed, path)?;
        let pair = Self::from_master_seed(derived_seed).map_err(|_| DeriveError::SoftKeyInPath)?;
        Ok((pair, Some(derived_seed)))
    }

    fn from_seed_slice(seed: &[u8]) -> Result<Self, SecretStringError> {
        if seed.len() != MASTER_SEED_LEN {
            return Err(SecretStringError::InvalidSeedLength);
        }

        let mut owned_seed = [0u8; MASTER_SEED_LEN];
        owned_seed.copy_from_slice(seed);
        Self::from_master_seed(owned_seed)
    }

    #[cfg(any(feature = "std", feature = "full_crypto"))]
    fn sign(&self, message: &[u8]) -> Self::Signature {
        Signature::from_suite_signature(W::Suite::sign_deterministic(
            self.secret(),
            message,
            PAIR_SIGNATURE_CONTEXT,
            b"",
        ))
    }

    fn verify<M: AsRef<[u8]>>(sig: &Self::Signature, message: M, pubkey: &Self::Public) -> bool {
        let Ok(public) = pubkey.to_suite_public() else {
            return false;
        };
        let Ok(signature) = sig.to_suite_signature() else {
            return false;
        };

        W::Suite::verify(&public, message.as_ref(), PAIR_SIGNATURE_CONTEXT, &signature)
    }

    fn public(&self) -> Self::Public {
        self.public.clone()
    }

    fn to_raw_vec(&self) -> Vec<u8> {
        self.seed.to_vec()
    }
}
