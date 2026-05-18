// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//! Local keystore implementation

use codec::Encode;
use parking_lot::RwLock;
use quip_crypto_primitives::substrate::{
	ed25519_mldsa44::{
		Pair as HybridGrandpaPair, Public as HybridGrandpaPublic, CRYPTO_ID as H144_CRYPTO_ID,
	},
	sr25519_mldsa44::{
		babe as hybrid_babe, Pair as HybridPair, Public as HybridPublic,
		CRYPTO_ID as H344_CRYPTO_ID,
	},
};
use sp_application_crypto::{AppCrypto, AppPair, IsWrappedBy};
use sp_core::{
	crypto::{
		ByteArray, CryptoTypeId, ExposeSecret, KeyTypeId, Pair as CorePair, SecretString, VrfSecret,
	},
	ecdsa, ed25519, sr25519,
};
use sp_keystore::{
	public_keys_with_default, sign_with_default, BabeVrfSignData, Error as TraitError, Keystore,
	KeystorePtr,
};
use std::{
	collections::HashMap,
	fs::{self, File},
	io::Write,
	path::PathBuf,
	sync::Arc,
};

sp_keystore::bandersnatch_experimental_enabled! {
use sp_core::bandersnatch;
}

sp_keystore::bls_experimental_enabled! {
use sp_core::{bls381, ecdsa_bls381, KeccakHasher, proof_of_possession::ProofOfPossessionGenerator};
}

use crate::{Error, Result};

/// Conservative budget for keystore filenames, well below the typical 255-byte
/// `NAME_MAX` on Linux/macOS so headroom remains for filesystem-specific
/// encoding overhead.
const KEYSTORE_FILENAME_BUDGET: usize = 240;

/// Whether `public` is too long for the filename to encode it as raw hex.
///
/// 32-byte classical pubkeys (sr25519/ed25519) + the 4-byte key-type prefix
/// produce a 72-char filename and fit easily; hybrid post-quantum pubkeys
/// (~1344 bytes) blow past `NAME_MAX` and must be hashed into the filename
/// while preserving the full pubkey inside the file body.
fn needs_hashed_filename(public: &[u8]) -> bool {
    8usize.saturating_add(public.len().saturating_mul(2)) > KEYSTORE_FILENAME_BUDGET
}

/// Read the secret URI back from a keystore file written by [`KeystoreInner::write_to_file`].
///
/// Supports both:
/// * legacy format: bare JSON string containing the URI/mnemonic
/// * envelope format: `{"public":"0x…","suri":"<uri>"}` used when the pubkey
///   wouldn't fit in the filename
fn read_suri_from_keystore_file(file: &File) -> Result<String> {
    let raw: serde_json::Value = serde_json::from_reader(file)?;
    match raw {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Object(obj) => match obj.get("suri") {
            Some(serde_json::Value::String(s)) => Ok(s.clone()),
            _ => Err(Error::Io(std::io::Error::other(
                "keystore envelope missing 'suri' string",
            ))),
        },
        _ => Err(Error::Io(std::io::Error::other("unexpected keystore file format"))),
    }
}

/// A local based keystore that is either memory-based or filesystem-based.
pub struct LocalKeystore(RwLock<KeystoreInner>);

impl LocalKeystore {
	/// Create a local keystore from filesystem.
	///
	/// The keystore will be created at `path`. The keystore optionally supports to encrypt/decrypt
	/// the keys in the keystore using `password`.
	///
	/// NOTE: Even when passing a `password`, the keys on disk appear to look like normal secret
	/// uris. However, without having the correct password the secret uri will not generate the
	/// correct private key. See [`SecretUri`](sp_core::crypto::SecretUri) for more information.
	pub fn open<T: Into<PathBuf>>(path: T, password: Option<SecretString>) -> Result<Self> {
		let inner = KeystoreInner::open(path, password)?;
		Ok(Self(RwLock::new(inner)))
	}

	/// Create a local keystore in memory.
	pub fn in_memory() -> Self {
		let inner = KeystoreInner::new_in_memory();
		Self(RwLock::new(inner))
	}

	/// Get a key pair for the given public key.
	///
	/// Returns `Ok(None)` if the key doesn't exist, `Ok(Some(_))` if the key exists and
	/// `Err(_)` when something failed.
	pub fn key_pair<Pair: AppPair>(
		&self,
		public: &<Pair as AppCrypto>::Public,
	) -> Result<Option<Pair>> {
		self.0.read().key_pair::<Pair>(public)
	}

	fn public_keys<T: CorePair>(&self, key_type: KeyTypeId) -> Vec<T::Public> {
		self.0
			.read()
			.raw_public_keys(key_type)
			.map(|v| {
				v.into_iter().filter_map(|k| T::Public::from_slice(k.as_slice()).ok()).collect()
			})
			.unwrap_or_default()
	}

