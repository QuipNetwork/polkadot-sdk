// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! VRFs backed by [Bandersnatch](https://neuromancer.sk/std/bls/Bandersnatch),
//! an elliptic curve built over BLS12-381 scalar field.
//!
//! The primitive can operate both as a regular VRF or as an anonymized Ring VRF.

#[cfg(feature = "full_crypto")]
use crate::crypto::VrfSecret;
use crate::{
	crypto::{
		ByteArray, CryptoType, CryptoTypeId, DeriveError, DeriveJunction, Pair as TraitPair,
		PublicBytes, SecretStringError, SignatureBytes, UncheckedFrom, VrfPublic,
	},
	proof_of_possession::NonAggregatable,
};
use alloc::{vec, vec::Vec};
use ark_vrf::{
	reexports::ark_serialize::{CanonicalDeserialize, CanonicalSerialize},
	suites::bandersnatch::{self, BandersnatchSha512Ell2 as BandersnatchSuite, Secret, VrfIo},
};
use codec::{Decode, DecodeWithMemTracking, Encode, EncodeLike, MaxEncodedLen};
use scale_info::TypeInfo;

/// Identifier used to match public keys against bandersnatch-vrf keys.
pub const CRYPTO_ID: CryptoTypeId = CryptoTypeId(*b"band");

/// The byte length of secret key seed.
pub const SEED_SERIALIZED_SIZE: usize = 32;

/// The byte length of serialized public key.
pub const PUBLIC_SERIALIZED_SIZE: usize = 32;

/// The byte length of serialized signature (thin VRF proof: 32-byte point + 32-byte scalar).
pub const SIGNATURE_SERIALIZED_SIZE: usize = 64;

/// The byte length of serialized pre-output.
pub const PREOUT_SERIALIZED_SIZE: usize = 32;

#[doc(hidden)]
pub struct BandersnatchTag;

/// Bandersnatch public key.
pub type Public = PublicBytes<PUBLIC_SERIALIZED_SIZE, BandersnatchTag>;

impl CryptoType for Public {
	type Pair = Pair;
}

/// Bandersnatch Schnorr signature.
pub type Signature = SignatureBytes<SIGNATURE_SERIALIZED_SIZE, BandersnatchTag>;

/// Proof of Possession is the same as Signature for Bandersnatch
pub type ProofOfPossession = Signature;

impl CryptoType for Signature {
	type Pair = Pair;
}

/// The raw secret seed, which can be used to reconstruct the secret [`Pair`].
type Seed = [u8; SEED_SERIALIZED_SIZE];

/// Bandersnatch secret key.
#[derive(Clone)]
pub struct Pair {
	secret: Secret,
	seed: Seed,
}

impl Pair {
	/// Get the key seed.
	pub fn seed(&self) -> Seed {
		self.seed
	}
}

impl TraitPair for Pair {
	type Seed = Seed;
	type Public = Public;
	type Signature = Signature;
	type ProofOfPossession = Signature;

	/// Make a new key pair from secret seed material.
	///
	/// The slice must be 32 bytes long or it will return an error.
	fn from_seed_slice(seed_slice: &[u8]) -> Result<Pair, SecretStringError> {
		if seed_slice.len() != SEED_SERIALIZED_SIZE {
			return Err(SecretStringError::InvalidSeedLength);
		}
		let mut seed = [0; SEED_SERIALIZED_SIZE];
		seed.copy_from_slice(seed_slice);
		let secret = Secret::from_seed(seed);
		Ok(Pair { secret, seed })
	}

	/// Derive a child key from a series of given (hard) junctions.
	///
	/// Soft junctions are not supported.
	fn derive<Iter: Iterator<Item = DeriveJunction>>(
		&self,
		path: Iter,
		_seed: Option<Seed>,
	) -> Result<(Pair, Option<Seed>), DeriveError> {
		let derive_hard = |seed, cc| -> Seed {
			("bandersnatch-vrf-HDKD", seed, cc).using_encoded(sp_crypto_hashing::blake2_256)
		};

		let mut seed = self.seed();
		for p in path {
			if let DeriveJunction::Hard(cc) = p {
				seed = derive_hard(seed, cc);
			} else {
				return Err(DeriveError::SoftKeyInPath);
			}
		}
		Ok((Self::from_seed(&seed), Some(seed)))
	}

