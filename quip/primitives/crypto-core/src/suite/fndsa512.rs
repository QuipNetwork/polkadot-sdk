//! Shared fixed-buffer adapter for pqhybridsign's delta-encoded H2/H4 suites.

use core::marker::PhantomData;

use fn_dsa::{SigningKey, SigningKey512, VerifyingKey, VerifyingKey512};
use pqhybridsign::{composite_delta, suite::DeltaSuite};
use rand_core::CryptoRngCore;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{HybridSignatureError, Result};

/// Classical public-key length shared by H2 and H4.
const CLASSICAL_PK_LEN: usize = 32;
/// Classical secret-key length shared by H2 and H4.
const CLASSICAL_SK_LEN: usize = 64;
/// Offset of the one-byte FN-DSA delta-length field.
const DELTA_OFFSET: usize = 64;
/// Fixed buffer size used by the Substrate-facing wrapper.
pub const HYBRID_PK_LEN: usize = 929;
/// Serialized secret-key size used by H2 and H4.
pub const HYBRID_SK_LEN: usize = 1409;
/// Maximum H2/H4 wire signature size.
pub const HYBRID_SIG_LEN: usize = 731;
/// Smallest complete delta-encoded H2/H4 signature.
const MIN_HYBRID_SIG_LEN: usize = 64 + 1 + pqhybridsign::MIN_FALCON512_SIG_LEN;

/// Suite-specific operations needed by the common H2/H4 adapter.
pub trait Config {
	/// Matching suite from pqhybridsign.
	type LibrarySuite: DeltaSuite;

	/// Validates the classical public-key half.
	fn classical_public_is_valid(bytes: &[u8]) -> bool;

	/// Derives the classical public-key half from its serialized secret key.
	fn classical_public_from_secret(bytes: &[u8]) -> Option<[u8; CLASSICAL_PK_LEN]>;
}

/// Fixed-size composite public key.
pub struct PublicKey<C> {
	bytes: [u8; HYBRID_PK_LEN],
	marker: PhantomData<fn() -> C>,
}

impl<C> Clone for PublicKey<C> {
	fn clone(&self) -> Self {
		Self::from_array(self.bytes)
	}
}

impl<C> PublicKey<C> {
	fn from_array(bytes: [u8; HYBRID_PK_LEN]) -> Self {
		Self { bytes, marker: PhantomData }
	}

	/// Serializes the public key.
	pub fn to_bytes(&self) -> [u8; HYBRID_PK_LEN] {
		self.bytes
	}
}

impl<C: Config> PublicKey<C> {
	/// Parses and validates a serialized H2/H4 public key.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() != HYBRID_PK_LEN {
			return Err(HybridSignatureError::InvalidLength {
				expected: HYBRID_PK_LEN,
				actual: bytes.len(),
			});
		}
		if !C::classical_public_is_valid(&bytes[..CLASSICAL_PK_LEN])
			|| VerifyingKey512::decode(&bytes[CLASSICAL_PK_LEN..]).is_none()
		{
			return Err(HybridSignatureError::InvalidPublicKey);
		}

		let mut out = [0u8; HYBRID_PK_LEN];
		out.copy_from_slice(bytes);
		Ok(Self::from_array(out))
	}
}

impl<C> AsRef<[u8]> for PublicKey<C> {
	fn as_ref(&self) -> &[u8] {
		&self.bytes
	}
}

/// Composite secret key, wiped on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretKey<C> {
	bytes: [u8; HYBRID_SK_LEN],
	#[zeroize(skip)]
	marker: PhantomData<fn() -> C>,
}

impl<C> SecretKey<C> {
	fn from_array(bytes: [u8; HYBRID_SK_LEN]) -> Self {
		Self { bytes, marker: PhantomData }
	}

	/// Serializes the secret key into a zeroizing buffer.
	pub fn to_bytes(&self) -> Zeroizing<[u8; HYBRID_SK_LEN]> {
		Zeroizing::new(self.bytes)
	}
}

impl<C: Config> SecretKey<C> {
	/// Parses and validates a serialized H2/H4 secret key.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() != HYBRID_SK_LEN {
			return Err(HybridSignatureError::InvalidLength {
				expected: HYBRID_SK_LEN,
				actual: bytes.len(),
			});
		}

		let mut out = [0u8; HYBRID_SK_LEN];
		out.copy_from_slice(bytes);
		if public_from_secret::<C>(&out).is_none() {
			out.zeroize();
			return Err(HybridSignatureError::InvalidSecretKey);
		}
		Ok(Self::from_array(out))
	}
}

/// Fixed-size, zero-padded delta-encoded signature.
pub struct Signature<C> {
	bytes: [u8; HYBRID_SIG_LEN],
	marker: PhantomData<fn() -> C>,
}

impl<C> Signature<C> {
	fn from_array(bytes: [u8; HYBRID_SIG_LEN]) -> Self {
		Self { bytes, marker: PhantomData }
	}

