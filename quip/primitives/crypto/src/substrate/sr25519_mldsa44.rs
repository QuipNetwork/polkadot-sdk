//! Substrate-facing wrapper for the H3 `sr25519 + ML-DSA-44` suite.
//!
//! This module is intentionally split in two layers:
//! - a shared Substrate/app-crypto signature wrapper core in
//!   [`crate::substrate::signature`]
//! - H3-specific VRF and BABE helpers defined here
//!
//! The shared core is reusable by non-VRF suites such as the planned H1
//! `ed25519 + ML-DSA-44` GRANDPA wrapper. This file keeps only the logic that
//! is specific to H3's hybrid VRF construction.
//!
//! The VRF construction delegates to [`pqhybridsign::vrf`]. Its output is
//! derived from the unique sr25519 pre-output alone; the ML-DSA-44 binding is
//! verified as authentication and never contributes entropy.

use alloc::vec::Vec;
use core::fmt;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use hkdf::Hkdf;
use pqhybridsign::{vrf, SrMl44};
use scale_info::{build::Fields, Path, Type, TypeInfo};
use sha2::{Digest, Sha256};
#[cfg(any(feature = "std", feature = "full_crypto"))]
use sp_core::crypto::VrfSecret;
use sp_core::crypto::{CryptoTypeId, DeriveError, DeriveJunction, VrfCrypto, VrfPublic};
use sp_core::sr25519;
use sp_core::Pair as _;

use crate::MASTER_SEED_LEN;
use crate::substrate::signature::{
    Pair as SignaturePair, Public as SignaturePublic, Signature as SignatureWrapper,
    SubstrateSignatureScheme,
};
use crate::suite::sr25519_mldsa44::{
    Sr25519MlDsa44, HYBRID_PK_LEN, HYBRID_SIG_LEN, ML_DSA_SIGNATURE_LEN,
};
#[cfg(any(feature = "std", feature = "full_crypto"))]
use crate::HybridVrf;

/// Unique identifier for the H3 hybrid crypto scheme.
pub const CRYPTO_ID: CryptoTypeId = CryptoTypeId(*b"h344");

/// Length in bytes of the consensus-facing hybrid VRF output.
pub const VRF_OUTPUT_LENGTH: usize = 32;

const SR25519_VRF_PROOF_LEN: usize = 64;
const LIBRARY_VRF_HEADER_LEN: usize = VRF_OUTPUT_LENGTH + SR25519_VRF_PROOF_LEN;
const PQ_SIGNATURE_LEN: usize = ML_DSA_SIGNATURE_LEN;

/// Shared Substrate-signature wrapper marker for H3.
#[doc(hidden)]
pub struct SubstrateH3;

impl SubstrateSignatureScheme for SubstrateH3 {
    type Suite = Sr25519MlDsa44;
    const CRYPTO_ID: CryptoTypeId = CRYPTO_ID;

    fn derive_seed<Iter: Iterator<Item = DeriveJunction>>(
        seed: [u8; MASTER_SEED_LEN],
        path: Iter,
    ) -> Result<[u8; MASTER_SEED_LEN], DeriveError> {
        let sr25519_pair = sr25519::Pair::from_seed(&seed);
        let (_derived_pair, derived_seed) = sr25519_pair.derive(path, Some(seed))?;
        derived_seed.ok_or(DeriveError::SoftKeyInPath)
    }
}

/// Substrate-style encoded H3 public key.
pub type Public = SignaturePublic<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

/// Substrate-style encoded H3 signature.
pub type Signature = SignatureWrapper<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

/// Proof of possession is the same as a normal signature for this
/// non-aggregatable scheme.
pub type ProofOfPossession = Signature;

/// Hybrid keypair backed by the 32-byte suite master seed.
///
/// The pair caches the expanded suite secret key so consensus hot paths do not
/// repeat ML-DSA-44 key generation for every signature.
pub type Pair = SignaturePair<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

fn verified_public_output(
    public: &Public,
    data: &VrfSignData,
    signature: &VrfSignature,
) -> Option<VrfOutput> {
    public
        .vrf_verify(data, signature)
        .then(|| signature.output())
}

