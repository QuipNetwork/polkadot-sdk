//! Substrate-facing wrapper for the H4 `sr25519 + FN-DSA-512` suite.
//!
//! This module is intentionally split in two layers:
//! - a shared Substrate/app-crypto signature wrapper core in
//!   [`crate::substrate::signature`]
//! - H4-specific VRF and BABE helpers defined here
//!
//! The shared core is reusable by non-VRF suites such as H2
//! `ed25519 + FN-DSA-512` GRANDPA wrapper. This file keeps only the logic that
//! is specific to H4's hybrid VRF construction.
//!
//! # Signature-byte dependence and grinding
//!
//! The consensus-facing VRF output hashes the exact unpadded H4 binding
//! signature bytes, not only the sr25519 pre-output. An H4 verifier proves
//! that those bytes form a valid signature, but it cannot prove that the
//! signer used this module's deterministic external nonce or otherwise chose
//! one canonical valid FN-DSA signature. A signer able to produce multiple
//! valid encodings for the same binding message can therefore grind the
//! resulting VRF output. The deterministic signing construction limits honest
//! implementations to one output; nonce/canonicality enforcement is not a
//! property of verification and must not be assumed by consensus callers.

use alloc::vec::Vec;
use core::fmt;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use hkdf::Hkdf;
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
#[cfg(any(feature = "std", feature = "full_crypto"))]
use crate::suite::sr25519_fndsa512::SecretKey as HybridSecretKey;
use crate::suite::sr25519_fndsa512::{Sr25519FnDsa512, HYBRID_PK_LEN, HYBRID_SIG_LEN};
use crate::HybridSignatureScheme;
#[cfg(any(feature = "std", feature = "full_crypto"))]
use crate::HybridVrf;

/// Unique identifier for the H4 hybrid crypto scheme.
pub const CRYPTO_ID: CryptoTypeId = CryptoTypeId(*b"h444");

const HYBRID_VRF_LABEL: &[u8] = b"hybrid-vrf";
#[cfg(any(feature = "std", feature = "full_crypto"))]
const HYBRID_VRF_NONCE_LABEL: &[u8] = b"hybrid-vrf-nonce";
/// Length in bytes of the consensus-facing hybrid VRF output.
pub const VRF_OUTPUT_LENGTH: usize = 32;

const SR25519_PUBLIC_KEY_LEN: usize = 32;
#[cfg(any(feature = "std", feature = "full_crypto"))]
const SR25519_SECRET_KEY_LEN: usize = 64;
const DELTA_OFFSET: usize = 64;
const MIN_HYBRID_SIGNATURE_LEN: usize =
	64 + 1 + pqhybridsign::MIN_FALCON512_SIG_LEN;

/// Shared Substrate-signature wrapper marker for H4.
#[doc(hidden)]
pub struct SubstrateH4;

impl SubstrateSignatureScheme for SubstrateH4 {
	type Suite = Sr25519FnDsa512;
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

/// Substrate-style encoded H4 public key.
pub type Public = SignaturePublic<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

/// Substrate-style encoded H4 signature.
pub type Signature = SignatureWrapper<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

/// Proof of possession is the same as a normal signature for this
/// non-aggregatable scheme.
pub type ProofOfPossession = Signature;

/// Hybrid keypair backed by the 32-byte suite master seed.
///
/// The pair stores only the master seed and a cached public key. The expanded
/// suite secret key is reconstructed on demand for signing.
pub type Pair = SignaturePair<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

fn classical_public_component(public: &Public) -> [u8; SR25519_PUBLIC_KEY_LEN] {
	let bytes = <Public as AsRef<[u8]>>::as_ref(public);

	let mut classical = [0u8; SR25519_PUBLIC_KEY_LEN];
	classical.copy_from_slice(&bytes[..SR25519_PUBLIC_KEY_LEN]);
	classical
}

fn verified_public_output(
	public: &Public,
	data: &VrfSignData,
	signature: &VrfSignature,
) -> Option<VrfOutput> {
	public.vrf_verify(data, signature).then(|| signature.output())
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
		Self { sr25519, binding_input }
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
/// This is the 32-byte `hybrid_output = H(vrf_output || h4_binding_sig)` value that
/// BABE should eventually consume for leader election, `make_bytes`, and epoch
/// randomness.
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
	fn from_parts(
		pre_output: &sr25519::vrf::VrfPreOutput,
		binding_signature: &[u8; HYBRID_SIG_LEN],
	) -> Self {
		let mut hasher = Sha256::new();
		hasher.update(pre_output.0.as_bytes());
		hasher.update(binding_signature_wire_bytes(binding_signature));

		let digest = hasher.finalize();
		let mut out = [0u8; VRF_OUTPUT_LENGTH];
		out.copy_from_slice(&digest);
		Self(out)
	}

	/// Expands the hybrid output into `N` bytes using HKDF-SHA256.
	pub fn make_bytes<const N: usize>(&self, context: &[u8]) -> [u8; N]
	where
		[u8; N]: Default,
	{
		let hkdf = Hkdf::<Sha256>::from_prk(&self.0)
			.expect("32-byte hybrid output is a valid HKDF pseudo-random key");
		let mut out = [0u8; N];
		hkdf.expand(context, &mut out).expect("HKDF output length is valid");
		out
	}
}

/// Hybrid H4 VRF proof.
///
/// This keeps the native sr25519 VRF proof material intact and adds the full
/// H4 binding signature over `H("hybrid-vrf" || input || vrf_output)`.
#[derive(TypeInfo)]
#[allow(dead_code)]
struct PqSignatureMetadata731([u8; 512], [u8; 219]);

#[derive(Clone, Eq, PartialEq, Encode, Decode, MaxEncodedLen)]
pub struct VrfSignature {
	/// Native sr25519 VRF proof material.
	pub sr25519: sr25519::vrf::VrfSignature,
	/// Full H4 binding signature over the canonical input/output hash.
	pub pq_signature: [u8; HYBRID_SIG_LEN],
}

impl TypeInfo for VrfSignature {
	type Identity = Self;