	fn generate_new<T: CorePair>(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> std::result::Result<T::Public, TraitError> {
		let pair = match seed {
			Some(seed) => self.0.write().insert_ephemeral_from_seed_by_type::<T>(seed, key_type),
			None => self.0.write().generate_by_type::<T>(key_type),
		}
		.map_err(|e| -> TraitError { e.into() })?;
		Ok(pair.public())
	}

	fn sign<T: CorePair>(
		&self,
		key_type: KeyTypeId,
		public: &T::Public,
		msg: &[u8],
	) -> std::result::Result<Option<T::Signature>, TraitError> {
		let signature = self
			.0
			.read()
			.key_pair_by_type::<T>(public, key_type)?
			.map(|pair| pair.sign(msg));
		Ok(signature)
	}

	fn vrf_sign<T: CorePair + VrfSecret>(
		&self,
		key_type: KeyTypeId,
		public: &T::Public,
		data: &T::VrfSignData,
	) -> std::result::Result<Option<T::VrfSignature>, TraitError> {
		let sig = self
			.0
			.read()
			.key_pair_by_type::<T>(public, key_type)?
			.map(|pair| pair.vrf_sign(data));
		Ok(sig)
	}

	fn vrf_pre_output<T: CorePair + VrfSecret>(
		&self,
		key_type: KeyTypeId,
		public: &T::Public,
		input: &T::VrfInput,
	) -> std::result::Result<Option<T::VrfPreOutput>, TraitError> {
		let pre_output = self
			.0
			.read()
			.key_pair_by_type::<T>(public, key_type)?
			.map(|pair| pair.vrf_pre_output(input));
		Ok(pre_output)
	}

	sp_keystore::bls_experimental_enabled! {
		fn generate_proof_of_possession<T: CorePair + ProofOfPossessionGenerator>(
			&self,
			key_type: KeyTypeId,
			public: &T::Public,
			owner: &[u8],
		) -> std::result::Result<Option<T::ProofOfPossession>, TraitError> {
			let proof_of_possession = self
				.0
				.read()
				.key_pair_by_type::<T>(public, key_type)?
				.map(|mut pair| pair.generate_proof_of_possession(owner));
			Ok(proof_of_possession)
		}
	}
}

impl Keystore for LocalKeystore {
	/// Insert a new secret key.
	///
	/// WARNING: if the secret keypair has been manually generated using a password
	/// (e.g. using methods such as [`sp_core::crypto::Pair::from_phrase`]) then such
	/// a password must match the one used to open the keystore via [`LocalKeystore::open`].
	/// If the passwords doesn't match then the inserted key ends up being unusable under
	/// the current keystore instance.
	fn insert(
		&self,
		key_type: KeyTypeId,
		suri: &str,
		public: &[u8],
	) -> std::result::Result<(), ()> {
		self.0.write().insert(key_type, suri, public).map_err(|_| ())
	}

	fn keys(&self, key_type: KeyTypeId) -> std::result::Result<Vec<Vec<u8>>, TraitError> {
		self.0.read().raw_public_keys(key_type).map_err(|e| e.into())
	}

	fn has_keys(&self, public_keys: &[(Vec<u8>, KeyTypeId)]) -> bool {
		public_keys
			.iter()
			.all(|(p, t)| self.0.read().key_phrase_by_type(p, *t).ok().flatten().is_some())
	}

	fn public_keys_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
	) -> std::result::Result<Vec<Vec<u8>>, TraitError> {
		match crypto_id {
			H144_CRYPTO_ID => Ok(self
				.public_keys::<HybridGrandpaPair>(id)
				.into_iter()
				.map(|public| public.to_raw_vec())
				.collect()),
			H344_CRYPTO_ID => Ok(self
				.public_keys::<HybridPair>(id)
				.into_iter()
				.map(|public| public.to_raw_vec())
				.collect()),
			_ => public_keys_with_default(self, id, crypto_id),
		}
	}