/// BABE-facing hybrid VRF input.
///
/// The underlying sr25519 transcript is paired with a canonical byte encoding
/// of the logical input so the PQ binding can sign stable bytes rather than an
/// opaque Merlin transcript object.
#[derive(Clone)]
pub struct VrfInput {
    sr25519: sr25519::vrf::VrfInput,
    binding_input: Vec<u8>,
}

impl VrfInput {
    /// Builds a hybrid VRF input from an sr25519 transcript plus a canonical
    /// byte encoding of the same logical input.
    pub fn new(sr25519: sr25519::vrf::VrfInput, binding_input: Vec<u8>) -> Self {
        Self {
            sr25519,
            binding_input,
        }
    }

    /// Returns the canonical byte encoding used for PQ binding.
    pub fn binding_input(&self) -> &[u8] {
        &self.binding_input
    }

    /// Clones the underlying sr25519 transcript.
    pub fn clone_sr25519(&self) -> sr25519::vrf::VrfInput {
        self.sr25519.clone()
    }
}

/// Hybrid VRF signing data.
#[derive(Clone)]
pub struct VrfSignData {
    input: VrfInput,
}

impl From<VrfInput> for VrfSignData {
    fn from(input: VrfInput) -> Self {
        Self { input }
    }
}

impl AsRef<VrfInput> for VrfSignData {
    fn as_ref(&self) -> &VrfInput {
        &self.input
    }
}

impl VrfSignData {
    /// Builds sign data from a hybrid VRF input.
    pub fn new(input: VrfInput) -> Self {
        input.into()
    }

    /// Returns the wrapped hybrid VRF input.
    pub fn input(&self) -> &VrfInput {
        &self.input
    }

    /// Clones the underlying sr25519 signing data.
    pub fn clone_sr25519(&self) -> sr25519::vrf::VrfSignData {
        self.input.sr25519.clone().into_sign_data()
    }
}

/// Consensus-facing hybrid VRF output.
///
/// This is `SHA-256(sr25519_pre_output)`. The H3 binding is deliberately
/// excluded because it authenticates the proof but is not unique.
#[derive(
    Clone, Eq, PartialEq, Hash, Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo,
)]
pub struct VrfOutput([u8; VRF_OUTPUT_LENGTH]);

impl AsRef<[u8]> for VrfOutput {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for VrfOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("VrfOutput").field(&self.as_ref()).finish()
    }
}

impl VrfOutput {
    fn from_pre_output(pre_output: &sr25519::vrf::VrfPreOutput) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(pre_output.0.as_bytes());

        let digest = hasher.finalize();
        let mut out = [0u8; VRF_OUTPUT_LENGTH];
        out.copy_from_slice(&digest);
        Self(out)
    }

    /// Expands this pre-output digest into `N` bytes using HKDF-SHA256.
    ///
    /// Protocol callers should use the module-level [`make_bytes`] helper,
    /// which verifies the proof and delegates derivation to pqhybridsign.
    pub fn make_bytes<const N: usize>(&self, context: &[u8]) -> [u8; N]
    where
        [u8; N]: Default,
    {
        let hkdf = Hkdf::<Sha256>::from_prk(&self.0)
            .expect("32-byte hybrid output is a valid HKDF pseudo-random key");
        let mut out = [0u8; N];
        hkdf.expand(context, &mut out)
            .expect("HKDF output length is valid");
        out
    }
}

/// Hybrid H3 VRF proof.
///
/// This keeps the native sr25519 VRF proof material intact and adds the
/// ML-DSA-44 binding produced by pqhybridsign's domain-separated
/// `binding_message(SrMl44::LABEL, input, pre_output)` construction.
#[derive(TypeInfo)]
#[allow(dead_code)]
struct PqSignatureMetadata2420([u8; 2048], [u8; 372]);

