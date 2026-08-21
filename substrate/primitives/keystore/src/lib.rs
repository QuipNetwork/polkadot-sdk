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

//! Keystore traits

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "std")]
pub mod testing;

#[cfg(feature = "bandersnatch-experimental")]
use sp_core::bandersnatch;
#[cfg(feature = "bls-experimental")]
use sp_core::{bls381, ecdsa_bls381};
use sp_core::{
	crypto::{ByteArray, CryptoTypeId, KeyTypeId},
	ecdsa, ed25519, sr25519,
};

use alloc::{string::String, sync::Arc, vec::Vec};
use codec::{Decode, Encode};

/// Keystore error
#[derive(Debug)]
pub enum Error {
	/// Public key type is not supported
	KeyNotSupported(KeyTypeId),
	/// Validation error
	ValidationError(String),
	/// Keystore unavailable
	Unavailable,
	/// Programming errors
	Other(String),
}

/// BABE transcript components needed to derive a scheme-specific VRF signing
/// payload.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct BabeVrfSignData {
	/// Chain randomness for the epoch.
	pub randomness: [u8; 32],
	/// Slot number being claimed.
	pub slot: u64,
	/// Epoch index associated with the slot claim.
	pub epoch: u64,
}

impl core::fmt::Display for Error {
	fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
		match self {
			Error::KeyNotSupported(key_type) => write!(fmt, "Key not supported: {key_type:?}"),
			Error::ValidationError(error) => write!(fmt, "Validation error: {error}"),
			Error::Unavailable => fmt.write_str("Keystore unavailable"),
			Error::Other(error) => write!(fmt, "An unknown keystore error occurred: {error}"),
		}
	}
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

/// Shared implementation of [`Keystore::public_keys_with`] for the built-in
/// Substrate crypto schemes.
pub fn public_keys_with_default<K: Keystore + ?Sized>(
	keystore: &K,
	id: KeyTypeId,
	crypto_id: CryptoTypeId,
) -> Result<Vec<Vec<u8>>, Error> {
	let public_keys = match crypto_id {
		sr25519::CRYPTO_ID => keystore
			.sr25519_public_keys(id)
			.into_iter()
			.map(|public| public.to_raw_vec())
			.collect(),
		ed25519::CRYPTO_ID => keystore
			.ed25519_public_keys(id)
			.into_iter()
			.map(|public| public.to_raw_vec())
			.collect(),
		ecdsa::CRYPTO_ID => keystore
			.ecdsa_public_keys(id)
			.into_iter()
			.map(|public| public.to_raw_vec())
			.collect(),
		#[cfg(feature = "bandersnatch-experimental")]
		bandersnatch::CRYPTO_ID => keystore
			.bandersnatch_public_keys(id)
			.into_iter()
			.map(|public| public.to_raw_vec())
			.collect(),
		#[cfg(feature = "bls-experimental")]
		bls381::CRYPTO_ID => keystore
			.bls381_public_keys(id)
			.into_iter()
			.map(|public| public.to_raw_vec())
			.collect(),
		#[cfg(feature = "bls-experimental")]
		ecdsa_bls381::CRYPTO_ID => keystore
			.ecdsa_bls381_public_keys(id)
			.into_iter()
			.map(|public| public.to_raw_vec())
			.collect(),
		_ => return Err(Error::KeyNotSupported(id)),
	};

	Ok(public_keys)
}

/// Shared implementation of [`Keystore::generate_new_with`] for the built-in
/// Substrate crypto schemes.
pub fn generate_new_with_default<K: Keystore + ?Sized>(
	keystore: &K,
	id: KeyTypeId,
	crypto_id: CryptoTypeId,
	seed: Option<&str>,
) -> Result<Vec<u8>, Error> {
	let public = match crypto_id {
		sr25519::CRYPTO_ID => keystore.sr25519_generate_new(id, seed)?.to_raw_vec(),
		ed25519::CRYPTO_ID => keystore.ed25519_generate_new(id, seed)?.to_raw_vec(),
		ecdsa::CRYPTO_ID => keystore.ecdsa_generate_new(id, seed)?.to_raw_vec(),
		#[cfg(feature = "bandersnatch-experimental")]
		bandersnatch::CRYPTO_ID => keystore.bandersnatch_generate_new(id, seed)?.to_raw_vec(),
		#[cfg(feature = "bls-experimental")]
		bls381::CRYPTO_ID => keystore.bls381_generate_new(id, seed)?.to_raw_vec(),
		#[cfg(feature = "bls-experimental")]
		ecdsa_bls381::CRYPTO_ID => keystore.ecdsa_bls381_generate_new(id, seed)?.to_raw_vec(),
		_ => return Err(Error::KeyNotSupported(id)),
	};

	Ok(public)
}

