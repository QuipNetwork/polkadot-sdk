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
//! The VRF construction delegates to [`pqhybridsign::vrf`]. Its output is
//! derived from the unique sr25519 pre-output alone; the FN-DSA-512 binding is
//! verified as authentication and never contributes entropy. Consequently,
//! multiple valid proofs for one key and input cannot move BABE's score or
//! randomness contribution.

use alloc::vec::Vec;
use core::fmt;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use hkdf::Hkdf;
use pqhybridsign::{vrf, SrFn512, MIN_FALCON512_SIG_LEN};
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
use crate::suite::sr25519_fndsa512::{Sr25519FnDsa512, HYBRID_PK_LEN, HYBRID_SIG_LEN};
#[cfg(any(feature = "std", feature = "full_crypto"))]
use crate::HybridVrf;

/// Unique identifier for the H4 hybrid crypto scheme.
pub const CRYPTO_ID: CryptoTypeId = CryptoTypeId(*b"h444");

/// Length in bytes of the consensus-facing hybrid VRF output.
pub const VRF_OUTPUT_LENGTH: usize = 32;

const SR25519_VRF_PROOF_LEN: usize = 64;
const LIBRARY_VRF_HEADER_LEN: usize = VRF_OUTPUT_LENGTH + SR25519_VRF_PROOF_LEN;
const VRF_BINDING_DELTA_OFFSET: usize = 0;
const MIN_VRF_BINDING_LEN: usize = 1 + MIN_FALCON512_SIG_LEN;

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
/// The pair caches the expanded suite secret key so consensus hot paths do not
/// repeat FN-DSA-512 key generation for every signature.
pub type Pair = SignaturePair<SubstrateH4, HYBRID_PK_LEN, HYBRID_SIG_LEN>;

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
/// This is `SHA-256(sr25519_pre_output)`. The H4 binding is deliberately
/// excluded: it authenticates the proof but is not a unique signature and
/// therefore cannot safely contribute to consensus randomness.
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
		hkdf.expand(context, &mut out).expect("HKDF output length is valid");
		out
	}
}

/// Hybrid H4 VRF proof.
///
/// This keeps the native sr25519 VRF proof material intact and adds the H4
/// FN-DSA-512 binding produced by pqhybridsign's domain-separated
/// `binding_message(SrFn512::LABEL, input, pre_output)` construction.
#[derive(TypeInfo)]
#[allow(dead_code)]
struct PqSignatureMetadata731([u8; 512], [u8; 219]);