	fn public(&self) -> Public {
		let public = self.secret.public();
		let mut raw = [0; PUBLIC_SERIALIZED_SIZE];
		public
			.serialize_compressed(raw.as_mut_slice())
			.expect("serialization length is constant and checked by test; qed");
		Public::unchecked_from(raw)
	}

	#[cfg(feature = "full_crypto")]
	fn sign(&self, data: &[u8]) -> Signature {
		vrf::vrf_sign(&self.secret, &[], data)
	}

	fn verify<M: AsRef<[u8]>>(signature: &Signature, data: M, public: &Public) -> bool {
		vrf::vrf_verify(public, &[], signature, data.as_ref())
	}

	/// Return a vector filled with the seed.
	fn to_raw_vec(&self) -> Vec<u8> {
		self.seed().to_vec()
	}
}

impl CryptoType for Pair {
	type Pair = Pair;
}

impl NonAggregatable for Pair {}

/// Bandersnatch VRF types and operations.
pub mod vrf {
	use super::*;
	use crate::crypto::VrfCrypto;

	/// [`VrfSignature`] serialized size.
	pub const VRF_SIGNATURE_SERIALIZED_SIZE: usize =
		PREOUT_SERIALIZED_SIZE + SIGNATURE_SERIALIZED_SIZE;

	/// VRF input to construct a [`VrfPreOutput`] instance and embeddable in [`VrfSignData`].
	#[derive(Clone, Debug)]
	pub struct VrfInput(pub(super) bandersnatch::Input);

	impl VrfInput {
		/// Construct a new VRF input.
		///
		/// Hash to Curve (H2C) using Elligator2.
		pub fn new(data: &[u8]) -> Self {
			Self(bandersnatch::Input::new(data).expect("H2C for Bandersnatch can't fail; qed"))
		}
	}

	/// VRF pre-output derived from [`VrfInput`] using a [`VrfSecret`].
	///
	/// This object is hashed to produce the actual VRF output.
	#[derive(Clone, Debug, PartialEq, Eq)]
	pub struct VrfPreOutput(pub(super) bandersnatch::Output);

	impl Encode for VrfPreOutput {
		fn encode(&self) -> Vec<u8> {
			let mut bytes = [0; PREOUT_SERIALIZED_SIZE];
			self.0
				.serialize_compressed(bytes.as_mut_slice())
				.expect("serialization length is constant and checked by test; qed");
			bytes.encode()
		}
	}

	impl Decode for VrfPreOutput {
		fn decode<R: codec::Input>(i: &mut R) -> Result<Self, codec::Error> {
			let buf = <[u8; PREOUT_SERIALIZED_SIZE]>::decode(i)?;
			let preout = bandersnatch::Output::deserialize_compressed(buf.as_slice())
				.map_err(|_| "vrf-preout decode error: bad preout")?;
			Ok(VrfPreOutput(preout))
		}
	}

	// `VrfPreOutput` resolves to:
	// ```
	// pub struct Affine<P: TECurveConfig> {
	//     pub x: P::BaseField,
	//     pub y: P::BaseField,
	// }
	// ```
	// where each `P::BaseField` contains a `pub struct BigInt<const N: usize>(pub [u64; N]);`
	// Since none of these structures is allocated on the heap, we don't need any special
	// memory tracking logic. We can simply implement `DecodeWithMemTracking`.
	impl DecodeWithMemTracking for VrfPreOutput {}

	impl EncodeLike for VrfPreOutput {}

	impl MaxEncodedLen for VrfPreOutput {
		fn max_encoded_len() -> usize {
			<[u8; PREOUT_SERIALIZED_SIZE]>::max_encoded_len()
		}
	}

	impl TypeInfo for VrfPreOutput {
		type Identity = [u8; PREOUT_SERIALIZED_SIZE];

		fn type_info() -> scale_info::Type {
			Self::Identity::type_info()
		}
	}