	fn generate_new_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		seed: Option<&str>,
	) -> std::result::Result<Vec<u8>, TraitError> {
		match crypto_id {
			H144_CRYPTO_ID => self
				.generate_new::<HybridGrandpaPair>(id, seed)
				.map(|public| public.to_raw_vec()),
			H344_CRYPTO_ID => {
				self.generate_new::<HybridPair>(id, seed).map(|public| public.to_raw_vec())
			},
			_ => sp_keystore::generate_new_with_default(self, id, crypto_id, seed),
		}
	}

	fn sign_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		public: &[u8],
		msg: &[u8],
	) -> std::result::Result<Option<Vec<u8>>, TraitError> {
		match crypto_id {
			H144_CRYPTO_ID => {
				let public = HybridGrandpaPublic::from_slice(public)
					.map_err(|_| TraitError::ValidationError("Invalid public key format".into()))?;
				self.sign::<HybridGrandpaPair>(id, &public, msg)
					.map(|signature| signature.map(|s| s.encode()))
			},
			H344_CRYPTO_ID => {
				let public = HybridPublic::from_slice(public)
					.map_err(|_| TraitError::ValidationError("Invalid public key format".into()))?;
				self.sign::<HybridPair>(id, &public, msg)
					.map(|signature| signature.map(|s| s.encode()))
			},
			_ => sign_with_default(self, id, crypto_id, public, msg),
		}
	}

	fn vrf_sign_with(
		&self,
		id: KeyTypeId,
		crypto_id: CryptoTypeId,
		public: &[u8],
		data: &BabeVrfSignData,
	) -> std::result::Result<Option<Vec<u8>>, TraitError> {
		let babe_vrf_data =
			hybrid_babe::make_vrf_sign_data(&data.randomness, data.slot, data.epoch);

		match crypto_id {
			sr25519::CRYPTO_ID => {
				let public = sr25519::Public::from_slice(public)
					.map_err(|_| TraitError::ValidationError("Invalid public key format".into()))?;
				let data = hybrid_babe::make_sr25519_vrf_sign_data(
					&data.randomness,
					data.slot,
					data.epoch,
				);
				self.vrf_sign::<sr25519::Pair>(id, &public, &data)
					.map(|signature| signature.map(|s| s.encode()))
			},
			H344_CRYPTO_ID => {
				let public = HybridPublic::from_slice(public)
					.map_err(|_| TraitError::ValidationError("Invalid public key format".into()))?;
				let data = babe_vrf_data;
				self.vrf_sign::<HybridPair>(id, &public, &data)
					.map(|signature| signature.map(|s| s.encode()))
			},
			_ => Err(TraitError::KeyNotSupported(id)),
		}
	}

	fn sr25519_public_keys(&self, key_type: KeyTypeId) -> Vec<sr25519::Public> {
		self.public_keys::<sr25519::Pair>(key_type)
	}

	/// Generate a new pair compatible with the 'ed25519' signature scheme.
	///
	/// If `[seed]` is `Some` then the key will be ephemeral and stored in memory.
	fn sr25519_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> std::result::Result<sr25519::Public, TraitError> {
		self.generate_new::<sr25519::Pair>(key_type, seed)
	}

	fn sr25519_sign(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		msg: &[u8],
	) -> std::result::Result<Option<sr25519::Signature>, TraitError> {
		self.sign::<sr25519::Pair>(key_type, public, msg)
	}

	fn sr25519_vrf_sign(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		data: &sr25519::vrf::VrfSignData,
	) -> std::result::Result<Option<sr25519::vrf::VrfSignature>, TraitError> {
		self.vrf_sign::<sr25519::Pair>(key_type, public, data)
	}

	fn sr25519_vrf_pre_output(
		&self,
		key_type: KeyTypeId,
		public: &sr25519::Public,
		input: &sr25519::vrf::VrfInput,
	) -> std::result::Result<Option<sr25519::vrf::VrfPreOutput>, TraitError> {
		self.vrf_pre_output::<sr25519::Pair>(key_type, public, input)
	}

	fn ed25519_public_keys(&self, key_type: KeyTypeId) -> Vec<ed25519::Public> {
		self.public_keys::<ed25519::Pair>(key_type)
	}

	/// Generate a new pair compatible with the 'sr25519' signature scheme.
	///
	/// If `[seed]` is `Some` then the key will be ephemeral and stored in memory.
	fn ed25519_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> std::result::Result<ed25519::Public, TraitError> {
		self.generate_new::<ed25519::Pair>(key_type, seed)
	}

	fn ed25519_sign(
		&self,
		key_type: KeyTypeId,
		public: &ed25519::Public,
		msg: &[u8],
	) -> std::result::Result<Option<ed25519::Signature>, TraitError> {
		self.sign::<ed25519::Pair>(key_type, public, msg)
	}

	fn ecdsa_public_keys(&self, key_type: KeyTypeId) -> Vec<ecdsa::Public> {
		self.public_keys::<ecdsa::Pair>(key_type)
	}

	/// Generate a new pair compatible with the 'ecdsa' signature scheme.
	///
	/// If `[seed]` is `Some` then the key will be ephemeral and stored in memory.
	fn ecdsa_generate_new(
		&self,
		key_type: KeyTypeId,
		seed: Option<&str>,
	) -> std::result::Result<ecdsa::Public, TraitError> {
		self.generate_new::<ecdsa::Pair>(key_type, seed)
	}

	fn ecdsa_sign(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa::Public,
		msg: &[u8],
	) -> std::result::Result<Option<ecdsa::Signature>, TraitError> {
		self.sign::<ecdsa::Pair>(key_type, public, msg)
	}

	fn ecdsa_sign_prehashed(
		&self,
		key_type: KeyTypeId,
		public: &ecdsa::Public,
		msg: &[u8; 32],
	) -> std::result::Result<Option<ecdsa::Signature>, TraitError> {
		let sig = self
			.0
			.read()
			.key_pair_by_type::<ecdsa::Pair>(public, key_type)?
			.map(|pair| pair.sign_prehashed(msg));
		Ok(sig)
	}

	sp_keystore::bandersnatch_experimental_enabled! {
		fn bandersnatch_public_keys(&self, key_type: KeyTypeId) -> Vec<bandersnatch::Public> {
			self.public_keys::<bandersnatch::Pair>(key_type)
		}

		/// Generate a new pair compatible with the 'bandersnatch' signature scheme.
		///
		/// If `[seed]` is `Some` then the key will be ephemeral and stored in memory.
		fn bandersnatch_generate_new(
			&self,
			key_type: KeyTypeId,
			seed: Option<&str>,
		) -> std::result::Result<bandersnatch::Public, TraitError> {
			self.generate_new::<bandersnatch::Pair>(key_type, seed)
		}

		fn bandersnatch_sign(
			&self,
			key_type: KeyTypeId,
			public: &bandersnatch::Public,
			msg: &[u8],
		) -> std::result::Result<Option<bandersnatch::Signature>, TraitError> {
			self.sign::<bandersnatch::Pair>(key_type, public, msg)
		}

		fn bandersnatch_vrf_sign(
			&self,
			key_type: KeyTypeId,
			public: &bandersnatch::Public,
			data: &bandersnatch::vrf::VrfSignData,
		) -> std::result::Result<Option<bandersnatch::vrf::VrfSignature>, TraitError> {
			self.vrf_sign::<bandersnatch::Pair>(key_type, public, data)
		}

		fn bandersnatch_vrf_pre_output(
			&self,
			key_type: KeyTypeId,
			public: &bandersnatch::Public,
			input: &bandersnatch::vrf::VrfInput,
		) -> std::result::Result<Option<bandersnatch::vrf::VrfPreOutput>, TraitError> {
			self.vrf_pre_output::<bandersnatch::Pair>(key_type, public, input)
		}

		fn bandersnatch_ring_vrf_sign(
			&self,
			key_type: KeyTypeId,
			public: &bandersnatch::Public,
			data: &bandersnatch::vrf::VrfSignData,
			prover: &bandersnatch::ring_vrf::RingProver,
		) -> std::result::Result<Option<bandersnatch::ring_vrf::RingVrfSignature>, TraitError> {
			let sig = self
				.0
				.read()
				.key_pair_by_type::<bandersnatch::Pair>(public, key_type)?
				.map(|pair| pair.ring_vrf_sign(data, prover));
			Ok(sig)
		}
	}

	sp_keystore::bls_experimental_enabled! {
		fn bls381_public_keys(&self, key_type: KeyTypeId) -> Vec<bls381::Public> {
			self.public_keys::<bls381::Pair>(key_type)
		}

		/// Generate a new pair compatible with the 'bls381' signature scheme.
		///
		/// If `[seed]` is `Some` then the key will be ephemeral and stored in memory.
		fn bls381_generate_new(
			&self,
			key_type: KeyTypeId,
			seed: Option<&str>,
		) -> std::result::Result<bls381::Public, TraitError> {
			self.generate_new::<bls381::Pair>(key_type, seed)
		}

		fn bls381_sign(
			&self,
			key_type: KeyTypeId,
			public: &bls381::Public,
			msg: &[u8],
		) -> std::result::Result<Option<bls381::Signature>, TraitError> {
			self.sign::<bls381::Pair>(key_type, public, msg)
		}

		fn bls381_generate_proof_of_possession(
			&self,
			key_type: KeyTypeId,
			public: &bls381::Public,
			owner: &[u8],
		) -> std::result::Result<Option<bls381::ProofOfPossession>, TraitError> {
			self.generate_proof_of_possession::<bls381::Pair>(key_type, public, owner)
		}

		fn ecdsa_bls381_public_keys(&self, key_type: KeyTypeId) -> Vec<ecdsa_bls381::Public> {
			self.public_keys::<ecdsa_bls381::Pair>(key_type)
		}

		/// Generate a new pair of paired-keys compatible with the '(ecdsa,bls381)' signature scheme.
		///
		/// If `[seed]` is `Some` then the key will be ephemeral and stored in memory.
		fn ecdsa_bls381_generate_new(
			&self,
			key_type: KeyTypeId,
			seed: Option<&str>,
		) -> std::result::Result<ecdsa_bls381::Public, TraitError> {
			let pubkey = self.generate_new::<ecdsa_bls381::Pair>(key_type, seed)?;

			let s = self
				.0
				.read()
				.additional
				.get(&(key_type, pubkey.to_vec()))
				.map(|s| s.to_string())
				.expect("Can retrieve seed");

			// This is done to give the keystore access to individual keys, this is necessary to avoid
			// unnecessary host functions for paired keys and re-use host functions implemented for each
			// element of the pair.
			self.generate_new::<ecdsa::Pair>(key_type, Some(&*s)).expect("seed slice is valid");
			self.generate_new::<bls381::Pair>(key_type, Some(&*s)).expect("seed slice is valid");

			Ok(pubkey)
		}

		fn ecdsa_bls381_sign(
			&self,
			key_type: KeyTypeId,
			public: &ecdsa_bls381::Public,
			msg: &[u8],
		) -> std::result::Result<Option<ecdsa_bls381::Signature>, TraitError> {
			self.sign::<ecdsa_bls381::Pair>(key_type, public, msg)
		}

		fn ecdsa_bls381_sign_with_keccak256(
			&self,
			key_type: KeyTypeId,
			public: &ecdsa_bls381::Public,
			msg: &[u8],
		) -> std::result::Result<Option<ecdsa_bls381::Signature>, TraitError> {
			 let sig = self.0
			.read()
			.key_pair_by_type::<ecdsa_bls381::Pair>(public, key_type)?
			.map(|pair| pair.sign_with_hasher::<KeccakHasher>(msg));
			Ok(sig)
		}
	}
}