	fn type_info() -> Type {
		Type::builder().path(Path::new("VrfSignature", module_path!())).composite(
			Fields::named()
				.field(|f| {
					f.ty::<sr25519::vrf::VrfSignature>()
						.name("sr25519")
						.type_name("sr25519::vrf::VrfSignature")
				})
				.field(|f| {
					f.ty::<PqSignatureMetadata731>()
						.name("pq_signature")
						.type_name("[u8; HYBRID_SIG_LEN]")
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
		VrfOutput::from_parts(&self.sr25519.pre_output, &self.pq_signature)
	}

	/// Builds a hybrid VRF proof from an sr25519 proof with an all-zero PQ
	/// binding.
	///
	/// This exists only for legacy test/support code that fabricates BABE VRF
	/// signatures without a real PQ signer. Production paths should create
	/// proofs via [`VrfSecret::vrf_sign`] on the hybrid pair instead.
	pub fn from_sr25519_with_zero_pq(sr25519: sr25519::vrf::VrfSignature) -> Self {
		Self { sr25519, pq_signature: [0u8; HYBRID_SIG_LEN] }
	}
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn sr25519_pair(secret: &HybridSecretKey) -> sr25519::Pair {
	let secret = secret.to_bytes();
	sr25519::Pair::from_seed_slice(&secret[..SR25519_SECRET_KEY_LEN])
		.expect("stored H4 secret key contains a valid sr25519 secret")
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pq_binding_signature(
	input: &VrfInput,
	pre_output: &sr25519::vrf::VrfPreOutput,
	secret: &HybridSecretKey,
) -> [u8; HYBRID_SIG_LEN] {
	let message = binding_message(input, pre_output);
	// Consensus-critical deterministic nonce:
	// SHA256("hybrid-vrf-nonce" || binding_input || vrf_pre_output).
	let nonce = binding_nonce(input, pre_output);
	Sr25519FnDsa512::sign_deterministic(secret, &message, b"", &nonce).to_bytes()
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pair_vrf_output(pair: &Pair, input: &VrfInput) -> VrfOutput {
	let secret = pair.expanded_secret();
	let sr25519_pre_output = sr25519_pair(&secret).vrf_pre_output(&input.clone_sr25519());
	let pq_signature = pq_binding_signature(input, &sr25519_pre_output, &secret);
	VrfOutput::from_parts(&sr25519_pre_output, &pq_signature)
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pair_make_bytes<const N: usize>(pair: &Pair, context: &[u8], input: &VrfInput) -> [u8; N]
where
	[u8; N]: Default,
{
	pair_vrf_output(pair, input).make_bytes(context)
}

impl VrfCrypto for SignaturePair<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
	type VrfInput = VrfInput;
	type VrfPreOutput = VrfOutput;
	type VrfSignData = VrfSignData;
	type VrfSignature = VrfSignature;
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
impl VrfSecret for SignaturePair<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
	fn vrf_pre_output(&self, data: &Self::VrfInput) -> Self::VrfPreOutput {
		pair_vrf_output(self, data)
	}

	fn vrf_sign(&self, data: &Self::VrfSignData) -> Self::VrfSignature {
		let secret = self.expanded_secret();
		let sr25519 = sr25519_pair(&secret).vrf_sign(&data.clone_sr25519());
		let pq_signature = pq_binding_signature(data.input(), &sr25519.pre_output, &secret);

		VrfSignature { sr25519, pq_signature }
	}
}

impl VrfCrypto for SignaturePublic<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
	type VrfInput = VrfInput;
	type VrfPreOutput = VrfOutput;
	type VrfSignData = VrfSignData;
	type VrfSignature = VrfSignature;
}

impl VrfPublic for SignaturePublic<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
	fn vrf_verify(&self, data: &Self::VrfSignData, signature: &Self::VrfSignature) -> bool {
		let classical = classical_public_component(self);
		let classical = sr25519::Public::from_raw(classical);

		if !classical.vrf_verify(&data.clone_sr25519(), &signature.sr25519) {
			return false;
		}

		let message = binding_message(data.input(), &signature.sr25519.pre_output);
		let Ok(public) = self.to_suite_public() else {
			return false;
		};
		let Ok(binding_signature) = Sr25519FnDsa512::signature_from_bytes(&signature.pq_signature)
		else {
			return false;
		};
		Sr25519FnDsa512::verify(&public, &message, b"", &binding_signature)
	}
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
impl HybridVrf for SignaturePair<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN> {
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

/// BABE-facing transcript helpers for the hybrid H4 wrapper.
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
		make_vrf_transcript(randomness, slot, epoch).clone_sr25519().into_sign_data()
	}
}

fn binding_message(
	input: &VrfInput,
	pre_output: &sr25519::vrf::VrfPreOutput,
) -> [u8; VRF_OUTPUT_LENGTH] {
	let mut hasher = Sha256::new();
	hasher.update(HYBRID_VRF_LABEL);
	hasher.update(input.binding_input());
	hasher.update(pre_output.0.as_bytes());

	let digest = hasher.finalize();
	let mut out = [0u8; VRF_OUTPUT_LENGTH];
	out.copy_from_slice(&digest);
	out
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn binding_nonce(
	input: &VrfInput,
	pre_output: &sr25519::vrf::VrfPreOutput,
) -> [u8; VRF_OUTPUT_LENGTH] {
	let mut hasher = Sha256::new();
	hasher.update(HYBRID_VRF_NONCE_LABEL);
	hasher.update(input.binding_input());
	hasher.update(pre_output.0.as_bytes());

	let digest = hasher.finalize();
	let mut out = [0u8; VRF_OUTPUT_LENGTH];
	out.copy_from_slice(&digest);
	out
}

fn binding_signature_wire_bytes(signature: &[u8; HYBRID_SIG_LEN]) -> &[u8] {
	let real_len = MIN_HYBRID_SIGNATURE_LEN + usize::from(signature[DELTA_OFFSET]);
	&signature[..real_len]
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
	vrf_output(public, data, signature).map(|output| output.make_bytes(context))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::suite::sr25519_fndsa512::Sr25519FnDsa512;
	use crate::HybridSignatureScheme;
	use sp_core::crypto::{VrfPublic, VrfSecret};

	mod app {
		use crate::substrate::sr25519_fndsa512 as hybrid;
		use sp_application_crypto::{app_crypto, key_types::BABE};

		app_crypto!(hybrid, BABE);
	}

	#[test]
	fn public_and_signature_roundtrip_to_suite_types() {
		let seed = [7u8; MASTER_SEED_LEN];
		let (secret, public) = Sr25519FnDsa512::from_seed_slice(&seed).unwrap();
		let signature = Sr25519FnDsa512::sign_deterministic(&secret, b"hello", b"", b"");
		let signature_bytes = signature.to_bytes();

		let wrapped_public = Public::from_suite_public(public.clone());
		let wrapped_signature = Signature::from_suite_signature(signature);

		let decoded_public = wrapped_public.to_suite_public().unwrap();
		let decoded_signature = wrapped_signature.to_suite_signature().unwrap();

		assert_eq!(decoded_public.as_ref(), public.as_ref());
		assert_eq!(decoded_signature.as_ref(), signature_bytes.as_ref());
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
			make_bytes::<32>(&public, babe::RANDOMNESS_VRF_CONTEXT, &sign_data, &signature)
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
		assert!(make_bytes::<32>(&public, babe::RANDOMNESS_VRF_CONTEXT, &sign_data, &signature)
			.is_none());
	}

	#[test]
	fn hybrid_vrf_rejects_noncanonical_binding_padding() {
		let pair = Pair::from_seed(&[18u8; MASTER_SEED_LEN]);
		let public = pair.public();
		let sign_data = babe::make_vrf_sign_data(&[4u8; babe::RANDOMNESS_LENGTH], 5, 20);
		let mut signature = VrfSecret::vrf_sign(&pair, &sign_data);
		assert!(signature.pq_signature[DELTA_OFFSET] > 0);
		signature.pq_signature[DELTA_OFFSET] -= 1;
		let padding_offset = binding_signature_wire_bytes(&signature.pq_signature).len();
		signature.pq_signature[padding_offset] = 1;

		assert!(!VrfPublic::vrf_verify(&public, &sign_data, &signature));
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
		assert!(pair.public().vrf_verify(&hybrid.clone_sr25519(), &hybrid_signature));
	}
}