	/// Data to be signed via one of the two provided vrf flavors.
	///
	/// The object contains the VRF input and additional data to be signed together
	/// with the VRF input. Additional data doesn't influence the VRF output.
	///
	/// The `input` is a [`VrfInput`]s which, during the signing procedure, is first mapped
	/// to a [`VrfPreOutput`].
	#[derive(Clone)]
	pub struct VrfSignData {
		/// VRF input.
		pub vrf_input: VrfInput,
		/// Additional data.
		pub aux_data: Vec<u8>,
	}

	impl VrfSignData {
		/// Construct a new data to be signed.
		pub fn new(vrf_input_data: &[u8], aux_data: &[u8]) -> Self {
			Self { vrf_input: VrfInput::new(vrf_input_data), aux_data: aux_data.to_vec() }
		}
	}

	/// VRF signature.
	///
	/// Includes both the VRF proof and the pre-output generated from the
	/// [`VrfSignData::vrf_input`].
	///
	/// Refer to [`VrfSignData`] for more details.
	#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, MaxEncodedLen, TypeInfo)]
	pub struct VrfSignature {
		/// VRF pre-output.
		pub pre_output: VrfPreOutput,
		/// VRF proof.
		pub proof: Signature,
	}

	#[cfg(feature = "full_crypto")]
	impl VrfCrypto for Pair {
		type VrfInput = VrfInput;
		type VrfPreOutput = VrfPreOutput;
		type VrfSignData = VrfSignData;
		type VrfSignature = VrfSignature;
	}

	#[cfg(feature = "full_crypto")]
	pub(super) fn vrf_sign(secret: &Secret, ios: &[VrfIo], aux_data: &[u8]) -> Signature {
		use ark_vrf::thin::Prover;
		let proof_impl = secret.prove(ios, aux_data);
		let mut proof = Signature::default();
		proof_impl
			.serialize_compressed(proof.0.as_mut_slice())
			.expect("serialization length is constant and checked by test; qed");
		proof
	}

	pub(super) fn vrf_verify(
		public: &Public,
		ios: &[VrfIo],
		proof: &Signature,
		aux_data: &[u8],
	) -> bool {
		use ark_vrf::thin::Verifier;
		let Ok(public) = bandersnatch::Public::deserialize_compressed(public.as_slice()) else {
			return false;
		};
		let Ok(proof) =
			ark_vrf::thin::Proof::<BandersnatchSuite>::deserialize_compressed(proof.as_slice())
		else {
			return false;
		};
		public.verify(ios, aux_data, &proof).is_ok()
	}

	#[cfg(feature = "full_crypto")]
	impl VrfSecret for Pair {
		fn vrf_sign(&self, data: &VrfSignData) -> VrfSignature {
			let output = self.secret.output(data.vrf_input.0);
			let pre_output = VrfPreOutput(output);
			let io = VrfIo { input: data.vrf_input.0, output };
			let proof = vrf_sign(&self.secret, &[io], &data.aux_data);
			VrfSignature { pre_output, proof }
		}

		fn vrf_pre_output(&self, input: &Self::VrfInput) -> Self::VrfPreOutput {
			let pre_output_impl = self.secret.output(input.0);
			VrfPreOutput(pre_output_impl)
		}
	}

	impl VrfCrypto for Public {
		type VrfInput = VrfInput;
		type VrfPreOutput = VrfPreOutput;
		type VrfSignData = VrfSignData;
		type VrfSignature = VrfSignature;
	}

	impl VrfPublic for Public {
		fn vrf_verify(&self, data: &VrfSignData, signature: &VrfSignature) -> bool {
			let io = VrfIo { input: data.vrf_input.0, output: signature.pre_output.0 };
			vrf_verify(self, &[io], &signature.proof, &data.aux_data)
		}
	}

	#[cfg(feature = "full_crypto")]
	impl Pair {
		/// Generate VRF output bytes for the given `input`.
		pub fn make_bytes(&self, input: &VrfInput) -> [u8; 32] {
			self.vrf_pre_output(input).make_bytes()
		}
	}

	impl VrfPreOutput {
		/// Generate VRF output bytes.
		pub fn make_bytes(&self) -> [u8; 32] {
			self.0.hash::<32>()
		}
	}
}

/// Bandersnatch Ring-VRF types and operations.
pub mod ring_vrf {
	use super::{vrf::*, *};
	pub use bandersnatch::{RingProver, RingVerifier};
	use bandersnatch::{RingSetup, RingVerifierKey as RingVerifierKeyImpl};