impl Into<KeystorePtr> for LocalKeystore {
	fn into(self) -> KeystorePtr {
		Arc::new(self)
	}
}

/// A local key store.
///
/// Stores key pairs in a file system store + short lived key pairs in memory.
///
/// Every pair that is being generated by a `seed`, will be placed in memory.
struct KeystoreInner {
	path: Option<PathBuf>,
	/// Map over `(KeyTypeId, Raw public key)` -> `Key phrase/seed`
	additional: HashMap<(KeyTypeId, Vec<u8>), String>,
	password: Option<SecretString>,
}

impl KeystoreInner {
	/// Open the store at the given path.
	///
	/// Optionally takes a password that will be used to encrypt/decrypt the keys.
	fn open<T: Into<PathBuf>>(path: T, password: Option<SecretString>) -> Result<Self> {
		let path = path.into();
		fs::create_dir_all(&path)?;

		Ok(Self { path: Some(path), additional: HashMap::new(), password })
	}

	/// Get the password for this store.
	fn password(&self) -> Option<&str> {
		self.password.as_ref().map(|p| p.expose_secret()).map(|p| p.as_str())
	}

	/// Create a new in-memory store.
	fn new_in_memory() -> Self {
		Self { path: None, additional: HashMap::new(), password: None }
	}