	/// Parses a fixed-size signature and enforces canonical zero padding.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() != HYBRID_SIG_LEN {
			return Err(HybridSignatureError::InvalidLength {
				expected: HYBRID_SIG_LEN,
				actual: bytes.len(),
			});
		}

		let real_len = signature_real_len(bytes);
		if bytes[real_len..].iter().any(|byte| *byte != 0) {
			let actual = bytes
				.iter()
				.rposition(|byte| *byte != 0)
				.map_or(real_len, |position| position + 1);
			return Err(HybridSignatureError::InvalidLength { expected: real_len, actual });
		}

		let mut out = [0u8; HYBRID_SIG_LEN];
		out.copy_from_slice(bytes);
		Ok(Self::from_array(out))
	}

	/// Returns the unpadded wire length encoded by the delta byte.
	pub fn real_len(&self) -> usize {
		signature_real_len(&self.bytes)
	}

	/// Returns the exact delta-encoded wire signature without fixed-buffer padding.
	pub fn wire_bytes(&self) -> &[u8] {
		&self.bytes[..self.real_len()]
	}

	/// Serializes the fixed-size padded signature.
	pub fn to_bytes(&self) -> [u8; HYBRID_SIG_LEN] {
		self.bytes
	}
}

impl<C> AsRef<[u8]> for Signature<C> {
	fn as_ref(&self) -> &[u8] {
		&self.bytes
	}
}

fn signature_real_len(bytes: &[u8]) -> usize {
	MIN_HYBRID_SIG_LEN + usize::from(bytes[DELTA_OFFSET])
}

/// Generates a keypair through the library's deterministic seed pipeline.
pub fn generate<C: Config>(rng: &mut impl CryptoRngCore) -> (SecretKey<C>, PublicKey<C>) {
	let mut seed = [0u8; 32];
	rng.fill_bytes(&mut seed);
	let pair = from_seed_slice::<C>(&seed)
		.expect("32-byte seed and exact H2/H4 output buffers cannot fail");
	seed.zeroize();
	pair
}

/// Derives a keypair through the matching pqhybridsign suite.
pub fn from_seed_slice<C: Config>(seed: &[u8]) -> Result<(SecretKey<C>, PublicKey<C>)> {
	let seed: &[u8; 32] = seed.try_into().map_err(|_| HybridSignatureError::InvalidSeedLength {
		expected: 32,
		actual: seed.len(),
	})?;
	let mut secret = [0u8; HYBRID_SK_LEN];
	let mut public = [0u8; HYBRID_PK_LEN];
	composite_delta::keypair_from_seed::<C::LibrarySuite>(seed, &mut secret, &mut public)
		.map_err(|_| HybridSignatureError::InvalidSecretKey)?;
	Ok((SecretKey::from_array(secret), PublicKey::from_array(public)))
}

/// Derives the composite public key from a validated secret key.
pub fn public<C: Config>(secret: &SecretKey<C>) -> PublicKey<C> {
	let bytes = public_from_secret::<C>(&secret.bytes)
		.expect("SecretKey is validated on construction; qed");
	PublicKey::from_array(bytes)
}

fn public_from_secret<C: Config>(secret: &[u8; HYBRID_SK_LEN]) -> Option<[u8; HYBRID_PK_LEN]> {
	let classical = C::classical_public_from_secret(&secret[..CLASSICAL_SK_LEN])?;
	let signing = SigningKey512::decode(&secret[CLASSICAL_SK_LEN..])?;
	let mut public = [0u8; HYBRID_PK_LEN];
	public[..CLASSICAL_PK_LEN].copy_from_slice(&classical);
	signing.to_verifying_key(&mut public[CLASSICAL_PK_LEN..]);
	Some(public)
}

/// Produces a hedged fixed-buffer signature.
pub fn sign<C: Config>(
	secret: &SecretKey<C>,
	msg: &[u8],
	ctx: &[u8],
	rng: &mut impl CryptoRngCore,
) -> Signature<C> {
	let mut bytes = [0u8; HYBRID_SIG_LEN];
	let written =
		composite_delta::sign::<C::LibrarySuite>(&secret.bytes, msg, ctx, rng, &mut bytes)
			.expect("validated key, supported context, and maximum signature buffer cannot fail");
	debug_assert!(written <= HYBRID_SIG_LEN);
	Signature::from_array(bytes)
}

/// Produces a nonce-derived deterministic fixed-buffer signature.
pub fn sign_deterministic<C: Config>(
	secret: &SecretKey<C>,
	msg: &[u8],
	ctx: &[u8],
	nonce: &[u8],
) -> Signature<C> {
	let mut bytes = [0u8; HYBRID_SIG_LEN];
	let written = composite_delta::sign_deterministic::<C::LibrarySuite>(
		&secret.bytes,
		msg,
		ctx,
		nonce,
		&mut bytes,
	)
	.expect("validated key, supported context, and maximum signature buffer cannot fail");
	debug_assert!(written <= HYBRID_SIG_LEN);
	Signature::from_array(bytes)
}

/// Verifies the exact unpadded wire signature through pqhybridsign.
pub fn verify<C: Config>(
	public: &PublicKey<C>,
	msg: &[u8],
	ctx: &[u8],
	signature: &Signature<C>,
) -> bool {
	composite_delta::verify::<C::LibrarySuite>(&public.bytes, msg, ctx, signature.wire_bytes())
}