/// Shared implementation of [`Keystore::sign_with`] for the built-in
/// Substrate crypto schemes.
pub fn sign_with_default<K: Keystore + ?Sized>(
	keystore: &K,
	id: KeyTypeId,
	crypto_id: CryptoTypeId,
	public: &[u8],
	msg: &[u8],
) -> Result<Option<Vec<u8>>, Error> {
	let signature = match crypto_id {
		sr25519::CRYPTO_ID => {
			let public = sr25519::Public::from_slice(public)
				.map_err(|_| Error::ValidationError("Invalid public key format".into()))?;
			keystore.sr25519_sign(id, &public, msg)?.map(|s| s.encode())
		},
		ed25519::CRYPTO_ID => {
			let public = ed25519::Public::from_slice(public)
				.map_err(|_| Error::ValidationError("Invalid public key format".into()))?;
			keystore.ed25519_sign(id, &public, msg)?.map(|s| s.encode())
		},
		ecdsa::CRYPTO_ID => {
			let public = ecdsa::Public::from_slice(public)
				.map_err(|_| Error::ValidationError("Invalid public key format".into()))?;
			keystore.ecdsa_sign(id, &public, msg)?.map(|s| s.encode())
		},
		#[cfg(feature = "bandersnatch-experimental")]
		bandersnatch::CRYPTO_ID => {
			let public = bandersnatch::Public::from_slice(public)
				.map_err(|_| Error::ValidationError("Invalid public key format".into()))?;
			keystore.bandersnatch_sign(id, &public, msg)?.map(|s| s.encode())
		},
		#[cfg(feature = "bls-experimental")]
		bls381::CRYPTO_ID => {
			let public = bls381::Public::from_slice(public)
				.map_err(|_| Error::ValidationError("Invalid public key format".into()))?;
			keystore.bls381_sign(id, &public, msg)?.map(|s| s.encode())
		},
		#[cfg(feature = "bls-experimental")]
		ecdsa_bls381::CRYPTO_ID => {
			let public = ecdsa_bls381::Public::from_slice(public)
				.map_err(|_| Error::ValidationError("Invalid public key format".into()))?;
			keystore.ecdsa_bls381_sign(id, &public, msg)?.map(|s| s.encode())
		},
		_ => return Err(Error::KeyNotSupported(id)),
	};

	Ok(signature)
}

/// Something that generates, stores and provides access to secret keys.
pub trait Keystore: Send + Sync {
	/// Returns all the sr25519 public keys for the given key type.
	fn sr25519_public_keys(&self, key_type: KeyTypeId) -> Vec<sr25519::Public>;

	/// Generate a new sr25519 key pair for the given key type and an optional seed.
	///
	/// Returns an `sr25519::Public` key of the generated key pair or an `Err` if
	/// something failed during key generation.
	fn sr25519_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<sr25519::Public, Error>;