#[derive(Clone, Eq, PartialEq, Encode, Decode, MaxEncodedLen)]
pub struct VrfSignature {
    /// Native sr25519 VRF proof material.
    pub sr25519: sr25519::vrf::VrfSignature,
    /// ML-DSA-44 binding signature over the canonical input/output hash.
    pub pq_signature: [u8; PQ_SIGNATURE_LEN],
}

impl TypeInfo for VrfSignature {
    type Identity = Self;

    fn type_info() -> Type {
        Type::builder()
            .path(Path::new("VrfSignature", module_path!()))
            .composite(
                Fields::named()
                    .field(|f| {
                        f.ty::<sr25519::vrf::VrfSignature>()
                            .name("sr25519")
                            .type_name("sr25519::vrf::VrfSignature")
                    })
                    .field(|f| {
                        f.ty::<PqSignatureMetadata2420>()
                            .name("pq_signature")
                            .type_name("[u8; PQ_SIGNATURE_LEN]")
                    }),
            )
    }
}

impl fmt::Debug for VrfSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VrfSignature")
            .field("sr25519", &self.sr25519)
            .field("pq_signature", &self.pq_signature.as_slice())
            .finish()
    }
}

impl VrfSignature {
    /// Returns the consensus-facing hybrid output bound to this proof.
    pub fn output(&self) -> VrfOutput {
        VrfOutput::from_pre_output(&self.sr25519.pre_output)
    }

    /// Builds a hybrid VRF proof from an sr25519 proof with an all-zero PQ
    /// binding.
    ///
    /// This exists only for legacy test/support code that fabricates BABE VRF
    /// signatures without a real PQ signer. Production paths should create
    /// proofs via [`VrfSecret::vrf_sign`] on the hybrid pair instead.
    pub fn from_sr25519_with_zero_pq(sr25519: sr25519::vrf::VrfSignature) -> Self {
        Self {
            sr25519,
            pq_signature: [0u8; PQ_SIGNATURE_LEN],
        }
    }
}

fn library_proof(signature: &VrfSignature) -> Vec<u8> {
    let mut proof = signature.sr25519.encode();
    debug_assert_eq!(proof.len(), LIBRARY_VRF_HEADER_LEN);
    proof.extend_from_slice(&signature.pq_signature);
    proof
}