#[derive(Clone, Eq, PartialEq, Encode, Decode, MaxEncodedLen)]
pub struct VrfSignature {
	/// Native sr25519 VRF proof material.
	pub sr25519: sr25519::vrf::VrfSignature,
	/// Delta byte plus FN-DSA-512 binding, zero-padded to the legacy field size.
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
		VrfOutput::from_pre_output(&self.sr25519.pre_output)
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

fn library_proof(signature: &VrfSignature) -> Option<Vec<u8>> {
	let binding_len = MIN_VRF_BINDING_LEN
		.checked_add(usize::from(signature.pq_signature[VRF_BINDING_DELTA_OFFSET]))?;
	if binding_len > signature.pq_signature.len()
		|| signature.pq_signature[binding_len..].iter().any(|byte| *byte != 0)
	{
		return None;
	}

	let mut proof = signature.sr25519.encode();
	if proof.len() != LIBRARY_VRF_HEADER_LEN {
		return None;
	}
	proof.extend_from_slice(&signature.pq_signature[..binding_len]);
	Some(proof)
}

fn signature_from_library_proof(proof: &[u8]) -> Option<VrfSignature> {
	if proof.len() < LIBRARY_VRF_HEADER_LEN + MIN_VRF_BINDING_LEN {
		return None;
	}
	let mut encoded_sr25519 = &proof[..LIBRARY_VRF_HEADER_LEN];
	let sr25519 = sr25519::vrf::VrfSignature::decode(&mut encoded_sr25519).ok()?;
	if !encoded_sr25519.is_empty() {
		return None;
	}

	let binding = &proof[LIBRARY_VRF_HEADER_LEN..];
	if binding.len() > HYBRID_SIG_LEN {
		return None;
	}
	let mut pq_signature = [0u8; HYBRID_SIG_LEN];
	pq_signature[..binding.len()].copy_from_slice(binding);
	Some(VrfSignature { sr25519, pq_signature })
}

#[cfg(any(feature = "std", feature = "full_crypto"))]
fn pair_vrf_signature(pair: &Pair, input: &VrfInput) -> VrfSignature {
	let secret = pair.secret().to_bytes();
	let mut proof = alloc::vec![0u8; vrf::max_proof_len_delta::<SrFn512>()];
	let written = vrf::sign_delta_deterministic::<SrFn512>(
		secret.as_ref(),
		input.binding_input(),
		&mut proof,
	)
	.expect("stored H4 key and exact VRF proof buffer cannot fail");
	proof.truncate(written);
	signature_from_library_proof(&proof)
		.expect("pqhybridsign emits a valid native sr25519 proof and H4 binding")
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
	let proof = library_proof(&signature).expect("fresh H4 proof has canonical padding");
	vrf::make_bytes_delta::<SrFn512, [u8; N]>(
		pair.public().as_ref(),
		input.binding_input(),
		&proof,
		context,
	)
	.expect("fresh H4 proof and public key are valid")
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
		pair_vrf_signature(self, data.input())
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
		let Some(proof) = library_proof(signature) else {
			return false;
		};
		vrf::verify_delta::<SrFn512>(self.as_ref(), data.input().binding_input(), &proof)
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
	let proof = library_proof(signature)?;
	vrf::verify_delta::<SrFn512>(public.as_ref(), data.input().binding_input(), &proof)
		.then(|| {
			vrf::make_bytes_delta::<SrFn512, [u8; N]>(
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
	use crate::suite::sr25519_fndsa512::Sr25519FnDsa512;
	use crate::HybridSignatureScheme;
	use pqhybridsign::{
		component::{ClassicalScheme, PqScheme},
		encoding_delta,
		suite::DeltaSuite,
	};
	use rand_core::OsRng;
	use sp_core::crypto::{VrfPublic, VrfSecret};

	fn randomized_binding_proof(pair: &Pair, input: &VrfInput, base_proof: &[u8]) -> Vec<u8> {
		let secret = pair.secret().to_bytes();
		let classical_secret_len =
			<<SrFn512 as DeltaSuite>::Classical as ClassicalScheme>::SECRET_KEY_LEN;
		let mut binding = vec![0u8; <<SrFn512 as DeltaSuite>::Pq as PqScheme>::SIGNATURE_LEN];
		let message = vrf::binding_message(
			SrFn512::LABEL,
			input.binding_input(),
			&base_proof[..VRF_OUTPUT_LENGTH],
		);
		<<SrFn512 as DeltaSuite>::Pq as PqScheme>::sign(
			&secret[classical_secret_len..],
			&message,
			&mut OsRng,
			&mut binding,
		)
		.expect("sign randomized FN-DSA-512 binding");

		let mut proof = base_proof[..LIBRARY_VRF_HEADER_LEN].to_vec();
		proof.push(
			encoding_delta::encode_delta_byte(binding.len(), SrFn512::MIN_PQ_SIG_LEN)
				.expect("H4 binding length is representable"),
		);
		proof.extend_from_slice(&binding);
		proof
	}

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
		let wrong_pair = Pair::from_seed(&[14u8; MASTER_SEED_LEN]);
		let wrong_input = babe::make_vrf_sign_data(&randomness, 8, 11);

		assert!(VrfPublic::vrf_verify(&public, &sign_data, &signature));
		assert!(!VrfPublic::vrf_verify(&wrong_pair.public(), &sign_data, &signature));
		assert!(!VrfPublic::vrf_verify(&public, &wrong_input, &signature));
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
		assert!(signature.pq_signature[VRF_BINDING_DELTA_OFFSET] > 0);
		signature.pq_signature[VRF_BINDING_DELTA_OFFSET] -= 1;
		let padding_offset = MIN_VRF_BINDING_LEN
			+ usize::from(signature.pq_signature[VRF_BINDING_DELTA_OFFSET]);
		signature.pq_signature[padding_offset] = 1;

		assert!(!VrfPublic::vrf_verify(&public, &sign_data, &signature));
	}

	#[test]
	fn a_second_valid_proof_has_the_same_vrf_output() {
		let pair = Pair::from_seed(&[20u8; MASTER_SEED_LEN]);
		let public = pair.public();
		let input = babe::make_vrf_transcript(&[6u8; babe::RANDOMNESS_LENGTH], 9, 3);
		let secret = pair.secret().to_bytes();
		let mut base = vec![0u8; vrf::max_proof_len_delta::<SrFn512>()];
		let written = vrf::sign_delta::<SrFn512>(
			secret.as_ref(),
			input.binding_input(),
			&mut OsRng,
			&mut base,
		)
		.expect("evaluate H4 VRF");
		base.truncate(written);

		let evaluate = || {
			let proof = randomized_binding_proof(&pair, &input, &base);
			assert!(vrf::verify_delta::<SrFn512>(
				public.as_ref(),
				input.binding_input(),
				&proof
			));
			(proof.clone(), signature_from_library_proof(&proof).expect("native proof"))
		};

		let (first_proof, first) = evaluate();
		let (second_proof, second) = evaluate();
		assert_ne!(
			&first_proof[LIBRARY_VRF_HEADER_LEN..],
			&second_proof[LIBRARY_VRF_HEADER_LEN..],
			"the randomized FN-DSA-512 bindings should differ"
		);
		assert_eq!(first.output(), second.output());
	}

	#[test]
	fn grinding_many_valid_proofs_cannot_move_the_output() {
		const ATTEMPTS: usize = 64;

		let pair = Pair::from_seed(&[21u8; MASTER_SEED_LEN]);
		let public = pair.public();
		let input = babe::make_vrf_transcript(&[8u8; babe::RANDOMNESS_LENGTH], 10, 4);
		let base = library_proof(&pair_vrf_signature(&pair, &input)).expect("base proof");
		let mut bindings = std::collections::BTreeSet::new();
		let mut expected_output = None;

		for _ in 0..ATTEMPTS {
			let proof = randomized_binding_proof(&pair, &input, &base);
			assert!(vrf::verify_delta::<SrFn512>(
				public.as_ref(),
				input.binding_input(),
				&proof
			));
			let signature = signature_from_library_proof(&proof).expect("native proof");
			let output = signature.output();
			match &expected_output {
				Some(expected) => assert_eq!(expected, &output),
				None => expected_output = Some(output),
			}
			bindings.insert(proof[LIBRARY_VRF_HEADER_LEN..].to_vec());
		}

		assert_eq!(bindings.len(), ATTEMPTS, "all accepted bindings should be distinct");
	}

	#[test]
	fn vrf_binding_and_pair_signature_are_domain_separated() {
		let pair = Pair::from_seed(&[22u8; MASTER_SEED_LEN]);
		let public = pair.public();
		let input = babe::make_vrf_transcript(&[10u8; babe::RANDOMNESS_LENGTH], 11, 5);
		let sign_data = VrfSignData::new(input.clone());
		let vrf_signature = VrfSecret::vrf_sign(&pair, &sign_data);
		let binding_digest = vrf::binding_message(
			SrFn512::LABEL,
			input.binding_input(),
			vrf_signature.sr25519.pre_output.0.as_bytes(),
		);

		let ordinary = pair.sign(&binding_digest);
		assert!(Pair::verify(&ordinary, binding_digest, &public));

		let mut ordinary_as_vrf = vrf_signature.clone();
		ordinary_as_vrf.pq_signature.copy_from_slice(ordinary.as_ref());
		assert!(!VrfPublic::vrf_verify(&public, &sign_data, &ordinary_as_vrf));

		let binding_as_ordinary = Signature::try_from(vrf_signature.pq_signature.as_slice())
			.expect("fixed-size wrapper");
		assert!(!Pair::verify(&binding_as_ordinary, binding_digest, &public));
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