	/// Generate an sr25519 signature for a given message.
	///
	/// Receives [`KeyTypeId`] and an [`sr25519::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`sr25519::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	fn sr25519_sign(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		msg: &[u8],
	) -> Result<Option<sr25519::Signature>, Error>;

	/// Generate an sr25519 VRF signature for the given data.
	///
	/// Receives [`KeyTypeId`] and an [`sr25519::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns `None` if the given `key_type` and `public` combination doesn't
	/// exist in the keystore or an `Err` when something failed.
	fn sr25519_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		data: &sr25519::vrf::VrfSignData,
	) -> Result<Option<sr25519::vrf::VrfSignature>, Error>;

	/// Generate an sr25519 VRF pre-output for a given input data.
	///
	/// Receives [`KeyTypeId`] and an [`sr25519::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns `None` if the given `key_type` and `public` combination doesn't
	/// exist in the keystore or an `Err` when something failed.
	fn sr25519_vrf_pre_output(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		input: &sr25519::vrf::VrfInput,
	) -> Result<Option<sr25519::vrf::VrfPreOutput>, Error>;

	/// Returns all ed25519 public keys for the given key type.
	fn ed25519_public_keys(&self, key_type: KeyTypeId) -> Vec<ed25519::Public>;

	/// Generate a new ed25519 key pair for the given key type and an optional seed.
	///
	/// Returns an `ed25519::Public` key of the generated key pair or an `Err` if
	/// something failed during key generation.
	fn ed25519_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<ed25519::Public, Error>;

	/// Generate an ed25519 signature for a given message.
	///
	/// Receives [`KeyTypeId`] and an [`ed25519::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`ed25519::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	fn ed25519_sign(
		&self,
		key_type: KeyTypeId,
		public: &ed25519::Public,
		msg: &[u8],
	) -> Result<Option<ed25519::Signature>, Error>;

	/// Returns all ecdsa public keys for the given key type.
	fn ecdsa_public_keys(&self, key_type: KeyTypeId) -> Vec<ecdsa::Public>;

	/// Generate a new ecdsa key pair for the given key type and an optional seed.
	///
	/// Returns an `ecdsa::Public` key of the generated key pair or an `Err` if
	/// something failed during key generation.
	fn ecdsa_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<ecdsa::Public, Error>;

	/// Generate an ecdsa signature for a given message.
	///
	/// Receives [`KeyTypeId`] and an [`ecdsa::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`ecdsa::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	fn ecdsa_sign(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa::Public,
		msg: &[u8],
	) -> Result<Option<ecdsa::Signature>, Error>;

	/// Generate an ecdsa signature for a given pre-hashed message.
	///
	/// Receives [`KeyTypeId`] and an [`ecdsa::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`ecdsa::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	fn ecdsa_sign_prehashed(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa::Public,
		msg: &[u8; 32],
	) -> Result<Option<ecdsa::Signature>, Error>;

	/// Returns all the bandersnatch public keys for the given key type.
	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_public_keys(&self, key_type: KeyTypeId) -> Vec<bandersnatch::Public>;

	/// Generate a new bandersnatch key pair for the given key type and an optional seed.
	///
	/// Returns an `bandersnatch::Public` key of the generated key pair or an `Err` if
	/// something failed during key generation.
	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<bandersnatch::Public, Error>;

	/// Generate an bandersnatch signature for a given message.
	///
	/// Receives [`KeyTypeId`] and an [`bandersnatch::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`bandersnatch::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_sign(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		msg: &[u8],
	) -> Result<Option<bandersnatch::Signature>, Error>;

	/// Generate a bandersnatch VRF signature for the given data.
	///
	/// Receives [`KeyTypeId`] and an [`bandersnatch::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns `None` if the given `key_type` and `public` combination doesn't
	/// exist in the keystore or an `Err` when something failed.
	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		input: &bandersnatch::vrf::VrfSignData,
	) -> Result<Option<bandersnatch::vrf::VrfSignature>, Error>;

	/// Generate a bandersnatch VRF pre-output for a given input data.
	///
	/// Receives [`KeyTypeId`] and an [`bandersnatch::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns `None` if the given `key_type` and `public` combination doesn't
	/// exist in the keystore or an `Err` when something failed.
	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_vrf_pre_output(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		input: &bandersnatch::vrf::VrfInput,
	) -> Result<Option<bandersnatch::vrf::VrfPreOutput>, Error>;

	/// Generate a bandersnatch ring-VRF signature for the given data.
	///
	/// Receives [`KeyTypeId`] and an [`bandersnatch::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Also takes a [`bandersnatch::ring_vrf::RingProver`] instance obtained from
	/// a valid [`bandersnatch::ring_vrf::RingContext`].
	///
	/// The ring signature is verifiable if the public key corresponding to the
	/// signing [`bandersnatch::Pair`] is part of the ring from which the
	/// [`bandersnatch::ring_vrf::RingProver`] has been constructed.
	/// If not, the produced signature is just useless.
	///
	/// Returns `None` if the given `key_type` and `public` combination doesn't
	/// exist in the keystore or an `Err` when something failed.
	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_ring_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		input: &bandersnatch::vrf::VrfSignData,
		prover: &bandersnatch::ring_vrf::RingProver,
	) -> Result<Option<bandersnatch::ring_vrf::RingVrfSignature>, Error>;

	/// Returns all bls12-381 public keys for the given key type.
	#[cfg(feature = "bls-experimental")]
	fn bls381_public_keys(&self, id: KeyTypeId) -> Vec<bls381::Public>;

	/// Returns all (ecdsa,bls12-381) paired public keys for the given key type.
	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_public_keys(&self, id: KeyTypeId) -> Vec<ecdsa_bls381::Public>;

	/// Generate a new bls381 key pair for the given key type and an optional seed.
	///
	/// Returns an `bls381::Public` key of the generated key pair or an `Err` if
	/// something failed during key generation.
	#[cfg(feature = "bls-experimental")]
	fn bls381_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<bls381::Public, Error>;

	/// Generate a new (ecdsa,bls381) key pair for the given key type and an optional seed.
	///
	/// Returns an `ecdsa_bls381::Public` key of the generated key pair or an `Err` if
	/// something failed during key generation.
	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<ecdsa_bls381::Public, Error>;

	/// Generate a bls381 signature for a given message.
	///
	/// Receives [`KeyTypeId`] and a [`bls381::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`bls381::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	#[cfg(feature = "bls-experimental")]
	fn bls381_sign(
		&self,
		key_type: KeyTypeId,
		public: &bls381::Public,
		msg: &[u8],
	) -> Result<Option<bls381::Signature>, Error>;

	/// Generate a bls381 Proof of Possession for a given public key
	///
	/// Receives ['KeyTypeId'] and a ['bls381::Public'] key to be able to map
	/// them to a private key that exists in the keystore
	///
	/// Returns an ['bls381::Signature'] or 'None' in case the given 'key_type'
	/// and 'public' combination doesn't exist in the keystore.
	/// An 'Err' will be returned if generating the proof of possession itself failed.
	#[cfg(feature = "bls-experimental")]
	fn bls381_generate_proof_of_possession(
		&self,
		key_type: KeyTypeId,
		public: &bls381::Public,
		owner: &[u8],
	) -> Result<Option<bls381::ProofOfPossession>, Error>;

	/// Generate a (ecdsa,bls381) signature pair for a given message.
	///
	/// Receives [`KeyTypeId`] and a [`ecdsa_bls381::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`ecdsa_bls381::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_sign(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa_bls381::Public,
		msg: &[u8],
	) -> Result<Option<ecdsa_bls381::Signature>, Error>;

	/// Hashes the `message` using keccak256 and then signs it using ECDSA
	/// algorithm. It does not affect the behavior of BLS12-381 component. It generates
	/// BLS12-381 Signature according to IETF standard.
	///
	/// Receives [`KeyTypeId`] and a [`ecdsa_bls381::Public`] key to be able to map
	/// them to a private key that exists in the keystore.
	///
	/// Returns an [`ecdsa_bls381::Signature`] or `None` in case the given `key_type`
	/// and `public` combination doesn't exist in the keystore.
	/// An `Err` will be returned if generating the signature itself failed.
	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_sign_with_keccak256(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa_bls381::Public,
		msg: &[u8],
	) -> Result<Option<ecdsa_bls381::Signature>, Error>;

	/// Insert a new secret key.
	fn insert(&self, key_type: KeyTypeId, suri: &str, public: &[u8]) -> Result<(), ()>;

	/// List all supported keys of a given type.
	///
	/// Returns a set of public keys the signer supports in raw format.
	fn keys(&self, key_type: KeyTypeId) -> Result<Vec<Vec<u8>>, Error>;

	/// Checks if the private keys for the given public key and key type combinations exist.
	///
	/// Returns `true` iff all private keys could be found.
	fn has_keys(&self, public_keys: &[(Vec<u8>, KeyTypeId)]) -> bool;

	/// Convenience method to list public keys using the cryptographic primitive
	/// specified by `crypto_id`.
	///
	/// Returns the raw public key bytes for all matching keys.
	fn public_keys_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
	) -> Result<Vec<Vec<u8>>, Error> {
		public_keys_with_default(self, id, crypto_id)
	}

	/// Convenience method to generate a key using the cryptographic primitive
	/// specified by `crypto_id`.
	///
	/// Returns the raw public key bytes of the generated key.
	fn generate_new_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		seed: Option<&str>,
	) -> Result<Vec<u8>, Error> {
		generate_new_with_default(self, id, crypto_id, seed)
	}

	/// Convenience method to sign a message using the given key type and a raw public key
	/// for secret lookup.
	///
	/// The message is signed using the cryptographic primitive specified by `crypto_id`.
	///
	/// Schemes supported by the default trait implementation:
	/// - sr25519
	/// - ed25519
	/// - ecdsa
	/// - bandersnatch
	/// - bls381
	/// - (ecdsa,bls381) paired keys
	///
	/// To support more schemes you can overwrite this method.
	///
	/// Returns the SCALE encoded signature if key is found and supported, `None` if the key doesn't
	/// exist or an error when something failed.
	fn sign_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		public: &[u8],
		msg: &[u8],
	) -> Result<Option<Vec<u8>>, Error> {
		sign_with_default(self, id, crypto_id, public, msg)
	}

	/// Convenience method to generate a VRF signature using the given key type
	/// and a raw public key for secret lookup.
	///
	/// The VRF signing data is SCALE encoded and interpreted using the
	/// cryptographic primitive specified by `crypto_id`.
	///
	/// Schemes supported by the default trait implementation:
	/// - sr25519
	///
	/// Additional schemes can be supported by overriding this method.
	///
	/// Returns the SCALE encoded VRF signature if key is found and supported,
	/// `None` if the key doesn't exist, or an error when something failed.
	fn vrf_sign_with(
		&self,
		id: KeyTypeId,
		_crypto_id: CryptoTypeId,
		_public: &[u8],
		_data: &BabeVrfSignData,
	) -> Result<Option<Vec<u8>>, Error> {
		Err(Error::KeyNotSupported(id))
	}
}