fn signature_from_library_proof(proof: &[u8]) -> Option<VrfSignature> {
    if proof.len() != LIBRARY_VRF_HEADER_LEN + PQ_SIGNATURE_LEN {
        return None;
    }
    let mut encoded_sr25519 = &proof[..LIBRARY_VRF_HEADER_LEN];
    let sr25519 = sr25519::vrf::VrfSignature::decode(&mut encoded_sr25519).ok()?;
    if !encoded_sr25519.is_empty() {
        return None;
    }

    let mut pq_signature = [0u8; PQ_SIGNATURE_LEN];
    pq_signature.copy_from_slice(&proof[LIBRARY_VRF_HEADER_LEN..]);
    Some(VrfSignature {
        sr25519,
        pq_signature,
    })
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pair_vrf_signature(pair: &Pair, input: &VrfInput) -> VrfSignature {
    let secret = pair.secret().to_bytes();
    let mut proof = alloc::vec![0u8; vrf::proof_len::<SrMl44>()];
    let written =
        vrf::sign_deterministic::<SrMl44>(secret.as_ref(), input.binding_input(), &mut proof)
            .expect("stored H3 key and exact VRF proof buffer cannot fail");
    debug_assert_eq!(written, proof.len());
    signature_from_library_proof(&proof)
        .expect("pqhybridsign emits a valid native sr25519 proof and H3 binding")
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pair_vrf_output(pair: &Pair, input: &VrfInput) -> VrfOutput {
    pair_vrf_signature(pair, input).output()
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pair_make_bytes<const N: usize>(pair: &Pair, context: &[u8], input: &VrfInput) -> [u8; N]
where
    [u8; N]: Default,
{
    let signature = pair_vrf_signature(pair, input);
    vrf::make_bytes::<SrMl44, [u8; N]>(
        pair.public().as_ref(),
        input.binding_input(),
        &library_proof(&signature),
        context,
    )
    .expect("fresh H3 proof and public key are valid")
}

impl VrfCrypto for SignaturePair<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
    type VrfInput = VrfInput;
    type VrfPreOutput = VrfOutput;
    type VrfSignData = VrfSignData;
    type VrfSignature = VrfSignature;
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
impl VrfSecret for SignaturePair<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
    fn vrf_pre_output(&self, data: &Self::VrfInput) -> Self::VrfPreOutput {
        pair_vrf_output(self, data)
    }

    fn vrf_sign(&self, data: &Self::VrfSignData) -> Self::VrfSignature {
        pair_vrf_signature(self, data.input())
    }
}

impl VrfCrypto for SignaturePublic<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
    type VrfInput = VrfInput;
    type VrfPreOutput = VrfOutput;
    type VrfSignData = VrfSignData;
    type VrfSignature = VrfSignature;
}

impl VrfPublic for SignaturePublic<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
    fn vrf_verify(&self, data: &Self::VrfSignData, signature: &Self::VrfSignature) -> bool {
        vrf::verify::<SrMl44>(
            self.as_ref(),
            data.input().binding_input(),
            &library_proof(signature),
        )
    }
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
impl HybridVrf for SignaturePair<SubstrateH3, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
    type PublicKey = Public;
    type VrfInput = VrfInput;
    type VrfSignData = VrfSignData;
    type VrfOutput = VrfOutput;
    type VrfSignature = VrfSignature;

    fn vrf_output(&self, input: &Self::VrfInput) -> Self::VrfOutput {
        pair_vrf_output(self, input)
    }

    fn vrf_sign(&self, data: &Self::VrfSignData) -> Self::VrfSignature {
        <Self as VrfSecret>::vrf_sign(self, data)
    }

    fn make_bytes<const N: usize>(&self, context: &[u8], input: &Self::VrfInput) -> [u8; N]
    where
        [u8; N]: Default,
    {
        pair_make_bytes(self, context, input)
    }

    fn vrf_verify(
        public: &Self::PublicKey,
        data: &Self::VrfSignData,
        signature: &Self::VrfSignature,
    ) -> bool {
        public.vrf_verify(data, signature)
    }
}

/// BABE-facing transcript helpers for the hybrid H3 wrapper.
pub mod babe {
    use super::{VrfInput, VrfSignData};
    use alloc::vec::Vec;

    /// VRF context used for BABE per-slot randomness generation.
    pub const RANDOMNESS_VRF_CONTEXT: &[u8] = b"BabeVRFInOutContext";
    /// Length in bytes of BABE randomness.
    pub const RANDOMNESS_LENGTH: usize = 32;
    /// BABE randomness type.
    pub type Randomness = [u8; RANDOMNESS_LENGTH];

    const BABE_ENGINE_ID: &[u8] = b"BABE";

    /// Builds the BABE transcript plus canonical binding bytes from the
    /// upstream `(randomness, slot, epoch)` tuple.
    pub fn make_vrf_transcript(randomness: &Randomness, slot: u64, epoch: u64) -> VrfInput {
        let slot_bytes = slot.to_le_bytes();
        let epoch_bytes = epoch.to_le_bytes();

        let transcript = sp_core::sr25519::vrf::VrfInput::new(
            BABE_ENGINE_ID,
            &[
                (b"slot number", &slot_bytes),
                (b"current epoch", &epoch_bytes),
                (b"chain randomness", randomness),
            ],
        );

        let mut binding_input = Vec::with_capacity(4 + 8 + 8 + RANDOMNESS_LENGTH);
        binding_input.extend_from_slice(BABE_ENGINE_ID);
        binding_input.extend_from_slice(&slot_bytes);
        binding_input.extend_from_slice(&epoch_bytes);
        binding_input.extend_from_slice(randomness);

        VrfInput::new(transcript, binding_input)
    }

    /// Builds hybrid VRF signing data matching BABE's slot/epoch/randomness
    /// transcript shape.
    pub fn make_vrf_sign_data(randomness: &Randomness, slot: u64, epoch: u64) -> VrfSignData {
        make_vrf_transcript(randomness, slot, epoch).into()
    }

