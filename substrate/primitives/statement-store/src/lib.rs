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

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

//! A crate which contains statement-store primitives.

extern crate alloc;

use alloc::vec::Vec;
use codec::{Compact, Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::ops::Deref;
use scale_info::{build::Fields, Path, Type, TypeInfo};
use sp_application_crypto::RuntimeAppPublic;
#[cfg(feature = "std")]
use sp_core::Pair;

/// Statement topic.
///
/// A 32-byte topic identifier that serializes as a hex string (like `sp_core::Bytes`).
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
)]
pub struct Topic(pub [u8; 32]);

#[cfg(feature = "serde")]
impl serde::Serialize for Topic {
	fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		sp_core::bytes::serialize(&self.0, serializer)
	}
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Topic {
	fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let mut arr = [0u8; 32];
		sp_core::bytes::deserialize_check_len(
			deserializer,
			sp_core::bytes::ExpectedLen::Exact(&mut arr[..]),
		)?;
		Ok(Topic(arr))
	}
}

impl From<[u8; 32]> for Topic {
	fn from(inner: [u8; 32]) -> Self {
		Topic(inner)
	}
}

impl From<Topic> for [u8; 32] {
	fn from(topic: Topic) -> Self {
		topic.0
	}
}

impl AsRef<[u8; 32]> for Topic {
	fn as_ref(&self) -> &[u8; 32] {
		&self.0
	}
}

impl AsRef<[u8]> for Topic {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}

impl Deref for Topic {
	type Target = [u8; 32];

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

/// Decryption key identifier.
pub type DecryptionKey = [u8; 32];
/// Statement hash.
pub type Hash = [u8; 32];
/// Block hash.
pub type BlockHash = [u8; 32];
/// Account id
pub type AccountId = [u8; 32];
/// Identifier of a per-account communication channel, used for message replacement.
///
/// A channel is unique per `(account, channel)` pair: a new statement on an existing channel
/// replaces the previous one from the same account when it has a strictly higher expiry (see
/// [`Statement::channel`]). The 32 bytes are opaque to the store — it does not prescribe how a
/// channel id is generated. A statement with no channel is subject only to priority-based eviction.
pub type Channel = [u8; 32];

/// Total number of topic fields allowed in a statement and in `MatchAll` filters.
pub const MAX_TOPICS: usize = 4;
/// `MatchAny` allows to provide a list of topics match against. This is the maximum number of
/// topics allowed.
pub const MAX_ANY_TOPICS: usize = 128;

/// Per-account statement allowance: the resource budget an account may consume in the store.
///
/// The allowance is enforced on two axes at once — a maximum number of statements
/// ([`max_count`](Self::max_count)) and a maximum total data size in bytes
/// ([`max_size`](Self::max_size)). Because the binding constraint is primarily size, an account
/// may spend its budget as either a few large statements or many small ones, up to whichever
/// limit it hits first. When a submission would exceed either limit, the account's
/// lowest-priority statements are evicted to make room.
///
/// Allowances are not fixed in this crate: they are held in chain state under
/// [`STATEMENT_ALLOWANCE_PREFIX`] (keyed by [`statement_allowance_key`]) and granted or revoked by
/// the runtime via [`increase_allowance_by`] / [`decrease_allowance_by`]; the store reads the
/// current value with [`get_allowance`] when validating a submission. An account with no allowance
/// (or a depleted one) cannot store statements.
#[derive(Clone, Default, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo)]
pub struct StatementAllowance {
	/// Maximum number of statements allowed
	pub max_count: u32,
	/// Maximum total size of statements in bytes
	pub max_size: u32,
}

impl StatementAllowance {
	/// Create a new statement allowance.
	pub fn new(max_count: u32, max_size: u32) -> Self {
		Self { max_count, max_size }
	}

	/// Saturating addition of statement allowances.
	pub const fn saturating_add(self, rhs: StatementAllowance) -> StatementAllowance {
		StatementAllowance {
			max_count: self.max_count.saturating_add(rhs.max_count),
			max_size: self.max_size.saturating_add(rhs.max_size),
		}
	}

	/// Saturating subtraction of statement allowances.
	pub const fn saturating_sub(self, rhs: StatementAllowance) -> StatementAllowance {
		StatementAllowance {
			max_count: self.max_count.saturating_sub(rhs.max_count),
			max_size: self.max_size.saturating_sub(rhs.max_size),
		}
	}