	// Max size of serialized ring-vrf context given `domain_len`.
	pub(crate) fn ring_context_serialized_size(ring_size: usize) -> usize {
		const G1_POINT_UNCOMPRESSED_SIZE: usize = 96;
		const G2_POINT_UNCOMPRESSED_SIZE: usize = 192;
		const OVERHEAD_SIZE: usize = 16;
		const G2_POINTS_NUM: usize = 2;
		let g1_points_num = ark_vrf::ring::pcs_domain_size::<BandersnatchSuite>(ring_size);
		OVERHEAD_SIZE +
			g1_points_num * G1_POINT_UNCOMPRESSED_SIZE +
			G2_POINTS_NUM * G2_POINT_UNCOMPRESSED_SIZE
	}

	/// [`RingVerifierKey`] serialized size.
	pub const RING_VERIFIER_KEY_SERIALIZED_SIZE: usize = 384;
	/// [`RingProof`] serialized size.
	pub(crate) const RING_PROOF_SERIALIZED_SIZE: usize = 752;
	/// [`RingVrfSignature`] serialized size.
	pub const RING_SIGNATURE_SERIALIZED_SIZE: usize =
		RING_PROOF_SERIALIZED_SIZE + PREOUT_SERIALIZED_SIZE;

	/// Ring verifier key
	pub struct RingVerifierKey(RingVerifierKeyImpl);

	impl Encode for RingVerifierKey {
		fn encode(&self) -> Vec<u8> {
			let mut buf = Vec::with_capacity(RING_VERIFIER_KEY_SERIALIZED_SIZE);
			self.0
				.serialize_compressed(&mut buf)
				.expect("serialization length is constant and checked by test; qed");
			buf
		}
	}

	impl Decode for RingVerifierKey {
		fn decode<R: codec::Input>(input: &mut R) -> Result<Self, codec::Error> {
			let mut buf = vec![0; RING_VERIFIER_KEY_SERIALIZED_SIZE];
			input.read(&mut buf[..])?;
			let vk = RingVerifierKeyImpl::deserialize_compressed_unchecked(buf.as_slice())
				.map_err(|_| "RingVerifierKey decode error")?;
			Ok(RingVerifierKey(vk))
		}
	}

	impl EncodeLike for RingVerifierKey {}

	impl MaxEncodedLen for RingVerifierKey {
		fn max_encoded_len() -> usize {
			RING_VERIFIER_KEY_SERIALIZED_SIZE
		}
	}

	impl TypeInfo for RingVerifierKey {
		type Identity = [u8; RING_VERIFIER_KEY_SERIALIZED_SIZE];
		fn type_info() -> scale_info::Type {
			Self::Identity::type_info()
		}
	}

	/// Context used to construct ring prover and verifier.
	///
	/// Generic parameter `R` represents the ring size.
	#[derive(Clone)]
	pub struct RingContext<const R: usize>(RingSetup);

	impl<const R: usize> RingContext<R> {
		/// Build an dummy instance for testing purposes.
		pub fn new_testing() -> Self {
			Self(RingSetup::from_seed(R, [0; 32]))
		}

		/// Get the keyset max size.
		pub fn max_keyset_size(&self) -> usize {
			self.0.max_ring_size()
		}

		/// Get ring prover for the key at index `public_idx` in the `public_keys` set.
		pub fn prover(&self, public_keys: &[Public], public_idx: usize) -> RingProver {
			let pks = Self::make_ring_vector(public_keys);
			let prover_key = self.0.prover_key(&pks).expect("ring size within bounds; qed");
			self.0.ring_prover(prover_key, public_idx)
		}

		/// Get ring verifier for the `public_keys` set.
		pub fn verifier(&self, public_keys: &[Public]) -> RingVerifier {
			let vk = self.verifier_key(public_keys);
			self.0.ring_verifier(vk.0)
		}

		/// Build `RingVerifierKey` for lazy `RingVerifier` construction.
		pub fn verifier_key(&self, public_keys: &[Public]) -> RingVerifierKey {
			let pks = Self::make_ring_vector(public_keys);
			RingVerifierKey(self.0.verifier_key(&pks).expect("ring size within bounds; qed"))
		}