	/// Get the key phrase for the given public key and key type from the in-memory store.
	fn get_additional_pair(&self, public: &[u8], key_type: KeyTypeId) -> Option<&String> {
		let key = (key_type, public.to_vec());
		self.additional.get(&key)
	}

	/// Insert the given public/private key pair with the given key type.
	///
	/// Does not place it into the file system store.
	fn insert_ephemeral_pair<Pair: CorePair>(
		&mut self,
		pair: &Pair,
		seed: &str,
		key_type: KeyTypeId,
	) {
		let key = (key_type, pair.public().to_raw_vec());
		self.additional.insert(key, seed.into());
	}

	/// Insert a new key with anonymous crypto.
	///
	/// Places it into the file system store, if a path is configured.
	fn insert(&self, key_type: KeyTypeId, suri: &str, public: &[u8]) -> Result<()> {
		if let Some(path) = self.key_file_path(public, key_type) {
			Self::write_to_file(path, suri, public)?;
		}

		Ok(())
	}

	/// Generate a new key.
	///
	/// Places it into the file system store, if a path is configured. Otherwise insert
	/// it into the memory cache only.
	fn generate_by_type<Pair: CorePair>(&mut self, key_type: KeyTypeId) -> Result<Pair> {
		let (pair, phrase, _) = Pair::generate_with_phrase(self.password());
		let public_bytes = pair.public().to_raw_vec();
		if let Some(path) = self.key_file_path(&public_bytes, key_type) {
			Self::write_to_file(path, &phrase, &public_bytes)?;
		} else {
			self.insert_ephemeral_pair(&pair, &phrase, key_type);
		}

		Ok(pair)
	}

	/// Write `suri` to `file`, with format depending on public-key length:
	///
	/// * **Legacy** (≤ 32 bytes pubkey): JSON string `"<suri>"`. Filename encodes
	///   the full pubkey hex so [`raw_public_keys`] can recover it.
	/// * **Envelope** (long pubkey, e.g. hybrid post-quantum): JSON object
	///   `{"public":"0x…","suri":"<suri>"}`. The filename is hash-based to fit
	///   under `NAME_MAX`, so the full pubkey must travel inside the file.
	///
	/// Both formats are recognised on read.
	fn write_to_file(file: PathBuf, suri: &str, public: &[u8]) -> Result<()> {
		let mut file = File::create(file)?;

		#[cfg(target_family = "unix")]
		{
			use std::os::unix::fs::PermissionsExt;
			file.set_permissions(fs::Permissions::from_mode(0o600))?;
		}

		if needs_hashed_filename(public) {
			let envelope = serde_json::json!({
				"public": format!("0x{}", array_bytes::bytes2hex("", public)),
				"suri": suri,
			});
			serde_json::to_writer(&file, &envelope)?;
		} else {
			serde_json::to_writer(&file, suri)?;
		}
		file.flush()?;
		Ok(())
	}

	/// Create a new key from seed.
	///
	/// Does not place it into the file system store.
	fn insert_ephemeral_from_seed_by_type<Pair: CorePair>(
		&mut self,
		seed: &str,
		key_type: KeyTypeId,
	) -> Result<Pair> {
		let pair = Pair::from_string(seed, None).map_err(|_| Error::InvalidSeed)?;
		self.insert_ephemeral_pair(&pair, seed, key_type);
		Ok(pair)
	}

	/// Get the key phrase for a given public key and key type.
	fn key_phrase_by_type(&self, public: &[u8], key_type: KeyTypeId) -> Result<Option<String>> {
		if let Some(phrase) = self.get_additional_pair(public, key_type) {
			return Ok(Some(phrase.clone()));
		}

		let path = if let Some(path) = self.key_file_path(public, key_type) {
			path
		} else {
			return Ok(None);
		};

		if path.exists() {
			let file = File::open(path)?;
			read_suri_from_keystore_file(&file).map(Some)
		} else {
			Ok(None)
		}
	}