    /// Builds the upstream sr25519 BABE signing data corresponding to the same
    /// logical `(randomness, slot, epoch)` input.
    pub fn make_sr25519_vrf_sign_data(
        randomness: &Randomness,
        slot: u64,
        epoch: u64,
    ) -> sp_core::sr25519::vrf::VrfSignData {
        make_vrf_transcript(randomness, slot, epoch)
            .clone_sr25519()
            .into_sign_data()
    }
}

/// Recomputes the hybrid output from a proof after verifying it.
pub fn vrf_output(
    public: &Public,
    data: &VrfSignData,
    signature: &VrfSignature,
) -> Option<VrfOutput> {
    verified_public_output(public, data, signature)
}

/// Derives protocol bytes from a verified hybrid VRF proof.
pub fn make_bytes<const N: usize>(
    public: &Public,
    context: &[u8],
    data: &VrfSignData,
    signature: &VrfSignature,
) -> Option<[u8; N]>
where
    [u8; N]: Default,
{
    let proof = library_proof(signature);
    vrf::verify::<SrMl44>(public.as_ref(), data.input().binding_input(), &proof)
        .then(|| {
            vrf::make_bytes::<SrMl44, [u8; N]>(
                public.as_ref(),
                data.input().binding_input(),
                &proof,
                context,
            )
            .ok()
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::sr25519_mldsa44::Sr25519MlDsa44;
    use crate::HybridSignatureScheme;
    use rand_core::OsRng;
    use sp_core::crypto::{VrfPublic, VrfSecret};

    mod app {
        use crate::substrate::sr25519_mldsa44 as hybrid;
        use sp_application_crypto::{app_crypto, key_types::BABE};

        app_crypto!(hybrid, BABE);
    }

    #[test]
    fn public_and_signature_roundtrip_to_suite_types() {
        let seed = [7u8; MASTER_SEED_LEN];
        let (secret, public) = Sr25519MlDsa44::from_seed_slice(&seed).unwrap();
        let signature = Sr25519MlDsa44::sign_deterministic(&secret, b"hello", b"", b"");

        let wrapped_public = Public::from_suite_public(public.clone());
        let wrapped_signature = Signature::from_suite_signature(signature.clone());

        let decoded_public = wrapped_public.to_suite_public().unwrap();
        let decoded_signature = wrapped_signature.to_suite_signature().unwrap();

        assert_eq!(decoded_public.as_ref(), public.as_ref());
        assert_eq!(decoded_signature.as_ref(), signature.as_ref());
    }

    #[test]
    fn pair_sign_and_verify_matches_suite_verification() {
        let seed = [9u8; MASTER_SEED_LEN];
        let pair = Pair::from_seed(&seed);
        let signature = pair.sign(b"hello babe");

        assert!(Pair::verify(&signature, b"hello babe", &pair.public()));
        assert!(!Pair::verify(&signature, b"wrong", &pair.public()));
    }

    #[test]
    fn app_crypto_can_wrap_the_hybrid_types() {
        let seed = [11u8; MASTER_SEED_LEN];
        let pair = Pair::from_seed(&seed);
        let signature: app::Signature = pair.sign(b"quip").into();
        let public: app::Public = pair.public().into();

        assert!(app::Pair::verify(&signature, b"quip", &public));
    }

    #[test]
    fn hard_suri_derivation_is_supported() {
        let pair = Pair::from_string("//Alice", None).unwrap();
        let pair_again = Pair::from_string("//Alice", None).unwrap();

        assert_eq!(
            AsRef::<[u8]>::as_ref(&pair.public()),
            AsRef::<[u8]>::as_ref(&pair_again.public())
        );
    }

    #[test]
    fn hybrid_vrf_roundtrip_works() {
        let seed = [13u8; MASTER_SEED_LEN];
        let pair = Pair::from_seed(&seed);
        let public = pair.public();
        let randomness = [5u8; babe::RANDOMNESS_LENGTH];
        let sign_data = babe::make_vrf_sign_data(&randomness, 7, 11);

        let signature = VrfSecret::vrf_sign(&pair, &sign_data);

        assert!(VrfPublic::vrf_verify(&public, &sign_data, &signature));
        assert_eq!(
            pair_make_bytes::<32>(&pair, babe::RANDOMNESS_VRF_CONTEXT, sign_data.input()),
            make_bytes::<32>(
                &public,
                babe::RANDOMNESS_VRF_CONTEXT,
                &sign_data,
                &signature
            )
            .unwrap(),
        );
    }

    #[test]
    fn hybrid_vrf_rejects_tampered_pq_binding() {
        let seed = [17u8; MASTER_SEED_LEN];
        let pair = Pair::from_seed(&seed);
        let public = pair.public();
        let randomness = [9u8; babe::RANDOMNESS_LENGTH];
        let sign_data = babe::make_vrf_sign_data(&randomness, 3, 19);
        let mut signature = VrfSecret::vrf_sign(&pair, &sign_data);

        signature.pq_signature[0] ^= 0x01;

        assert!(!VrfPublic::vrf_verify(&public, &sign_data, &signature));
        assert!(make_bytes::<32>(
            &public,
            babe::RANDOMNESS_VRF_CONTEXT,
            &sign_data,
            &signature
        )
        .is_none());
    }

    #[test]
    fn a_second_valid_h3_proof_has_the_same_vrf_output() {
        let pair = Pair::from_seed(&[18u8; MASTER_SEED_LEN]);
        let public = pair.public();
        let input = babe::make_vrf_transcript(&[4u8; babe::RANDOMNESS_LENGTH], 5, 20);
        let secret = pair.secret().to_bytes();

        let evaluate = || {
            let mut proof = vec![0u8; vrf::proof_len::<SrMl44>()];
            vrf::sign::<SrMl44>(
                secret.as_ref(),
                input.binding_input(),
                &mut OsRng,
                &mut proof,
            )
            .expect("evaluate H3 VRF");
            assert!(vrf::verify::<SrMl44>(
                public.as_ref(),
                input.binding_input(),
                &proof
            ));
            (proof.clone(), signature_from_library_proof(&proof).expect("native proof"))
        };

        let (first_proof, first) = evaluate();
        let (second_proof, second) = evaluate();
        assert_ne!(first_proof, second_proof, "the randomized proofs should differ");
        assert_eq!(first.output(), second.output());
    }

    #[test]
    fn hybrid_vrf_output_matches_signed_proof_output() {
        let seed = [19u8; MASTER_SEED_LEN];
        let pair = Pair::from_seed(&seed);
        let randomness = [3u8; babe::RANDOMNESS_LENGTH];
        let input = babe::make_vrf_transcript(&randomness, 21, 2);
        let sign_data = VrfSignData::new(input.clone());

        let output = pair_vrf_output(&pair, &input);
        let signature = VrfSecret::vrf_sign(&pair, &sign_data);

        assert_eq!(output.as_ref(), signature.output().as_ref());
    }

    #[test]
    fn babe_transcript_helper_matches_upstream_sr25519_shape() {
        let seed = [23u8; MASTER_SEED_LEN];
        let pair = sr25519::Pair::from_seed(&seed);
        let randomness = [7u8; babe::RANDOMNESS_LENGTH];
        let slot = 42u64;
        let epoch = 6u64;

        let upstream = babe::make_sr25519_vrf_sign_data(&randomness, slot, epoch);
        let hybrid = babe::make_vrf_sign_data(&randomness, slot, epoch);

        let upstream_signature = pair.vrf_sign(&upstream);
        let hybrid_signature = pair.vrf_sign(&hybrid.clone_sr25519());

        assert_eq!(upstream_signature.pre_output, hybrid_signature.pre_output);
        assert!(pair.public().vrf_verify(&upstream, &upstream_signature));
        assert!(pair
            .public()
            .vrf_verify(&hybrid.clone_sr25519(), &hybrid_signature));
    }
}