		/// Constructs a `RingVerifier` from a `VerifierKey` without a `RingContext` instance.
		///
		/// While this approach is computationally slightly less efficient than using a
		/// pre-constructed `RingContext`, as some parameters need to be computed on-the-fly, it
		/// is beneficial in memory or storage constrained environments. This avoids the need to
		/// retain the full `RingContext` for ring signature verification. Instead, the
		/// `VerifierKey` contains only the essential information needed to verify ring proofs.
		pub fn verifier_no_context(verifier_key: RingVerifierKey) -> RingVerifier {
			ark_vrf::ring::RingContext::<BandersnatchSuite>::new(R)
				.into_ring_verifier(verifier_key.0)
		}

		fn make_ring_vector(public_keys: &[Public]) -> Vec<bandersnatch::AffinePoint> {
			use bandersnatch::AffinePoint;
			public_keys
				.iter()
				.map(|pk| {
					AffinePoint::deserialize_compressed_unchecked(pk.as_slice())
						.unwrap_or(RingSetup::padding_point())
				})
				.collect()
		}
	}

	impl<const R: usize> Encode for RingContext<R> {
		fn encode(&self) -> Vec<u8> {
			let mut buf = Vec::with_capacity(ring_context_serialized_size(R));
			self.0
				.serialize_uncompressed(&mut buf)
				.expect("serialization length is constant and checked by test; qed");
			buf
		}
	}