	/// Get a key pair for the given public key and key type.
	fn key_pair_by_type<Pair: CorePair>(
		&self,
		public: &Pair::Public,
		key_type: KeyTypeId,
	) -> Result<Option<Pair>> {
		let phrase = if let Some(p) = self.key_phrase_by_type(public.as_slice(), key_type)? {
			p
		} else {
			return Ok(None);
		};

		let pair = Pair::from_string(&phrase, self.password()).map_err(|_| Error::InvalidPhrase)?;

		if &pair.public() == public {
			Ok(Some(pair))
		} else {
			Err(Error::PublicKeyMismatch)
		}
	}

	/// Get the file path for the given public key and key type.
	///
	/// Returns `None` if the keystore only exists in-memory and there isn't any path to provide.
	///
	/// Most filesystems cap filenames at 255 bytes (`NAME_MAX`). For classical
	/// 32-byte pubkeys the full hex (64 chars + 8-char key-type prefix) fits
	/// easily; for hybrid post-quantum pubkeys (~1344 bytes raw) it does not.
	/// In that case we encode the pubkey via a blake2_256 hash so the filename
	/// stays under the limit; the full pubkey is preserved inside the file
	/// (see [`write_to_file`] and [`read_suri_from_keystore_file`]).
	fn key_file_path(&self, public: &[u8], key_type: KeyTypeId) -> Option<PathBuf> {
		let mut buf = self.path.as_ref()?.clone();
		let key_type_hex = array_bytes::bytes2hex("", &key_type.0);
		let suffix = if needs_hashed_filename(public) {
			array_bytes::bytes2hex("", sp_core::hashing::blake2_256(public))
		} else {
			array_bytes::bytes2hex("", public)
		};
		buf.push(key_type_hex + suffix.as_str());
		Some(buf)
	}

	/// Returns a list of raw public keys filtered by `KeyTypeId`
	fn raw_public_keys(&self, key_type: KeyTypeId) -> Result<Vec<Vec<u8>>> {
		let mut public_keys: Vec<Vec<u8>> = self
			.additional
			.keys()
			.into_iter()
			.filter_map(|k| if k.0 == key_type { Some(k.1.clone()) } else { None })
			.collect();

		if let Some(path) = &self.path {
			for entry in fs::read_dir(&path)? {
				let entry = entry?;
				let entry_path = entry.path();

				// skip directories and non-unicode file names (hex is unicode)
				let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) else {
					continue;
				};

				let prefix_hex = array_bytes::bytes2hex("", &key_type.0);
				let Some(suffix_hex) = name.strip_prefix(&prefix_hex) else {
					continue;
				};

				// Try to read the file as the envelope format first; the public
				// key in the envelope is authoritative when the filename is a
				// hash (long pubkey case). For legacy files (short pubkey) the
				// content is a plain JSON string and we recover the pubkey
				// from the filename suffix.
				let Ok(file) = File::open(&entry_path) else { continue };
				let Ok(raw) = serde_json::from_reader::<_, serde_json::Value>(&file) else {
					continue;
				};
				match raw {
					serde_json::Value::Object(obj) => {
						if let Some(serde_json::Value::String(pub_hex)) = obj.get("public") {
							let hex = pub_hex.trim_start_matches("0x");
							if let Ok(public) = array_bytes::hex2bytes(hex) {
								public_keys.push(public);
							}
						}
					},
					serde_json::Value::String(_) => {
						if let Ok(public) = array_bytes::hex2bytes(suffix_hex) {
							public_keys.push(public);
						}
					},
					_ => continue,
				}
			}
		}