impl<T: Keystore + ?Sized> Keystore for Arc<T> {
	fn sr25519_public_keys(&self, key_type: KeyTypeId) -> Vec<sr25519::Public> {
		(**self).sr25519_public_keys(key_type)
	}

	fn sr25519_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<sr25519::Public, Error> {
		(**self).sr25519_generate_new(key_type, seed)
	}

	fn sr25519_sign(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		msg: &[u8],
	) -> Result<Option<sr25519::Signature>, Error> {
		(**self).sr25519_sign(key_type, public, msg)
	}

	fn sr25519_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		data: &sr25519::vrf::VrfSignData,
	) -> Result<Option<sr25519::vrf::VrfSignature>, Error> {
		(**self).sr25519_vrf_sign(key_type, public, data)
	}

	fn sr25519_vrf_pre_output(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		input: &sr25519::vrf::VrfInput,
	) -> Result<Option<sr25519::vrf::VrfPreOutput>, Error> {
		(**self).sr25519_vrf_pre_output(key_type, public, input)
	}

	fn ed25519_public_keys(&self, key_type: KeyTypeId) -> Vec<ed25519::Public> {
		(**self).ed25519_public_keys(key_type)
	}

	fn ed25519_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<ed25519::Public, Error> {
		(**self).ed25519_generate_new(key_type, seed)
	}

	fn ed25519_sign(
		&self,
		key_type: KeyTypeId,
		public: &ed25519::Public,
		msg: &[u8],
	) -> Result<Option<ed25519::Signature>, Error> {
		(**self).ed25519_sign(key_type, public, msg)
	}

	fn ecdsa_public_keys(&self, key_type: KeyTypeId) -> Vec<ecdsa::Public> {
		(**self).ecdsa_public_keys(key_type)
	}

	fn ecdsa_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<ecdsa::Public, Error> {
		(**self).ecdsa_generate_new(key_type, seed)
	}

	fn ecdsa_sign(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa::Public,
		msg: &[u8],
	) -> Result<Option<ecdsa::Signature>, Error> {
		(**self).ecdsa_sign(key_type, public, msg)
	}

	fn ecdsa_sign_prehashed(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa::Public,
		msg: &[u8; 32],
	) -> Result<Option<ecdsa::Signature>, Error> {
		(**self).ecdsa_sign_prehashed(key_type, public, msg)
	}

	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_public_keys(&self, key_type: KeyTypeId) -> Vec<bandersnatch::Public> {
		(**self).bandersnatch_public_keys(key_type)
	}

	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<bandersnatch::Public, Error> {
		(**self).bandersnatch_generate_new(key_type, seed)
	}

	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_sign(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		msg: &[u8],
	) -> Result<Option<bandersnatch::Signature>, Error> {
		(**self).bandersnatch_sign(key_type, public, msg)
	}

	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		input: &bandersnatch::vrf::VrfSignData,
	) -> Result<Option<bandersnatch::vrf::VrfSignature>, Error> {
		(**self).bandersnatch_vrf_sign(key_type, public, input)
	}

	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_vrf_pre_output(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		input: &bandersnatch::vrf::VrfInput,
	) -> Result<Option<bandersnatch::vrf::VrfPreOutput>, Error> {
		(**self).bandersnatch_vrf_pre_output(key_type, public, input)
	}

	#[cfg(feature = "bandersnatch-experimental")]
	fn bandersnatch_ring_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &bandersnatch::Public,
		input: &bandersnatch::vrf::VrfSignData,
		prover: &bandersnatch::ring_vrf::RingProver,
	) -> Result<Option<bandersnatch::ring_vrf::RingVrfSignature>, Error> {
		(**self).bandersnatch_ring_vrf_sign(key_type, public, input, prover)
	}

	#[cfg(feature = "bls-experimental")]
	fn bls381_public_keys(&self, id: KeyTypeId) -> Vec<bls381::Public> {
		(**self).bls381_public_keys(id)
	}

	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_public_keys(&self, id: KeyTypeId) -> Vec<ecdsa_bls381::Public> {
		(**self).ecdsa_bls381_public_keys(id)
	}

	#[cfg(feature = "bls-experimental")]
	fn bls381_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<bls381::Public, Error> {
		(**self).bls381_generate_new(key_type, seed)
	}

	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> Result<ecdsa_bls381::Public, Error> {
		(**self).ecdsa_bls381_generate_new(key_type, seed)
	}

	#[cfg(feature = "bls-experimental")]
	fn bls381_sign(
		&self,
		key_type: KeyTypeId,
		public: &bls381::Public,
		msg: &[u8],
	) -> Result<Option<bls381::Signature>, Error> {
		(**self).bls381_sign(key_type, public, msg)
	}

	#[cfg(feature = "bls-experimental")]
	fn bls381_generate_proof_of_possession(
		&self,
		key_type: KeyTypeId,
		public: &bls381::Public,
		owner: &[u8],
	) -> Result<Option<bls381::ProofOfPossession>, Error> {
		(**self).bls381_generate_proof_of_possession(key_type, public, owner)
	}

	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_sign(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa_bls381::Public,
		msg: &[u8],
	) -> Result<Option<ecdsa_bls381::Signature>, Error> {
		(**self).ecdsa_bls381_sign(key_type, public, msg)
	}

	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_sign_with_keccak256(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa_bls381::Public,
		msg: &[u8],
	) -> Result<Option<ecdsa_bls381::Signature>, Error> {
		(**self).ecdsa_bls381_sign_with_keccak256(key_type, public, msg)
	}

	fn insert(&self, key_type: KeyTypeId, suri: &str, public: &[u8]) -> Result<(), ()> {
		(**self).insert(key_type, suri, public)
	}

	fn keys(&self, key_type: KeyTypeId) -> Result<Vec<Vec<u8>>, Error> {
		(**self).keys(key_type)
	}

	fn has_keys(&self, public_keys: &[(Vec<u8>, KeyTypeId)]) -> bool {
		(**self).has_keys(public_keys)
	}

	fn public_keys_with(&self, id: KeyTypeId, crypto_id: CryptoTypeId) -> Result<Vec<Vec<u8>>, Error> {
		(**self).public_keys_with(id, crypto_id)
	}

	fn generate_new_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		seed: Option<&str>,
	) -> Result<Vec<u8>, Error> {
		(**self).generate_new_with(id, crypto_id, seed)
	}

	fn sign_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		public: &[u8],
		msg: &[u8],
	) -> Result<Option<Vec<u8>>, Error> {
		(**self).sign_with(id, crypto_id, public, msg)
	}

	fn vrf_sign_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		public: &[u8],
		data: &BabeVrfSignData,
	) -> Result<Option<Vec<u8>>, Error> {
		(**self).vrf_sign_with(id, crypto_id, public, data)
	}
}

/// A shared pointer to a keystore implementation.
pub type KeystorePtr = Arc<dyn Keystore>;

sp_externalities::decl_extension! {
	/// The keystore extension to register/retrieve from the externalities.
	pub struct KeystoreExt(KeystorePtr);
}

impl KeystoreExt {
	/// Create a new instance of `KeystoreExt`
	///
	/// This is more performant as we don't need to wrap keystore in another [`Arc`].
	pub fn from(keystore: KeystorePtr) -> Self {
		Self(keystore)
	}

	/// Create a new instance of `KeystoreExt` using the given `keystore`.
	pub fn new<T: Keystore + 'static>(keystore: T) -> Self {
		Self(Arc::new(keystore))
	}
}

sp_core::generate_feature_enabled_macro!(
	bandersnatch_experimental_enabled,
	feature = "bandersnatch-experimental",
	$
);

sp_core::generate_feature_enabled_macro!(
	bls_experimental_enabled,
	feature = "bls-experimental",
	$
);
