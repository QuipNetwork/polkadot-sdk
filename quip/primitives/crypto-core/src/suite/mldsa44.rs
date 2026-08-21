//! Shared fixed-buffer adapter for pqhybridsign's H1/H3 composite suites.

use core::marker::PhantomData;

use fips204::{
	ml_dsa_44,
	traits::{SerDes, Signer},
};
use pqhybridsign::{composite, suite::Suite};
use rand_core::CryptoRngCore;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{HybridSignatureError, Result};

/// Classical public-key length shared by H1 and H3.
const CLASSICAL_PK_LEN: usize = 32;
/// Classical secret-key length shared by H1 and H3.
const CLASSICAL_SK_LEN: usize = 64;
/// ML-DSA-44 public-key length.
const ML_DSA_PK_LEN: usize = 1312;
/// ML-DSA-44 secret-key length.
const ML_DSA_SK_LEN: usize = 2560;
/// Fixed H1/H3 public-key length.
pub const HYBRID_PK_LEN: usize = CLASSICAL_PK_LEN + ML_DSA_PK_LEN;
/// Fixed H1/H3 secret-key length.
pub const HYBRID_SK_LEN: usize = CLASSICAL_SK_LEN + ML_DSA_SK_LEN;
/// Fixed H1/H3 signature length.
pub const HYBRID_SIG_LEN: usize = 64 + 2420;

/// Suite-specific operations needed by the common H1/H3 adapter.
pub trait Config {
	/// Matching suite from pqhybridsign.
	type LibrarySuite: Suite;

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
	/// Parses and validates a serialized H1/H3 public key.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() != HYBRID_PK_LEN {
			return Err(HybridSignatureError::InvalidLength {
				expected: HYBRID_PK_LEN,
				actual: bytes.len(),
			});
		}

		let pq_bytes: [u8; ML_DSA_PK_LEN] =
			bytes[CLASSICAL_PK_LEN..].try_into().expect("total length checked; qed");
		if !C::classical_public_is_valid(&bytes[..CLASSICAL_PK_LEN])
			|| ml_dsa_44::PublicKey::try_from_bytes(pq_bytes).is_err()
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

	/// Splits the serialized classical and ML-DSA-44 components.
	///
	/// This is exposed for the legacy H3 VRF adapter, which signs its binding
	/// proof with the ML-DSA-44 component rather than a full H3 signature.
	#[doc(hidden)]
	pub fn split_components(&self) -> (&[u8], &[u8]) {
		self.bytes.split_at(CLASSICAL_SK_LEN)
	}
}

impl<C: Config> SecretKey<C> {
	/// Parses and validates a serialized H1/H3 secret key.
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

/// Fixed-size composite signature.
pub struct Signature<C> {
	bytes: [u8; HYBRID_SIG_LEN],
	marker: PhantomData<fn() -> C>,
}

impl<C> Clone for Signature<C> {
	fn clone(&self) -> Self {
		Self::from_array(self.bytes)
	}
}

impl<C> Signature<C> {
	fn from_array(bytes: [u8; HYBRID_SIG_LEN]) -> Self {
		Self { bytes, marker: PhantomData }
	}

	/// Parses a fixed-size H1/H3 signature.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() != HYBRID_SIG_LEN {
			return Err(HybridSignatureError::InvalidLength {
				expected: HYBRID_SIG_LEN,
				actual: bytes.len(),
			});
		}

		let mut out = [0u8; HYBRID_SIG_LEN];
		out.copy_from_slice(bytes);
		Ok(Self::from_array(out))
	}

	/// Serializes the fixed-size signature.
	pub fn to_bytes(&self) -> [u8; HYBRID_SIG_LEN] {
		self.bytes
	}
}

impl<C> AsRef<[u8]> for Signature<C> {
	fn as_ref(&self) -> &[u8] {
		&self.bytes
	}
}

/// Generates a keypair through the library's suite-separated seed pipeline.
pub fn generate<C: Config>(rng: &mut impl CryptoRngCore) -> (SecretKey<C>, PublicKey<C>) {
	let mut seed = [0u8; crate::MASTER_SEED_LEN];
	rng.fill_bytes(&mut seed);
	let pair = from_seed_slice::<C>(&seed)
		.expect("32-byte seed and exact H1/H3 output buffers cannot fail");
	seed.zeroize();
	pair
}

/// Derives a keypair through the matching pqhybridsign suite.
pub fn from_seed_slice<C: Config>(seed: &[u8]) -> Result<(SecretKey<C>, PublicKey<C>)> {
	let seed: &[u8; crate::MASTER_SEED_LEN] =
		seed.try_into().map_err(|_| HybridSignatureError::InvalidSeedLength {
			expected: crate::MASTER_SEED_LEN,
			actual: seed.len(),
		})?;
	let mut secret = [0u8; HYBRID_SK_LEN];
	let mut public = [0u8; HYBRID_PK_LEN];
	composite::keypair_from_seed::<C::LibrarySuite>(seed, &mut secret, &mut public)
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
	let pq_secret: [u8; ML_DSA_SK_LEN] = secret[CLASSICAL_SK_LEN..].try_into().ok()?;
	let pq_public = ml_dsa_44::PrivateKey::try_from_bytes(pq_secret)
		.ok()?
		.get_public_key()
		.into_bytes();

	let mut public = [0u8; HYBRID_PK_LEN];
	public[..CLASSICAL_PK_LEN].copy_from_slice(&classical);
	public[CLASSICAL_PK_LEN..].copy_from_slice(&pq_public);
	Some(public)
}

/// Produces a hedged fixed-buffer signature through pqhybridsign.
pub fn sign<C: Config>(
	secret: &SecretKey<C>,
	msg: &[u8],
	ctx: &[u8],
	rng: &mut impl CryptoRngCore,
) -> Signature<C> {
	let mut bytes = [0u8; HYBRID_SIG_LEN];
	composite::sign::<C::LibrarySuite>(&secret.bytes, msg, ctx, rng, &mut bytes)
		.expect("validated key, supported context, and exact signature buffer cannot fail");
	Signature::from_array(bytes)
}

/// Produces a deterministic fixed-buffer signature through pqhybridsign.
pub fn sign_deterministic<C: Config>(
	secret: &SecretKey<C>,
	msg: &[u8],
	ctx: &[u8],
	nonce: &[u8],
) -> Signature<C> {
	let mut bytes = [0u8; HYBRID_SIG_LEN];
	composite::sign_deterministic::<C::LibrarySuite>(&secret.bytes, msg, ctx, nonce, &mut bytes)
		.expect("validated key, supported context, and exact signature buffer cannot fail");
	Signature::from_array(bytes)
}

/// Verifies a fixed-size signature through pqhybridsign.
pub fn verify<C: Config>(
	public: &PublicKey<C>,
	msg: &[u8],
	ctx: &[u8],
	signature: &Signature<C>,
) -> bool {
	composite::verify::<C::LibrarySuite>(&public.bytes, msg, ctx, &signature.bytes)
}