		Ok(public_keys)
	}

	/// Get a key pair for the given public key.
	///
	/// Returns `Ok(None)` if the key doesn't exist, `Ok(Some(_))` if the key exists or `Err(_)`
	/// when something failed.
	pub fn key_pair<Pair: AppPair>(
		&self,
		public: &<Pair as AppCrypto>::Public,
	) -> Result<Option<Pair>> {
		self.key_pair_by_type::<Pair::Generic>(IsWrappedBy::from_ref(public), Pair::ID)
			.map(|v| v.map(Into::into))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_application_crypto::{ed25519, sr25519, AppPublic};
	use sp_core::{crypto::Ss58Codec, testing::SR25519, Pair};
	use std::{fs, str::FromStr};
	use tempfile::TempDir;

	const TEST_KEY_TYPE: KeyTypeId = KeyTypeId(*b"test");

	impl KeystoreInner {
		fn insert_ephemeral_from_seed<Pair: AppPair>(&mut self, seed: &str) -> Result<Pair> {
			self.insert_ephemeral_from_seed_by_type::<Pair::Generic>(seed, Pair::ID)
				.map(Into::into)
		}

		fn public_keys<Public: AppPublic>(&self) -> Result<Vec<Public>> {
			self.raw_public_keys(Public::ID).map(|v| {
				v.into_iter().filter_map(|k| Public::from_slice(k.as_slice()).ok()).collect()
			})
		}

		fn generate<Pair: AppPair>(&mut self) -> Result<Pair> {
			self.generate_by_type::<Pair::Generic>(Pair::ID).map(Into::into)
		}
	}

	#[test]
	fn basic_store() {
		let temp_dir = TempDir::new().unwrap();
		let mut store = KeystoreInner::open(temp_dir.path(), None).unwrap();

		assert!(store.public_keys::<ed25519::AppPublic>().unwrap().is_empty());

		let key: ed25519::AppPair = store.generate().unwrap();
		let key2: ed25519::AppPair = store.key_pair(&key.public()).unwrap().unwrap();

		assert_eq!(key.public(), key2.public());

		assert_eq!(store.public_keys::<ed25519::AppPublic>().unwrap()[0], key.public());
	}

	#[test]
	fn has_keys_works() {
		let temp_dir = TempDir::new().unwrap();
		let store = LocalKeystore::open(temp_dir.path(), None).unwrap();

		let key: ed25519::AppPair = store.0.write().generate().unwrap();
		let key2 = ed25519::Pair::generate().0;

		assert!(!store.has_keys(&[(key2.public().to_vec(), ed25519::AppPublic::ID)]));

		assert!(!store.has_keys(&[
			(key2.public().to_vec(), ed25519::AppPublic::ID),
			(key.public().to_raw_vec(), ed25519::AppPublic::ID),
		],));

		assert!(store.has_keys(&[(key.public().to_raw_vec(), ed25519::AppPublic::ID)]));
	}

	#[test]
	fn test_insert_ephemeral_from_seed() {
		let temp_dir = TempDir::new().unwrap();
		let mut store = KeystoreInner::open(temp_dir.path(), None).unwrap();

		let pair: ed25519::AppPair = store
			.insert_ephemeral_from_seed(
				"0x3d97c819d68f9bafa7d6e79cb991eebcd77d966c5334c0b94d9e1fa7ad0869dc",
			)
			.unwrap();
		assert_eq!(
			"5DKUrgFqCPV8iAXx9sjy1nyBygQCeiUYRFWurZGhnrn3HJCA",
			pair.public().to_ss58check()
		);

		drop(store);
		let store = KeystoreInner::open(temp_dir.path(), None).unwrap();
		// Keys generated from seed should not be persisted!
		assert!(store.key_pair::<ed25519::AppPair>(&pair.public()).unwrap().is_none());
	}

	#[test]
	fn password_being_used() {
		let password = String::from("password");
		let temp_dir = TempDir::new().unwrap();
		let mut store = KeystoreInner::open(
			temp_dir.path(),
			Some(FromStr::from_str(password.as_str()).unwrap()),
		)
		.unwrap();

		let pair: ed25519::AppPair = store.generate().unwrap();
		assert_eq!(
			pair.public(),
			store.key_pair::<ed25519::AppPair>(&pair.public()).unwrap().unwrap().public(),
		);

		// Without the password the key should not be retrievable
		let store = KeystoreInner::open(temp_dir.path(), None).unwrap();
		assert!(store.key_pair::<ed25519::AppPair>(&pair.public()).is_err());

		let store = KeystoreInner::open(
			temp_dir.path(),
			Some(FromStr::from_str(password.as_str()).unwrap()),
		)
		.unwrap();
		assert_eq!(
			pair.public(),
			store.key_pair::<ed25519::AppPair>(&pair.public()).unwrap().unwrap().public(),
		);
	}

	#[test]
	fn public_keys_are_returned() {
		let temp_dir = TempDir::new().unwrap();
		let mut store = KeystoreInner::open(temp_dir.path(), None).unwrap();

		let mut keys = Vec::new();
		for i in 0..10 {
			keys.push(store.generate::<ed25519::AppPair>().unwrap().public());
			keys.push(
				store
					.insert_ephemeral_from_seed::<ed25519::AppPair>(&format!(
						"0x3d97c819d68f9bafa7d6e79cb991eebcd7{}d966c5334c0b94d9e1fa7ad0869dc",
						i
					))
					.unwrap()
					.public(),
			);
		}

		// Generate a key of a different type
		store.generate::<sr25519::AppPair>().unwrap();

		keys.sort();
		let mut store_pubs = store.public_keys::<ed25519::AppPublic>().unwrap();
		store_pubs.sort();

		assert_eq!(keys, store_pubs);
	}

	#[test]
	fn store_unknown_and_extract_it() {
		let temp_dir = TempDir::new().unwrap();
		let store = KeystoreInner::open(temp_dir.path(), None).unwrap();

		let secret_uri = "//Alice";
		let key_pair = sr25519::AppPair::from_string(secret_uri, None).expect("Generates key pair");

		store
			.insert(SR25519, secret_uri, key_pair.public().as_ref())
			.expect("Inserts unknown key");

		let store_key_pair = store
			.key_pair_by_type::<sr25519::AppPair>(&key_pair.public(), SR25519)
			.expect("Gets key pair from keystore")
			.unwrap();

		assert_eq!(key_pair.public(), store_key_pair.public());
	}

	#[test]
	fn store_ignores_files_with_invalid_name() {
		let temp_dir = TempDir::new().unwrap();
		let store = LocalKeystore::open(temp_dir.path(), None).unwrap();

		let file_name = temp_dir.path().join(array_bytes::bytes2hex("", &SR25519.0[..2]));
		fs::write(file_name, "test").expect("Invalid file is written");

		assert!(store.sr25519_public_keys(SR25519).is_empty());
	}

	#[test]
	fn generate_with_seed_is_not_stored() {
		let temp_dir = TempDir::new().unwrap();
		let store = LocalKeystore::open(temp_dir.path(), None).unwrap();
		let _alice_tmp_key = store.sr25519_generate_new(TEST_KEY_TYPE, Some("//Alice")).unwrap();

		assert_eq!(store.sr25519_public_keys(TEST_KEY_TYPE).len(), 1);

		drop(store);
		let store = LocalKeystore::open(temp_dir.path(), None).unwrap();
		assert_eq!(store.sr25519_public_keys(TEST_KEY_TYPE).len(), 0);
	}

	#[test]
	fn generate_can_be_fetched_in_memory() {
		let store = LocalKeystore::in_memory();
		store.sr25519_generate_new(TEST_KEY_TYPE, Some("//Alice")).unwrap();

		assert_eq!(store.sr25519_public_keys(TEST_KEY_TYPE).len(), 1);
		store.sr25519_generate_new(TEST_KEY_TYPE, None).unwrap();
		assert_eq!(store.sr25519_public_keys(TEST_KEY_TYPE).len(), 2);
	}

	#[test]
	#[cfg(target_family = "unix")]
	fn uses_correct_file_permissions_on_unix() {
		use std::os::unix::fs::PermissionsExt;

		let temp_dir = TempDir::new().unwrap();
		let store = LocalKeystore::open(temp_dir.path(), None).unwrap();

		let public = store.sr25519_generate_new(TEST_KEY_TYPE, None).unwrap();

		let path = store.0.read().key_file_path(public.as_ref(), TEST_KEY_TYPE).unwrap();
		let permissions = File::open(path).unwrap().metadata().unwrap().permissions();

		assert_eq!(0o100600, permissions.mode());
	}

	#[test]
	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_generate_with_none_works() {
		use sp_core::testing::ECDSA_BLS381;

		let store = LocalKeystore::in_memory();
		let ecdsa_bls381_key =
			store.ecdsa_bls381_generate_new(ECDSA_BLS381, None).expect("Cant generate key");

		let ecdsa_keys = store.ecdsa_public_keys(ECDSA_BLS381);
		let bls381_keys = store.bls381_public_keys(ECDSA_BLS381);
		let ecdsa_bls381_keys = store.ecdsa_bls381_public_keys(ECDSA_BLS381);

		assert_eq!(ecdsa_keys.len(), 1);
		assert_eq!(bls381_keys.len(), 1);
		assert_eq!(ecdsa_bls381_keys.len(), 1);

		let ecdsa_key = ecdsa_keys[0];
		let bls381_key = bls381_keys[0];

		let mut combined_key_raw = [0u8; ecdsa_bls381::PUBLIC_KEY_LEN];
		combined_key_raw[..ecdsa::PUBLIC_KEY_SERIALIZED_SIZE].copy_from_slice(ecdsa_key.as_ref());
		combined_key_raw[ecdsa::PUBLIC_KEY_SERIALIZED_SIZE..].copy_from_slice(bls381_key.as_ref());
		let combined_key = ecdsa_bls381::Public::from_raw(combined_key_raw);

		assert_eq!(combined_key, ecdsa_bls381_key);
	}

	#[test]
	#[cfg(feature = "bls-experimental")]
	fn ecdsa_bls381_generate_with_seed_works() {
		use sp_core::testing::ECDSA_BLS381;

		let store = LocalKeystore::in_memory();
		let ecdsa_bls381_key = store
			.ecdsa_bls381_generate_new(ECDSA_BLS381, Some("//Alice"))
			.expect("Cant generate key");

		let ecdsa_keys = store.ecdsa_public_keys(ECDSA_BLS381);
		let bls381_keys = store.bls381_public_keys(ECDSA_BLS381);
		let ecdsa_bls381_keys = store.ecdsa_bls381_public_keys(ECDSA_BLS381);

		assert_eq!(ecdsa_keys.len(), 1);
		assert_eq!(bls381_keys.len(), 1);
		assert_eq!(ecdsa_bls381_keys.len(), 1);

		let ecdsa_key = ecdsa_keys[0];
		let bls381_key = bls381_keys[0];

		let mut combined_key_raw = [0u8; ecdsa_bls381::PUBLIC_KEY_LEN];
		combined_key_raw[..ecdsa::PUBLIC_KEY_SERIALIZED_SIZE].copy_from_slice(ecdsa_key.as_ref());
		combined_key_raw[ecdsa::PUBLIC_KEY_SERIALIZED_SIZE..].copy_from_slice(bls381_key.as_ref());
		let combined_key = ecdsa_bls381::Public::from_raw(combined_key_raw);

		assert_eq!(combined_key, ecdsa_bls381_key);
	}
}