	/// Returns `true` if the allowance is exhausted on either axis — that is, if `max_count` or
	/// `max_size` has reached zero.
	pub fn is_depleted(&self) -> bool {
		self.max_count == 0 || self.max_size == 0
	}
}

/// Storage key prefix for per-account statement allowances.
pub const STATEMENT_ALLOWANCE_PREFIX: &[u8] = b":statement_allowance:";

/// Constructs a per-account statement allowance storage key.
///
/// # Arguments
/// * `account_id` - Account identifier as byte slice
///
/// # Returns
/// Storage key: `":statement_allowance:" ++ account_id`
pub fn statement_allowance_key(account_id: impl AsRef<[u8]>) -> Vec<u8> {
	let mut key = STATEMENT_ALLOWANCE_PREFIX.to_vec();
	key.extend_from_slice(account_id.as_ref());
	key
}

/// Increase the statement allowance by the given amount.
pub fn increase_allowance_by(account_id: impl AsRef<[u8]>, by: StatementAllowance) {
	let key = statement_allowance_key(account_id);
	let mut allowance: StatementAllowance = frame_support::storage::unhashed::get_or_default(&key);
	allowance = allowance.saturating_add(by);
	frame_support::storage::unhashed::put(&key, &allowance);
}

/// Decrease the statement allowance by the given amount.
pub fn decrease_allowance_by(account_id: impl AsRef<[u8]>, by: StatementAllowance) {
	let key = statement_allowance_key(account_id);
	let mut allowance: StatementAllowance = frame_support::storage::unhashed::get_or_default(&key);
	allowance = allowance.saturating_sub(by);
	if allowance.is_depleted() {
		frame_support::storage::unhashed::kill(&key);
	} else {
		frame_support::storage::unhashed::put(&key, &allowance);
	}
}

/// Get the statement allowance for the given account.
pub fn get_allowance(account_id: impl AsRef<[u8]>) -> StatementAllowance {
	let key = statement_allowance_key(account_id);
	frame_support::storage::unhashed::get_or_default(&key)
}

pub use event::{
	AddFilterResponse, LimitReachedResult, LimitReachedTag, NewStatementEntry, SubscribeEvent,
};
#[cfg(feature = "std")]
pub use store_api::{
	AdmittedBatch, Error, FilterDecision, FilterId, InvalidReason, LiveStatementEvent,
	OptimizedTopicFilter, RejectionReason, Result, StatementEvent, StatementSource, StatementStore,
	SubmitInvalidReason, SubmitOutcome, SubmitRejectionReason, SubmitResult, TopicFilter,
};

#[cfg(feature = "std")]
mod ecies;
mod event;
pub mod runtime_api;
#[cfg(feature = "std")]
mod store_api;

mod sr25519 {
	mod app_sr25519 {
		use sp_application_crypto::{app_crypto, key_types::STATEMENT, sr25519};
		app_crypto!(sr25519, STATEMENT);
	}
	pub type Public = app_sr25519::Public;
}

/// Statement-store specific ed25519 crypto primitives.
pub mod ed25519 {
	mod app_ed25519 {
		use sp_application_crypto::{app_crypto, ed25519, key_types::STATEMENT};
		app_crypto!(ed25519, STATEMENT);
	}
	/// Statement-store specific ed25519 public key.
	pub type Public = app_ed25519::Public;
	/// Statement-store specific ed25519 key pair.
	#[cfg(feature = "std")]
	pub type Pair = app_ed25519::Pair;
}

mod ecdsa {
	mod app_ecdsa {
		use sp_application_crypto::{app_crypto, ecdsa, key_types::STATEMENT};
		app_crypto!(ecdsa, STATEMENT);
	}
	pub type Public = app_ecdsa::Public;
}

/// Returns blake2-256 hash for the encoded statement.
#[cfg(feature = "std")]
pub fn hash_encoded(data: &[u8]) -> [u8; 32] {
	sp_crypto_hashing::blake2_256(data)
}

/// Statement proof.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Debug, Clone, PartialEq, Eq,
)]
pub enum Proof {
	/// Sr25519 Signature.
	Sr25519 {
		/// Signature.
		signature: [u8; 64],
		/// Public key.
		signer: [u8; 32],
	},
	/// Ed25519 Signature.
	Ed25519 {
		/// Signature.
		signature: [u8; 64],
		/// Public key.
		signer: [u8; 32],
	},
	/// Secp256k1 Signature.
	Secp256k1Ecdsa {
		/// Signature.
		signature: [u8; 65],
		/// Public key.
		signer: [u8; 33],
	},
}

impl Proof {
	/// Return account id for the proof creator.
	pub fn account_id(&self) -> AccountId {
		match self {
			Proof::Sr25519 { signer, .. } => *signer,
			Proof::Ed25519 { signer, .. } => *signer,
			Proof::Secp256k1Ecdsa { signer, .. } => {
				<sp_runtime::traits::BlakeTwo256 as sp_core::Hasher>::hash(signer).into()
			},
		}
	}
}

/// Statement attributes. Each statement is a list of 0 or more fields. Fields may only appear once
/// and in the order declared here.
#[derive(Encode, Decode, TypeInfo, Debug, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum Field {
	/// Statement proof.
	AuthenticityProof(Proof) = 0,
	/// An identifier for the key that `Data` field may be decrypted with.
	DecryptionKey(DecryptionKey) = 1,
	/// Expiry of the statement. See [`Statement::expiry`] for details on the format.
	Expiry(u64) = 2,
	/// Account channel to use. Only one message per `(account, channel)` pair is allowed.
	Channel(Channel) = 3,
	/// First statement topic.
	Topic1(Topic) = 4,
	/// Second statement topic.
	Topic2(Topic) = 5,
	/// Third statement topic.
	Topic3(Topic) = 6,
	/// Fourth statement topic.
	Topic4(Topic) = 7,
	/// Additional data.
	Data(Vec<u8>) = 8,
}

impl Field {
	fn discriminant(&self) -> u8 {
		// This is safe for repr(u8)
		// see https://doc.rust-lang.org/reference/items/enumerations.html#pointer-casting
		unsafe { *(self as *const Self as *const u8) }
	}
}

/// Statement structure.
#[derive(DecodeWithMemTracking, Debug, Clone, PartialEq, Eq, Default)]
pub struct Statement {
	/// Proof used for authorizing the statement.
	proof: Option<Proof>,
	/// An identifier for the key that `Data` field may be decrypted with.
	#[deprecated(note = "Experimental feature, may be removed/changed in future releases")]
	decryption_key: Option<DecryptionKey>,
	/// Used for identifying a distinct communication channel, only a message per channel is
	/// stored.
	///
	/// This can be used to implement message replacement, submitting a new message with a
	/// different topic/data on the same channel and a greater expiry replaces the previous one.
	///
	/// If the new statement data is bigger than the old one, submitting a statement with the same
	/// channel does not guarantee that **ONLY** the old one will be replaced, as it might not fit
	/// in the account quota. In that case, other statements from the same account with the lowest
	/// expiry will be removed.
	channel: Option<Channel>,
	/// Message expiry, used for determining which statements to keep.
	///
	/// The most significant 32 bits represents the expiration timestamp (in seconds since
	/// UNIX epoch) after which the statement gets removed. These ensure that statements with a
	/// higher expiration time have a higher priority.
	/// The lower 32 bits represents an arbitrary sequence number used to order statements with the
	/// same expiration time.
	///
	/// Higher values indicate a higher priority.
	/// This is used in two cases:
	/// 1) When an account exceeds its quota and some statements need to be removed. Statements
	///    with the lowest `expiry` are removed first.
	/// 2) When multiple statements are submitted on the same channel, the one with the highest
	///    expiry replaces the one with the same channel.
	expiry: u64,
	/// Number of topics present.
	num_topics: u8,
	/// Topics, used for querying and filtering statements.
	topics: [Topic; MAX_TOPICS],
	/// Statement data.
	data: Option<Vec<u8>>,
}