	impl<const R: usize> Decode for RingContext<R> {
		fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
			let mut buf = vec![0; ring_context_serialized_size(R)];
			input.read(&mut buf[..])?;
			let ctx = RingSetup::deserialize_uncompressed_unchecked(buf.as_slice())
				.map_err(|_| "RingContext decode error")?;
			Ok(RingContext(ctx))
		}
	}

	impl<const R: usize> EncodeLike for RingContext<R> {}

	impl<const R: usize> MaxEncodedLen for RingContext<R> {
		fn max_encoded_len() -> usize {
			ring_context_serialized_size(R)
		}
	}

	impl<const R: usize> TypeInfo for RingContext<R> {
		type Identity = Self;
		fn type_info() -> scale_info::Type {
			let path = scale_info::Path::new("RingContext", module_path!());
			let array_type_def = scale_info::TypeDefArray {
				len: ring_context_serialized_size(R) as u32,
				type_param: scale_info::MetaType::new::<u8>(),
			};
			let type_def = scale_info::TypeDef::Array(array_type_def);
			scale_info::Type { path, type_params: Vec::new(), type_def, docs: Vec::new() }
		}
	}

	/// Ring VRF signature.
	#[derive(
		Clone, Debug, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo,
	)]
	pub struct RingVrfSignature {
		/// VRF pre-output.
		pub pre_output: VrfPreOutput,
		/// Ring signature.
		pub proof: [u8; RING_PROOF_SERIALIZED_SIZE],
	}

	#[cfg(feature = "full_crypto")]
	impl Pair {
		/// Produce a ring-vrf signature.
		///
		/// The ring signature is verifiable if the public key corresponding to the
		/// signing [`Pair`] is part of the ring from which the [`RingProver`] has
		/// been constructed. If not, the produced signature is just useless.
		pub fn ring_vrf_sign(&self, data: &VrfSignData, prover: &RingProver) -> RingVrfSignature {
			use ark_vrf::ring::Prover;
			let pre_output_impl = self.secret.output(data.vrf_input.0);
			let pre_output = VrfPreOutput(pre_output_impl);
			let io = VrfIo { input: data.vrf_input.0, output: pre_output.0 };
			let proof_impl = self.secret.prove(io, &data.aux_data, prover);
			let mut proof = [0; RING_PROOF_SERIALIZED_SIZE];
			proof_impl
				.serialize_compressed(proof.as_mut_slice())
				.expect("serialization length is constant and checked by test; qed");
			RingVrfSignature { pre_output, proof }
		}
	}

	impl RingVrfSignature {
		/// Verify a ring-vrf signature.
		///
		/// The signature is verifiable if it has been produced by a member of the ring
		/// from which the [`RingVerifier`] has been constructed.
		pub fn ring_vrf_verify(&self, data: &VrfSignData, verifier: &RingVerifier) -> bool {
			use ark_vrf::ring::Verifier;
			let Ok(proof) = bandersnatch::RingProof::deserialize_compressed(self.proof.as_slice())
			else {
				return false;
			};
			let io = VrfIo { input: data.vrf_input.0, output: self.pre_output.0 };
			bandersnatch::Public::verify(io, &data.aux_data, &proof, verifier).is_ok()
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{ring_vrf::*, vrf::*, *};
	use crate::{
		crypto::{VrfPublic, VrfSecret, DEV_PHRASE},
		proof_of_possession::{ProofOfPossessionGenerator, ProofOfPossessionVerifier},
	};

	const TEST_SEED: &[u8; SEED_SERIALIZED_SIZE] = &[0xcb; SEED_SERIALIZED_SIZE];
	const TEST_RING_SIZE: usize = 16;

	type TestRingContext = RingContext<TEST_RING_SIZE>;

	#[allow(unused)]
	fn b2h(bytes: &[u8]) -> String {
		array_bytes::bytes2hex("", bytes)
	}

	fn h2b(hex: &str) -> Vec<u8> {
		array_bytes::hex2bytes_unchecked(hex)
	}

	#[test]
	fn backend_assumptions_sanity_check() {
		use bandersnatch::{Input, RingSetup};

		let ctx = RingSetup::from_seed(TEST_RING_SIZE, [0_u8; 32]);

		let domain_size = ark_vrf::ring::pcs_domain_size::<BandersnatchSuite>(TEST_RING_SIZE);
		assert_eq!(domain_size, ctx.pcs_params.powers_in_g1.len());
		let domain_size2 = ark_vrf::ring::pcs_domain_size::<BandersnatchSuite>(ctx.max_ring_size());
		assert_eq!(domain_size, domain_size2);
		assert_eq!(
			ark_vrf::ring::max_ring_size_from_pcs_domain_size::<BandersnatchSuite>(domain_size),
			ctx.max_ring_size()
		);

		assert_eq!(ctx.uncompressed_size(), ring_context_serialized_size(TEST_RING_SIZE));

		let prover_key_index = 3;
		let secret = Secret::from_seed([prover_key_index as u8; 32]);
		let public = secret.public();
		assert_eq!(public.compressed_size(), PUBLIC_SERIALIZED_SIZE);

		let input = Input::new(b"foo").unwrap();
		let preout = secret.output(input);
		assert_eq!(preout.compressed_size(), PREOUT_SERIALIZED_SIZE);

		let ring_keys: Vec<_> = (0..TEST_RING_SIZE)
			.map(|i| Secret::from_seed([i as u8; 32]).public().0.into())
			.collect();

		let verifier_key = ctx.verifier_key(&ring_keys[..]).expect("valid ring; qed");
		assert_eq!(verifier_key.compressed_size(), RING_VERIFIER_KEY_SERIALIZED_SIZE);

		let prover_key = ctx.prover_key(&ring_keys).expect("valid ring; qed");
		let ring_prover = ctx.ring_prover(prover_key, prover_key_index);

		{
			use ark_vrf::thin::Prover;
			let io = ark_vrf::VrfIo::<BandersnatchSuite> { input, output: preout };
			let proof = secret.prove(io, &[]);
			assert_eq!(proof.compressed_size(), SIGNATURE_SERIALIZED_SIZE);
		}

		{
			use ark_vrf::ring::Prover;
			let io = ark_vrf::VrfIo::<BandersnatchSuite> { input, output: preout };
			let proof = secret.prove(io, &[], &ring_prover);
			assert_eq!(proof.compressed_size(), RING_PROOF_SERIALIZED_SIZE);
		}
	}

	#[test]
	fn derive_works() {
		let pair = Pair::from_string(&format!("{}//Alice//Hard", DEV_PHRASE), None).unwrap();
		let known = h2b("915626c7d4363856277e3dc0729c11df1f5a1caa1d3d0ec4986f0ae9226c5c45");
		assert_eq!(pair.public().as_ref(), known);

		// Soft derivation not supported
		let res = Pair::from_string(&format!("{}//Alice/Soft", DEV_PHRASE), None);
		assert!(res.is_err());
	}

	#[test]
	fn generate_with_phrase_should_be_recoverable_with_from_string() {
		let (pair, phrase, seed) = Pair::generate_with_phrase(None);
		let repair_seed = Pair::from_seed_slice(seed.as_ref()).expect("seed slice is valid");
		assert_eq!(pair.public(), repair_seed.public());
		let (repair_phrase, reseed) =
			Pair::from_phrase(phrase.as_ref(), None).expect("seed slice is valid");
		assert_eq!(seed, reseed);
		assert_eq!(pair.public(), repair_phrase.public());
		let repair_string = Pair::from_string(phrase.as_str(), None).expect("seed slice is valid");
		assert_eq!(pair.public(), repair_string.public());
	}

	#[test]
	fn sign_verify() {
		let pair = Pair::from_seed(TEST_SEED);
		let public = pair.public();
		let msg = b"foo";
		let signature = pair.sign(msg);
		assert!(Pair::verify(&signature, msg, &public));
	}

	#[test]
	fn vrf_sign_verify() {
		let pair = Pair::from_seed(TEST_SEED);
		let public = pair.public();
		let data = VrfSignData::new(b"foo", b"aux");
		let signature = pair.vrf_sign(&data);
		assert!(public.vrf_verify(&data, &signature));
	}

	#[test]
	fn vrf_sign_verify_with_bad_input() {
		let pair = Pair::from_seed(TEST_SEED);
		let public = pair.public();
		let data = VrfSignData::new(b"foo", b"aux");
		let signature = pair.vrf_sign(&data);
		let data = VrfSignData::new(b"foo", b"bad");
		assert!(!public.vrf_verify(&data, &signature));
		let data = VrfSignData::new(b"bar", b"aux");
		assert!(!public.vrf_verify(&data, &signature));
	}

	#[test]
	fn vrf_output_bytes_match() {
		let pair = Pair::from_seed(TEST_SEED);
		let data = VrfSignData::new(b"foo", b"aux");
		let signature = pair.vrf_sign(&data);
		let o0 = pair.make_bytes(&data.vrf_input);
		let o1 = signature.pre_output.make_bytes();
		assert_eq!(o0, o1);
	}

	#[test]
	fn vrf_signature_encode_decode() {
		let pair = Pair::from_seed(TEST_SEED);

		let data = VrfSignData::new(b"data", b"aux");
		let expected = pair.vrf_sign(&data);

		let bytes = expected.encode();

		let expected_len = PREOUT_SERIALIZED_SIZE + SIGNATURE_SERIALIZED_SIZE;
		assert_eq!(bytes.len(), expected_len);

		let decoded = VrfSignature::decode(&mut bytes.as_slice()).unwrap();
		assert_eq!(expected, decoded);
	}

	#[test]
	fn ring_vrf_sign_verify() {
		let ring_ctx = TestRingContext::new_testing();

		let mut pks: Vec<_> =
			(0..TEST_RING_SIZE).map(|i| Pair::from_seed(&[i as u8; 32]).public()).collect();
		assert!(pks.len() <= ring_ctx.max_keyset_size());

		let pair = Pair::from_seed(TEST_SEED);

		// Just pick one index to patch with the actual public key
		let prover_idx = 3;
		pks[prover_idx] = pair.public();
		let prover = ring_ctx.prover(&pks, prover_idx);

		let data = VrfSignData::new(b"data", b"aux");
		let signature = pair.ring_vrf_sign(&data, &prover);

		let verifier = ring_ctx.verifier(&pks);
		assert!(signature.ring_vrf_verify(&data, &verifier));
	}

	#[test]
	fn ring_vrf_sign_verify_with_out_of_ring_key() {
		let ring_ctx = TestRingContext::new_testing();

		let pks: Vec<_> =
			(0..TEST_RING_SIZE).map(|i| Pair::from_seed(&[i as u8; 32]).public()).collect();
		let pair = Pair::from_seed(TEST_SEED);

		let data = VrfSignData::new(b"foo", b"aux");

		// pair.public != pks[0]
		let prover = ring_ctx.prover(&pks, 0);
		let signature = pair.ring_vrf_sign(&data, &prover);

		let verifier = ring_ctx.verifier(&pks);
		assert!(!signature.ring_vrf_verify(&data, &verifier));
	}

	#[test]
	fn ring_vrf_make_bytes_matches() {
		let ring_ctx = TestRingContext::new_testing();

		let mut pks: Vec<_> =
			(0..TEST_RING_SIZE).map(|i| Pair::from_seed(&[i as u8; 32]).public()).collect();
		assert!(pks.len() <= ring_ctx.max_keyset_size());

		let pair = Pair::from_seed(TEST_SEED);

		// Just pick one index to patch with the actual public key
		let prover_idx = 3;
		pks[prover_idx] = pair.public();

		let data = VrfSignData::new(b"data", b"aux");

		let prover = ring_ctx.prover(&pks, prover_idx);
		let signature = pair.ring_vrf_sign(&data, &prover);

		let o0 = pair.make_bytes(&data.vrf_input);
		let o1 = signature.pre_output.make_bytes();
		assert_eq!(o0, o1);
	}

	#[test]
	fn ring_vrf_signature_encode_decode() {
		let ring_ctx = TestRingContext::new_testing();

		let mut pks: Vec<_> =
			(0..TEST_RING_SIZE).map(|i| Pair::from_seed(&[i as u8; 32]).public()).collect();
		assert!(pks.len() <= ring_ctx.max_keyset_size());

		let pair = Pair::from_seed(TEST_SEED);

		// Just pick one index to patch with the actual public key
		let prover_idx = 3;
		pks[prover_idx] = pair.public();

		let data = VrfSignData::new(b"foo", b"aux");

		let prover = ring_ctx.prover(&pks, prover_idx);
		let expected = pair.ring_vrf_sign(&data, &prover);

		let bytes = expected.encode();
		assert_eq!(bytes.len(), RING_SIGNATURE_SERIALIZED_SIZE);

		let decoded = RingVrfSignature::decode(&mut bytes.as_slice()).unwrap();
		assert_eq!(expected, decoded);
	}

	#[test]
	fn ring_vrf_context_encode_decode() {
		let ctx1 = TestRingContext::new_testing();
		let enc1 = ctx1.encode();

		assert_eq!(enc1.len(), ring_context_serialized_size(TEST_RING_SIZE));
		assert_eq!(enc1.len(), TestRingContext::max_encoded_len());

		let ctx2 = TestRingContext::decode(&mut enc1.as_slice()).unwrap();
		let enc2 = ctx2.encode();

		assert_eq!(enc1, enc2);
	}

	#[test]
	fn verifier_key_encode_decode() {
		let ring_ctx = TestRingContext::new_testing();

		let pks: Vec<_> =
			(0..TEST_RING_SIZE).map(|i| Pair::from_seed(&[i as u8; 32]).public()).collect();
		assert!(pks.len() <= ring_ctx.max_keyset_size());

		let verifier_key = ring_ctx.verifier_key(&pks);
		let enc1 = verifier_key.encode();
		assert_eq!(enc1.len(), RING_VERIFIER_KEY_SERIALIZED_SIZE);
		assert_eq!(RingVerifierKey::max_encoded_len(), RING_VERIFIER_KEY_SERIALIZED_SIZE);

		let vd2 = RingVerifierKey::decode(&mut enc1.as_slice()).unwrap();
		let enc2 = vd2.encode();
		assert_eq!(enc1, enc2);
	}

	#[test]
	fn good_proof_of_possession_should_work_bad_proof_of_possession_should_fail() {
		let owner = b"owner";
		let not_owner = b"not owner";
		let mut pair = Pair::from_seed(b"12345678901234567890123456789012");
		let other_pair = Pair::from_seed(b"23456789012345678901234567890123");
		let proof_of_possession = pair.generate_proof_of_possession(owner);
		assert!(Pair::verify_proof_of_possession(owner, &proof_of_possession, &pair.public()));
		assert!(!Pair::verify_proof_of_possession(
			owner,
			&proof_of_possession,
			&other_pair.public()
		));
		assert!(!Pair::verify_proof_of_possession(not_owner, &proof_of_possession, &pair.public()));
	}
}