/// Note: The `TypeInfo` implementation reflects the actual encoding format (`Vec<Field>`)
/// rather than the struct fields, since `Statement` has custom `Encode`/`Decode` implementations.
impl TypeInfo for Statement {
	type Identity = Self;

	fn type_info() -> Type {
		// Statement encodes as Vec<Field>, so we report the same type info
		Type::builder()
			.path(Path::new("Statement", module_path!()))
			.docs(&["Statement structure"])
			.composite(Fields::unnamed().field(|f| f.ty::<Vec<Field>>()))
	}
}

impl Decode for Statement {
	fn decode<I: codec::Input>(input: &mut I) -> core::result::Result<Self, codec::Error> {
		// Encoding matches that of Vec<Field>. Basically this just means accepting that there
		// will be a prefix of vector length.
		let num_fields: codec::Compact<u32> = Decode::decode(input)?;
		let mut tag = 0;
		let mut statement = Statement::new();
		for i in 0..num_fields.into() {
			let field: Field = Decode::decode(input)?;
			if i > 0 && field.discriminant() <= tag {
				return Err("Invalid field order or duplicate fields".into());
			}
			tag = field.discriminant();
			match field {
				Field::AuthenticityProof(p) => statement.set_proof(p),
				Field::DecryptionKey(key) => statement.set_decryption_key(key),
				Field::Expiry(p) => statement.set_expiry(p),
				Field::Channel(c) => statement.set_channel(c),
				Field::Topic1(t) => statement.set_topic(0, t),
				Field::Topic2(t) => statement.set_topic(1, t),
				Field::Topic3(t) => statement.set_topic(2, t),
				Field::Topic4(t) => statement.set_topic(3, t),
				Field::Data(data) => statement.set_plain_data(data),
			}
		}
		Ok(statement)
	}
}

impl Encode for Statement {
	fn encode(&self) -> Vec<u8> {
		self.encoded(false)
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// Result returned by `Statement::verify_signature`
pub enum SignatureVerificationResult {
	/// Signature is valid and matches this account id.
	Valid(AccountId),
	/// Signature has failed verification.
	Invalid,
	/// No signature in the proof or no proof.
	NoSignature,
}

impl Statement {
	/// Create a new empty statement with no proof.
	pub fn new() -> Statement {
		Default::default()
	}

	/// Create a new statement with a proof.
	pub fn new_with_proof(proof: Proof) -> Statement {
		let mut statement = Self::new();
		statement.set_proof(proof);
		statement
	}

	/// Sign with a key that matches given public key in the keystore.
	///
	/// Returns `true` if signing worked (private key present etc).
	///
	/// NOTE: This can only be called from the runtime.
	pub fn sign_sr25519_public(&mut self, key: &sr25519::Public) -> bool {
		let to_sign = self.signature_material();
		if let Some(signature) = key.sign(&to_sign) {
			let proof = Proof::Sr25519 {
				signature: signature.into_inner().into(),
				signer: key.clone().into_inner().into(),
			};
			self.set_proof(proof);
			true
		} else {
			false
		}
	}

	/// Returns slice of all topics set in the statement.
	pub fn topics(&self) -> &[Topic] {
		&self.topics[..self.num_topics as usize]
	}

	/// Sign with a given private key and add the signature proof field.
	#[cfg(feature = "std")]
	pub fn sign_sr25519_private(&mut self, key: &sp_core::sr25519::Pair) {
		let to_sign = self.signature_material();
		let proof =
			Proof::Sr25519 { signature: key.sign(&to_sign).into(), signer: key.public().into() };
		self.set_proof(proof);
	}

	/// Sign with a key that matches given public key in the keystore.
	///
	/// Returns `true` if signing worked (private key present etc).
	///
	/// NOTE: This can only be called from the runtime.
	pub fn sign_ed25519_public(&mut self, key: &ed25519::Public) -> bool {
		let to_sign = self.signature_material();
		if let Some(signature) = key.sign(&to_sign) {
			let proof = Proof::Ed25519 {
				signature: signature.into_inner().into(),
				signer: key.clone().into_inner().into(),
			};
			self.set_proof(proof);
			true
		} else {
			false
		}
	}

	/// Sign with a given private key and add the signature proof field.
	#[cfg(feature = "std")]
	pub fn sign_ed25519_private(&mut self, key: &sp_core::ed25519::Pair) {
		let to_sign = self.signature_material();
		let proof =
			Proof::Ed25519 { signature: key.sign(&to_sign).into(), signer: key.public().into() };
		self.set_proof(proof);
	}

	/// Sign with a key that matches given public key in the keystore.
	///
	/// Returns `true` if signing worked (private key present etc).
	///
	/// NOTE: This can only be called from the runtime.
	pub fn sign_ecdsa_public(&mut self, key: &ecdsa::Public) -> bool {
		let to_sign = self.signature_material();
		if let Some(signature) = key.sign(&to_sign) {
			let proof = Proof::Secp256k1Ecdsa {
				signature: signature.into_inner().into(),
				signer: key.clone().into_inner().0,
			};
			self.set_proof(proof);
			true
		} else {
			false
		}
	}

	/// Sign with a given private key and add the signature proof field.
	#[cfg(feature = "std")]
	pub fn sign_ecdsa_private(&mut self, key: &sp_core::ecdsa::Pair) {
		let to_sign = self.signature_material();
		let proof =
			Proof::Secp256k1Ecdsa { signature: key.sign(&to_sign).into(), signer: key.public().0 };
		self.set_proof(proof);
	}

	/// Verify the proof's signature over the statement's signature material (all fields except the
	/// proof itself).
	///
	/// Returns [`SignatureVerificationResult::NoSignature`] when there is no proof. On success the
	/// returned account is the signer for sr25519/ed25519, but for ECDSA it is the BLAKE2-256 hash
	/// of the signer key, not the key itself.
	pub fn verify_signature(&self) -> SignatureVerificationResult {
		use sp_runtime::traits::Verify;

		match self.proof() {
			None => SignatureVerificationResult::NoSignature,
			Some(Proof::Sr25519 { signature, signer }) => {
				let to_sign = self.signature_material();
				let signature = sp_core::sr25519::Signature::from(*signature);
				let public = sp_core::sr25519::Public::from(*signer);
				if signature.verify(to_sign.as_slice(), &public) {
					SignatureVerificationResult::Valid(*signer)
				} else {
					SignatureVerificationResult::Invalid
				}
			},
			Some(Proof::Ed25519 { signature, signer }) => {
				let to_sign = self.signature_material();
				let signature = sp_core::ed25519::Signature::from(*signature);
				let public = sp_core::ed25519::Public::from(*signer);
				if signature.verify(to_sign.as_slice(), &public) {
					SignatureVerificationResult::Valid(*signer)
				} else {
					SignatureVerificationResult::Invalid
				}
			},
			Some(Proof::Secp256k1Ecdsa { signature, signer }) => {
				let to_sign = self.signature_material();
				let signature = sp_core::ecdsa::Signature::from(*signature);
				let public = sp_core::ecdsa::Public::from(*signer);
				if signature.verify(to_sign.as_slice(), &public) {
					let sender_hash =
						<sp_runtime::traits::BlakeTwo256 as sp_core::Hasher>::hash(signer);
					SignatureVerificationResult::Valid(sender_hash.into())
				} else {
					SignatureVerificationResult::Invalid
				}
			},
		}
	}

	/// The statement's hash: the BLAKE2-256 hash of its SCALE encoding.
	///
	/// This is the statement's identity for deduplication and indexing across the store and
	/// network. It covers the full encoding (including the proof), so changing any field changes
	/// the hash.
	#[cfg(feature = "std")]
	pub fn hash(&self) -> [u8; 32] {
		self.using_encoded(hash_encoded)
	}

	/// Returns a topic by topic index.
	pub fn topic(&self, index: usize) -> Option<Topic> {
		if index < self.num_topics as usize {
			Some(self.topics[index])
		} else {
			None
		}
	}

	/// Returns decryption key if any.
	#[allow(deprecated)]
	pub fn decryption_key(&self) -> Option<DecryptionKey> {
		self.decryption_key
	}

	/// Consume the statement and return its data field (see [`data`](Self::data)).
	pub fn into_data(self) -> Option<Vec<u8>> {
		self.data
	}

	/// Get a reference to the statement proof, if any.
	pub fn proof(&self) -> Option<&Proof> {
		self.proof.as_ref()
	}

	/// Get proof account id, if any
	pub fn account_id(&self) -> Option<AccountId> {
		self.proof.as_ref().map(Proof::account_id)
	}

	/// Returns the statement's data field, if any. The bytes are plaintext when set via
	/// [`set_plain_data`](Self::set_plain_data) or ciphertext when set via
	/// [`encrypt`](Self::encrypt).
	pub fn data(&self) -> Option<&Vec<u8>> {
		self.data.as_ref()
	}

	/// Length in bytes of the statement's data field (`0` if absent).
	///
	/// This is the size the per-account quota ([`StatementAllowance::max_size`]) is measured
	/// against — the data length, not the full SCALE-encoded statement size.
	pub fn data_len(&self) -> usize {
		self.data().map_or(0, Vec::len)
	}

	/// Get channel, if any.
	pub fn channel(&self) -> Option<Channel> {
		self.channel
	}

	/// Get expiry.
	pub fn expiry(&self) -> u64 {
		self.expiry
	}

	/// Get expiration timestamp in seconds.
	///
	/// The expiration timestamp in seconds is stored in the most significant 32 bits of the expiry
	/// field.
	pub fn get_expiration_timestamp_secs(&self) -> u32 {
		(self.expiry >> 32) as u32
	}

	/// Whether the statement has expired at `now_secs`, a unix timestamp in seconds.
	///
	/// Expired once `now_secs` reaches the expiration timestamp.
	pub fn is_expired(&self, now_secs: u64) -> bool {
		now_secs >= u64::from(self.get_expiration_timestamp_secs())
	}

	/// Return encoded fields that can be signed to construct or verify a proof
	fn signature_material(&self) -> Vec<u8> {
		self.encoded(true)
	}

	/// Remove the proof of this statement.
	pub fn remove_proof(&mut self) {
		self.proof = None;
	}

	/// Set statement proof. Any existing proof is overwritten.
	pub fn set_proof(&mut self, proof: Proof) {
		self.proof = Some(proof)
	}

	/// Set statement expiry.
	pub fn set_expiry(&mut self, expiry: u64) {
		self.expiry = expiry;
	}

	/// Set statement expiry from its parts. See [`Statement::expiry`] for details on the format.
	pub fn set_expiry_from_parts(&mut self, expiration_timestamp_secs: u32, sequence_number: u32) {
		self.expiry = (expiration_timestamp_secs as u64) << 32 | sequence_number as u64;
	}

	/// Set statement channel.
	pub fn set_channel(&mut self, channel: Channel) {
		self.channel = Some(channel)
	}

	/// Set topic by index. Does nothing if `index` is at or beyond [`MAX_TOPICS`].
	///
	/// Grows the statement's topic count so that `index` becomes addressable.
	pub fn set_topic(&mut self, index: usize, topic: Topic) {
		if index < MAX_TOPICS {
			self.topics[index] = topic;
			self.num_topics = self.num_topics.max(index as u8 + 1);
		}
	}

	/// Set decryption key.
	#[allow(deprecated)]
	pub fn set_decryption_key(&mut self, key: DecryptionKey) {
		self.decryption_key = Some(key);
	}

	/// Set unencrypted statement data.
	pub fn set_plain_data(&mut self, data: Vec<u8>) {
		self.data = Some(data)
	}

	/// Estimate the encoded size for preallocation.
	///
	/// Returns a close approximation of the SCALE-encoded size without actually performing the
	/// encoding. Uses max_encoded_len() for type sizes:
	/// - Compact length prefix: max_encoded_len() bytes
	/// - Proof field: 1 (tag) + max_encoded_len()
	/// - DecryptionKey: 1 (tag) + max_encoded_len()
	/// - Expiry: 1 (tag) + max_encoded_len()
	/// - Channel: 1 (tag) + max_encoded_len()
	/// - Each topic: 1 (tag) + max_encoded_len()
	/// - Data: 1 (tag) + max_encoded_len() (compact len) + data.len()
	#[allow(deprecated)]
	fn estimated_encoded_size(&self, for_signing: bool) -> usize {
		let proof_size =
			if !for_signing && self.proof.is_some() { 1 + Proof::max_encoded_len() } else { 0 };
		let decryption_key_size =
			if self.decryption_key.is_some() { 1 + DecryptionKey::max_encoded_len() } else { 0 };
		let expiry_size = 1 + u64::max_encoded_len();
		let channel_size = if self.channel.is_some() { 1 + Channel::max_encoded_len() } else { 0 };
		let topics_size = self.num_topics as usize * (1 + Topic::max_encoded_len());
		let data_size = self
			.data
			.as_ref()
			.map_or(0, |d| 1 + Compact::<u32>::max_encoded_len() + d.len());
		let compact_prefix_size = if !for_signing { Compact::<u32>::max_encoded_len() } else { 0 };

		compact_prefix_size +
			proof_size +
			decryption_key_size +
			expiry_size +
			channel_size +
			topics_size +
			data_size
	}

	#[allow(deprecated)]
	fn encoded(&self, for_signing: bool) -> Vec<u8> {
		// Encoding matches that of Vec<Field>. Basically this just means accepting that there
		// will be a prefix of vector length.
		// Expiry field is always present.
		let num_fields = if !for_signing && self.proof.is_some() { 2 } else { 1 } +
			if self.decryption_key.is_some() { 1 } else { 0 } +
			if self.channel.is_some() { 1 } else { 0 } +
			if self.data.is_some() { 1 } else { 0 } +
			self.num_topics as u32;

		let mut output = Vec::with_capacity(self.estimated_encoded_size(for_signing));
		// When encoding signature payload, the length prefix is omitted.
		// This is so that the signature for encoded statement can potentially be derived without
		// needing to re-encode the statement.
		if !for_signing {
			let compact_len = codec::Compact::<u32>(num_fields);
			compact_len.encode_to(&mut output);

			if let Some(proof) = &self.proof {
				0u8.encode_to(&mut output);
				proof.encode_to(&mut output);
			}
		}
		if let Some(decryption_key) = &self.decryption_key {
			1u8.encode_to(&mut output);
			decryption_key.encode_to(&mut output);
		}

		2u8.encode_to(&mut output);
		self.expiry().encode_to(&mut output);

		if let Some(channel) = &self.channel {
			3u8.encode_to(&mut output);
			channel.encode_to(&mut output);
		}
		for t in 0..self.num_topics {
			(4u8 + t).encode_to(&mut output);
			self.topics[t as usize].encode_to(&mut output);
		}
		if let Some(data) = &self.data {
			8u8.encode_to(&mut output);
			data.encode_to(&mut output);
		}
		output
	}

	/// Encrypt `data` to `key` (ECIES) and store both the ciphertext and the matching decryption
	/// key on the statement.
	///
	/// Note: encryption is experimental (the decryption-key field is deprecated).
	#[allow(deprecated)]
	#[cfg(feature = "std")]
	pub fn encrypt(
		&mut self,
		data: &[u8],
		key: &sp_core::ed25519::Public,
	) -> core::result::Result<(), ecies::Error> {
		let encrypted = ecies::encrypt_ed25519(key, data)?;
		self.data = Some(encrypted);
		self.decryption_key = Some((*key).into());
		Ok(())
	}

	/// Decrypt the statement's data with the given private key (ECIES).
	///
	/// Returns `Ok(None)` if the statement has no data; errors if the data was not encrypted to
	/// this key.
	#[cfg(feature = "std")]
	pub fn decrypt_private(
		&self,
		key: &sp_core::ed25519::Pair,
	) -> core::result::Result<Option<Vec<u8>>, ecies::Error> {
		self.data.as_ref().map(|d| ecies::decrypt_ed25519(key, d)).transpose()
	}
}

#[cfg(test)]
mod test {
	use crate::{
		hash_encoded, Field, Proof, SignatureVerificationResult, Statement, Topic, MAX_TOPICS,
	};
	use codec::{Decode, Encode, MaxEncodedLen};
	use scale_info::{MetaType, TypeInfo};
	use sp_application_crypto::Pair;
	use sp_core::sr25519;

	#[test]
	fn statement_encoding_matches_vec() {
		let mut statement = Statement::new();
		assert!(statement.proof().is_none());
		let proof = Proof::Sr25519 { signature: [42u8; 64], signer: [24u8; 32] };

		let decryption_key = [0xde; 32];
		let topic1: Topic = [0x01; 32].into();
		let topic2: Topic = [0x02; 32].into();
		let data = vec![55, 99];
		let expiry = 999;
		let channel = [0xcc; 32];

		statement.set_proof(proof.clone());
		statement.set_decryption_key(decryption_key);
		statement.set_expiry(expiry);
		statement.set_channel(channel);
		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);
		statement.set_plain_data(data.clone());

		statement.set_topic(5, [0x55; 32].into());
		assert_eq!(statement.topic(5), None);

		let fields = vec![
			Field::AuthenticityProof(proof.clone()),
			Field::DecryptionKey(decryption_key),
			Field::Expiry(expiry),
			Field::Channel(channel),
			Field::Topic1(topic1),
			Field::Topic2(topic2),
			Field::Data(data.clone()),
		];

		let encoded = statement.encode();
		assert_eq!(statement.hash(), hash_encoded(&encoded));
		assert_eq!(encoded, fields.encode());

		let decoded = Statement::decode(&mut encoded.as_slice()).unwrap();
		assert_eq!(decoded, statement);
	}

	#[test]
	fn decode_checks_fields() {
		let topic1: Topic = [0x01; 32].into();
		let topic2: Topic = [0x02; 32].into();
		let priority = 999;

		let dup_topic1 = vec![
			Field::Expiry(priority),
			Field::Topic1(topic1),
			Field::Topic1(topic1),
			Field::Topic2(topic2),
		]
		.encode();
		assert!(Statement::decode(&mut dup_topic1.as_slice()).is_err());

		let topic1_before_expiry =
			vec![Field::Topic1(topic1), Field::Expiry(priority), Field::Topic2(topic2)].encode();
		assert!(Statement::decode(&mut topic1_before_expiry.as_slice()).is_err());

		let dup_expiry = vec![Field::Expiry(1), Field::Expiry(2)].encode();
		assert!(Statement::decode(&mut dup_expiry.as_slice()).is_err());

		let dup_data = vec![Field::Data(vec![1]), Field::Data(vec![2])].encode();
		assert!(Statement::decode(&mut dup_data.as_slice()).is_err());

		let data_before_expiry = vec![Field::Data(vec![1]), Field::Expiry(42)].encode();
		assert!(Statement::decode(&mut data_before_expiry.as_slice()).is_err());

		let channel_before_expiry = vec![Field::Channel([0; 32]), Field::Expiry(1)].encode();
		assert!(Statement::decode(&mut channel_before_expiry.as_slice()).is_err());

		let topic2_before_topic1 =
			vec![Field::Expiry(1), Field::Topic2(topic1), Field::Topic1(topic2)].encode();
		assert!(Statement::decode(&mut topic2_before_topic1.as_slice()).is_err());
	}

	#[test]
	fn decode_rejects_malformed_bytes() {
		assert!(Statement::decode(&mut &[][..]).is_err());

		// Take a valid encoded statement and corrupt it in different ways
		let valid = vec![Field::Expiry(42)].encode();
		let decoded = Statement::decode(&mut valid.as_slice()).unwrap();
		assert_eq!(decoded.expiry(), 42);

		// Truncate to just the length prefix
		assert!(Statement::decode(&mut &valid[..1][..]).is_err());

		// Replace field discriminant with invalid value (Field only has 0..=8)
		let mut invalid_discriminant = valid.clone();
		invalid_discriminant[1] = 9;
		assert!(Statement::decode(&mut invalid_discriminant.as_slice()).is_err());

		invalid_discriminant[1] = 255;
		assert!(Statement::decode(&mut invalid_discriminant.as_slice()).is_err());

		// Truncate the Expiry payload (need 8 bytes for u64, provide fewer)
		assert!(Statement::decode(&mut &valid[..5][..]).is_err());

		// Encode a statement with Proof, then corrupt the Proof variant
		let with_proof = vec![
			Field::AuthenticityProof(Proof::Sr25519 { signature: [0u8; 64], signer: [0u8; 32] }),
			Field::Expiry(42),
		]
		.encode();
		assert!(Statement::decode(&mut with_proof.as_slice()).is_ok());

		let mut invalid_proof_variant = with_proof.clone();
		invalid_proof_variant[2] = 99;
		assert!(Statement::decode(&mut invalid_proof_variant.as_slice()).is_err());

		// Truncate the Proof payload
		assert!(Statement::decode(&mut &with_proof[..6][..]).is_err());

		// Claim more fields than actually present
		let mut inflated_count = valid.clone();
		inflated_count[0] = 5 << 2; // change field count from 1 to 5
		assert!(Statement::decode(&mut inflated_count.as_slice()).is_err());
	}

	#[test]
	fn sign_and_verify() {
		let mut statement = Statement::new();
		statement.set_plain_data(vec![42]);

		let sr25519_kp = sp_core::sr25519::Pair::from_string("//Alice", None).unwrap();
		let ed25519_kp = sp_core::ed25519::Pair::from_string("//Alice", None).unwrap();
		let secp256k1_kp = sp_core::ecdsa::Pair::from_string("//Alice", None).unwrap();

		statement.sign_sr25519_private(&sr25519_kp);
		assert_eq!(
			statement.verify_signature(),
			SignatureVerificationResult::Valid(sr25519_kp.public().0)
		);

		statement.sign_ed25519_private(&ed25519_kp);
		assert_eq!(
			statement.verify_signature(),
			SignatureVerificationResult::Valid(ed25519_kp.public().0)
		);

		statement.sign_ecdsa_private(&secp256k1_kp);
		assert_eq!(
			statement.verify_signature(),
			SignatureVerificationResult::Valid(sp_crypto_hashing::blake2_256(
				&secp256k1_kp.public().0
			))
		);

		// set an invalid Sr25519 signature
		statement.set_proof(Proof::Sr25519 { signature: [0u8; 64], signer: [0u8; 32] });
		assert_eq!(statement.verify_signature(), SignatureVerificationResult::Invalid);

		// set an invalid Ed25519 signature
		statement.set_proof(Proof::Ed25519 { signature: [0xAB; 64], signer: [0xCD; 32] });
		assert_eq!(statement.verify_signature(), SignatureVerificationResult::Invalid);

		// set an invalid Secp256k1Ecdsa signature
		statement.set_proof(Proof::Secp256k1Ecdsa { signature: [0u8; 65], signer: [0u8; 33] });
		assert_eq!(statement.verify_signature(), SignatureVerificationResult::Invalid);

		statement.remove_proof();
		assert_eq!(statement.verify_signature(), SignatureVerificationResult::NoSignature);
	}

	#[test]
	fn encrypt_decrypt() {
		let mut statement = Statement::new();
		let (pair, _) = sp_core::ed25519::Pair::generate();
		let plain = b"test data".to_vec();

		// let sr25519_kp = sp_core::sr25519::Pair::from_string("//Alice", None).unwrap();
		statement.encrypt(&plain, &pair.public()).unwrap();
		assert_ne!(plain.as_slice(), statement.data().unwrap().as_slice());

		let decrypted = statement.decrypt_private(&pair).unwrap();
		assert_eq!(decrypted, Some(plain));
	}

	#[test]
	fn check_matches() {
		let mut statement = Statement::new();
		let topic1: Topic = [0x01; 32].into();
		let topic2: Topic = [0x02; 32].into();
		let topic3: Topic = [0x03; 32].into();

		statement.set_topic(0, topic1);
		statement.set_topic(1, topic2);

		let filter_any = crate::OptimizedTopicFilter::Any;
		assert!(filter_any.matches(&statement));

		let filter_all =
			crate::OptimizedTopicFilter::MatchAll([topic1, topic2].iter().cloned().collect());
		assert!(filter_all.matches(&statement));

		let filter_all_fail =
			crate::OptimizedTopicFilter::MatchAll([topic1, topic3].iter().cloned().collect());
		assert!(!filter_all_fail.matches(&statement));

		let filter_any_match =
			crate::OptimizedTopicFilter::MatchAny([topic2, topic3].iter().cloned().collect());
		assert!(filter_any_match.matches(&statement));

		let filter_any_fail =
			crate::OptimizedTopicFilter::MatchAny([topic3].iter().cloned().collect());
		assert!(!filter_any_fail.matches(&statement));
	}

	#[test]
	fn statement_type_info_matches_encoding() {
		// Statement has custom Encode/Decode that encodes as Vec<Field>.
		// Verify that TypeInfo reflects this by containing a reference to Vec<Field>.
		let statement_type = Statement::type_info();
		let vec_field_meta = MetaType::new::<Vec<Field>>();

		// The Statement type should be a composite with one unnamed field of type Vec<Field>
		match statement_type.type_def {
			scale_info::TypeDef::Composite(composite) => {
				assert_eq!(composite.fields.len(), 1, "Statement should have exactly one field");
				let field = &composite.fields[0];
				assert!(field.name.is_none(), "Field should be unnamed (newtype pattern)");
				assert_eq!(field.ty, vec_field_meta, "Statement's inner type should be Vec<Field>");
			},
			_ => panic!("Statement TypeInfo should be a Composite"),
		}
	}

	#[test]
	fn measure_hash_30_000_statements() {
		use std::time::Instant;
		const NUM_STATEMENTS: usize = 30_000;
		let (keyring, _) = sr25519::Pair::generate();

		// Create 2000 statements with varying data
		let statements: Vec<Statement> = (0..NUM_STATEMENTS)
			.map(|i| {
				let mut statement = Statement::new();

				statement.set_expiry(i as u64);
				statement.set_topic(0, [(i % 256) as u8; 32].into());
				statement.set_plain_data(vec![i as u8; 512]);
				statement.sign_sr25519_private(&keyring);

				statement.sign_sr25519_private(&keyring);
				statement
			})
			.collect();
		// Measure time to hash all statements
		let start = Instant::now();
		let hashes: Vec<[u8; 32]> = statements.iter().map(|s| s.hash()).collect();
		let elapsed = start.elapsed();
		println!("Time to hash {} statements: {:?}", NUM_STATEMENTS, elapsed);
		println!("Average time per statement: {:?}", elapsed / NUM_STATEMENTS as u32);
		// Verify hashes are unique
		let unique_hashes: std::collections::HashSet<_> = hashes.iter().collect();
		assert_eq!(unique_hashes.len(), NUM_STATEMENTS);
	}

	#[test]
	fn estimated_encoded_size_is_sufficient() {
		// Allow some overhead due to using max_encoded_len() approximations.
		const MAX_ACCEPTED_OVERHEAD: usize = 33;

		// Use Secp256k1Ecdsa: with sig=65 + signer=33 bytes, it is the worst-case proof payload
		let proof = Proof::Secp256k1Ecdsa { signature: [42u8; 65], signer: [24u8; 33] };
		let decryption_key = [0xde; 32];
		let data = vec![55; 1000];
		let expiry = 999;
		let channel = [0xcc; 32];

		// Test with all fields populated
		let mut statement = Statement::new();
		statement.set_proof(proof);
		statement.set_decryption_key(decryption_key);
		statement.set_expiry(expiry);
		statement.set_channel(channel);
		for i in 0..MAX_TOPICS {
			statement.set_topic(i, [i as u8; 32].into());
		}
		statement.set_plain_data(data);

		let encoded = statement.encode();
		let estimated = statement.estimated_encoded_size(false);
		assert!(
			estimated >= encoded.len(),
			"estimated_encoded_size ({}) should be >= actual encoded length ({})",
			estimated,
			encoded.len()
		);
		let overhead = estimated - encoded.len();
		assert!(
			overhead <= MAX_ACCEPTED_OVERHEAD,
			"estimated overhead ({}) should be small, estimated: {}, actual: {}",
			overhead,
			estimated,
			encoded.len()
		);

		// Test for_signing = true (no proof, no compact prefix)
		let signing_payload = statement.encoded(true);
		let signing_estimated = statement.estimated_encoded_size(true);
		assert!(
			signing_estimated >= signing_payload.len(),
			"estimated_encoded_size for signing ({}) should be >= actual signing payload length ({})",
			signing_estimated,
			signing_payload.len()
		);
		let signing_overhead = signing_estimated - signing_payload.len();
		assert!(
			signing_overhead <= MAX_ACCEPTED_OVERHEAD,
			"signing overhead ({}) should be small, estimated: {}, actual: {}",
			signing_overhead,
			signing_estimated,
			signing_payload.len()
		);

		// Test with minimal statement (empty)
		let empty_statement = Statement::new();
		let empty_encoded = empty_statement.encode();
		let empty_estimated = empty_statement.estimated_encoded_size(false);
		assert!(
			empty_estimated >= empty_encoded.len(),
			"estimated_encoded_size for empty ({}) should be >= actual encoded length ({})",
			empty_estimated,
			empty_encoded.len()
		);
		let empty_overhead = empty_estimated - empty_encoded.len();
		assert!(
			empty_overhead <= MAX_ACCEPTED_OVERHEAD,
			"empty overhead ({}) should be minimal, estimated: {}, actual: {}",
			empty_overhead,
			empty_estimated,
			empty_encoded.len()
		);
	}

	// Wire-format regression tests.
	//
	// `Proof::OnChain` was removed in favour of cryptographic-only proofs.
	// These tests pin the SCALE encoding of the surviving variants so that any future reordering,
	// renaming, or payload change is caught immediately.

	/// Canonical fixture: a `Statement` with three topics, a channel, an expiry,
	/// and a 4-byte payload. Used by every wire-format test in this section.
	fn populate_canonical_fixture(stmt: &mut Statement) {
		stmt.set_topic(0, [0x01; 32].into());
		stmt.set_topic(1, [0x02; 32].into());
		stmt.set_topic(2, [0x03; 32].into());
		stmt.set_channel([0xcc; 32]);
		stmt.set_expiry_from_parts(0x7fff_ffff, 0xabcd_1234);
		stmt.set_plain_data(vec![0xde, 0xad, 0xbe, 0xef]);
	}

	/// The "tail" of every canonical fixture: everything after the optional
	/// `AuthenticityProof` field. Pulled out so each variant fixture is one
	/// short, reviewable block.
	fn canonical_tail() -> Vec<u8> {
		let mut v = Vec::new();
		v.push(0x02); // Field::Expiry discriminant
		v.extend_from_slice(&[0x34, 0x12, 0xcd, 0xab, 0xff, 0xff, 0xff, 0x7f]); // u64 LE
		v.push(0x03); // Field::Channel discriminant
		v.extend_from_slice(&[0xcc; 32]);
		v.push(0x04); // Field::Topic1 discriminant
		v.extend_from_slice(&[0x01; 32]);
		v.push(0x05); // Field::Topic2 discriminant
		v.extend_from_slice(&[0x02; 32]);
		v.push(0x06); // Field::Topic3 discriminant
		v.extend_from_slice(&[0x03; 32]);
		v.push(0x08); // Field::Data discriminant
		v.push(0x10); // Compact<u32> = 4
		v.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
		v
	}

	/// Pinned byte fixture for `Proof::Sr25519` statements.
	#[test]
	fn wire_format_sr25519_pinned() {
		let mut stmt = Statement::new();
		populate_canonical_fixture(&mut stmt);
		stmt.set_proof(Proof::Sr25519 { signature: [0x11; 64], signer: [0xAA; 32] });

		let mut expected = Vec::new();
		expected.push(0x1c); // Compact<u32> = 7 fields (7 << 2)
		expected.push(0x00); // Field::AuthenticityProof discriminant
		expected.push(0x00); // Proof::Sr25519 discriminant
		expected.extend_from_slice(&[0x11; 64]); // signature
		expected.extend_from_slice(&[0xAA; 32]); // signer
		expected.extend(canonical_tail());

		assert_eq!(stmt.encode(), expected, "Sr25519 wire format drifted");
		assert_eq!(expected.len(), 246);
		// Round-trip
		assert_eq!(Statement::decode(&mut expected.as_slice()).unwrap(), stmt);
	}

	/// `Proof::OnChain` byte sequences are rejected on decode.
	#[test]
	fn wire_format_legacy_onchain_proof_is_rejected() {
		// Hand-crafted SCALE: 2 fields = [AuthenticityProof(OnChain { ... }), Expiry].
		let mut legacy = Vec::new();
		legacy.push(0x08); // Compact<u32> = 2 fields
		legacy.push(0x00); // Field::AuthenticityProof discriminant
		legacy.push(0x03); // Proof variant discriminant 3 (the old OnChain slot)
		legacy.extend_from_slice(&[0xdd; 32]); // who
		legacy.extend_from_slice(&[0xee; 32]); // block_hash
		legacy.extend_from_slice(&[0xbe, 0xba, 0xfe, 0xca, 0xef, 0xbe, 0xad, 0xde]); // event_index
		legacy.push(0x02); // Field::Expiry
		legacy.extend_from_slice(&[0x2a, 0, 0, 0, 0, 0, 0, 0]); // 42 as u64 LE

		assert!(
			Statement::decode(&mut legacy.as_slice()).is_err(),
			"legacy OnChain bytes must no longer decode into a Statement",
		);

		// And the same payload with the discriminant moved into the survivor
		// range still works — proving the rejection is specifically about the
		// removed slot, not a wholesale break of the codec.
		let mut survivor = Vec::new();
		survivor.push(0x08);
		survivor.push(0x00);
		survivor.push(0x00); // Proof::Sr25519 — survivor variant
		survivor.extend_from_slice(&[0xdd; 64]);
		survivor.extend_from_slice(&[0xee; 32]);
		survivor.push(0x02);
		survivor.extend_from_slice(&[0x2a, 0, 0, 0, 0, 0, 0, 0]);
		assert!(
			Statement::decode(&mut survivor.as_slice()).is_ok(),
			"surviving variant in the same byte layout must still decode",
		);
	}

	/// `Proof::max_encoded_len()` reflects the three-variant enum.
	#[test]
	fn proof_max_encoded_len_after_onchain_removal() {
		assert_eq!(
			Proof::max_encoded_len(),
			1 + 65 + 33,
			"max_encoded_len must equal Secp256k1Ecdsa's payload + 1-byte discriminant",
		);
	}
}
