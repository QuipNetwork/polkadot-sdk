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

//! Primitives for GRANDPA integration, suitable for WASM compilation.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "serde")]
use serde::Serialize;

use alloc::vec::Vec;
use codec::{Codec, Decode, DecodeWithMemTracking, Encode};
use scale_info::TypeInfo;
#[cfg(feature = "std")]
use sp_keystore::KeystorePtr;
use sp_runtime::{
	traits::{Header as HeaderT, NumberFor},
	ConsensusEngineId, Debug, OpaqueValue,
};

/// The log target to be used by client code.
pub const CLIENT_LOG_TARGET: &str = "grandpa";
/// The log target to be used by runtime code.
pub const RUNTIME_LOG_TARGET: &str = "runtime::grandpa";

/// Key type for GRANDPA module.
pub const KEY_TYPE: sp_core::crypto::KeyTypeId = sp_application_crypto::key_types::GRANDPA;

mod app {
	use quip_crypto_primitives::substrate::ed25519_mldsa44 as hybrid;
	use sp_application_crypto::{app_crypto, key_types::GRANDPA};

	app_crypto!(hybrid, GRANDPA);
}

sp_application_crypto::with_pair! {
	/// The grandpa crypto scheme defined via the keypair type.
	pub type AuthorityPair = app::Pair;
}

/// Identity of a Grandpa authority.
pub type AuthorityId = app::Public;

/// Signature for a Grandpa authority.
pub type AuthoritySignature = app::Signature;

/// The `ConsensusEngineId` of GRANDPA.
pub const GRANDPA_ENGINE_ID: ConsensusEngineId = *b"FRNK";

/// The weight of an authority.
pub type AuthorityWeight = u64;

/// The index of an authority.
pub type AuthorityIndex = u64;

/// The monotonic identifier of a GRANDPA set of authorities.
pub type SetId = u64;

/// The round indicator.
pub type RoundNumber = u64;

/// A list of Grandpa authorities with associated weights.
pub type AuthorityList = Vec<(AuthorityId, AuthorityWeight)>;

/// A GRANDPA message for a substrate chain.
pub type Message<Header> =
	finality_grandpa::Message<<Header as HeaderT>::Hash, <Header as HeaderT>::Number>;

/// A signed message.
pub type SignedMessage<Header> = finality_grandpa::SignedMessage<
	<Header as HeaderT>::Hash,
	<Header as HeaderT>::Number,
	AuthoritySignature,
	AuthorityId,
>;

/// A primary propose message for this chain's block type.
pub type PrimaryPropose<Header> =
	finality_grandpa::PrimaryPropose<<Header as HeaderT>::Hash, <Header as HeaderT>::Number>;
/// A prevote message for this chain's block type.
pub type Prevote<Header> =
	finality_grandpa::Prevote<<Header as HeaderT>::Hash, <Header as HeaderT>::Number>;
/// A precommit message for this chain's block type.
pub type Precommit<Header> =
	finality_grandpa::Precommit<<Header as HeaderT>::Hash, <Header as HeaderT>::Number>;
/// A catch up message for this chain's block type.
pub type CatchUp<Header> = finality_grandpa::CatchUp<
	<Header as HeaderT>::Hash,
	<Header as HeaderT>::Number,
	AuthoritySignature,
	AuthorityId,
>;
/// A commit message for this chain's block type.
pub type Commit<Header> = finality_grandpa::Commit<
	<Header as HeaderT>::Hash,
	<Header as HeaderT>::Number,
	AuthoritySignature,
	AuthorityId,
>;

/// A compact commit message for this chain's block type.
pub type CompactCommit<Header> = finality_grandpa::CompactCommit<
	<Header as HeaderT>::Hash,
	<Header as HeaderT>::Number,
	AuthoritySignature,
	AuthorityId,
>;

/// A GRANDPA justification for block finality, it includes a commit message and
/// an ancestry proof including all headers routing all precommit target blocks
/// to the commit target block. Due to the current voting strategy the precommit
/// targets should be the same as the commit target, since honest voters don't
/// vote past authority set change blocks.
///
/// This is meant to be stored in the db and passed around the network to other
/// nodes, and are used by syncing nodes to prove authority set handoffs.
#[derive(Clone, Encode, Decode, PartialEq, Eq, TypeInfo)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct GrandpaJustification<Header: HeaderT> {
	pub round: u64,
	pub commit: Commit<Header>,
	pub votes_ancestries: Vec<Header>,
}

/// A scheduled change of authority set.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ScheduledChange<N> {
	/// The new authorities after the change, along with their respective weights.
	pub next_authorities: AuthorityList,
	/// The number of blocks to delay.
	pub delay: N,
}

/// An consensus log item for GRANDPA.
#[derive(Decode, Encode, PartialEq, Eq, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ConsensusLog<N: Codec> {
	/// Schedule an authority set change.
	///
	/// The earliest digest of this type in a single block will be respected,
	/// provided that there is no `ForcedChange` digest. If there is, then the
	/// `ForcedChange` will take precedence.
	///
	/// No change should be scheduled if one is already and the delay has not
	/// passed completely.
	///
	/// This should be a pure function: i.e. as long as the runtime can interpret
	/// the digest type it should return the same result regardless of the current
	/// state.
	#[codec(index = 1)]
	ScheduledChange(ScheduledChange<N>),
	/// Force an authority set change.
	///
	/// Forced changes are applied after a delay of _imported_ blocks,
	/// while pending changes are applied after a delay of _finalized_ blocks.
	///
	/// The earliest digest of this type in a single block will be respected,
	/// with others ignored.
	///
	/// No change should be scheduled if one is already and the delay has not
	/// passed completely.
	///
	/// This should be a pure function: i.e. as long as the runtime can interpret
	/// the digest type it should return the same result regardless of the current
	/// state.
	#[codec(index = 2)]
	ForcedChange(N, ScheduledChange<N>),
	/// Note that the authority with given index is disabled until the next change.
	#[codec(index = 3)]
	OnDisabled(AuthorityIndex),
	/// A signal to pause the current authority set after the given delay.
	/// After finalizing the block at _delay_ the authorities should stop voting.
	#[codec(index = 4)]
	Pause(N),
	/// A signal to resume the current authority set after the given delay.
	/// After authoring the block at _delay_ the authorities should resume voting.
	#[codec(index = 5)]
	Resume(N),
}

impl<N: Codec> ConsensusLog<N> {
	/// Try to cast the log entry as a contained signal.
	pub fn try_into_change(self) -> Option<ScheduledChange<N>> {
		match self {
			ConsensusLog::ScheduledChange(change) => Some(change),
			_ => None,
		}
	}

	/// Try to cast the log entry as a contained forced signal.
	pub fn try_into_forced_change(self) -> Option<(N, ScheduledChange<N>)> {
		match self {
			ConsensusLog::ForcedChange(median, change) => Some((median, change)),
			_ => None,
		}
	}

	/// Try to cast the log entry as a contained pause signal.
	pub fn try_into_pause(self) -> Option<N> {
		match self {
			ConsensusLog::Pause(delay) => Some(delay),
			_ => None,
		}
	}

	/// Try to cast the log entry as a contained resume signal.
	pub fn try_into_resume(self) -> Option<N> {
		match self {
			ConsensusLog::Resume(delay) => Some(delay),
			_ => None,
		}
	}
}

/// Proof of voter misbehavior on a given set id. Misbehavior/equivocation in
/// GRANDPA happens when a voter votes on the same round (either at prevote or
/// precommit stage) for different blocks. Proving is achieved by collecting the
/// signed messages of conflicting votes.
#[derive(Clone, Debug, Decode, DecodeWithMemTracking, Encode, PartialEq, Eq, TypeInfo)]
pub struct EquivocationProof<H, N> {
	set_id: SetId,
	equivocation: Equivocation<H, N>,
}

impl<H, N> EquivocationProof<H, N> {
	/// Create a new `EquivocationProof` for the given set id and using the
	/// given equivocation as proof.
	pub fn new(set_id: SetId, equivocation: Equivocation<H, N>) -> Self {
		EquivocationProof { set_id, equivocation }
	}

	/// Returns the set id at which the equivocation occurred.
	pub fn set_id(&self) -> SetId {
		self.set_id
	}

	/// Returns the round number at which the equivocation occurred.
	pub fn round(&self) -> RoundNumber {
		match self.equivocation {
			Equivocation::Prevote(ref equivocation) => equivocation.round_number,
			Equivocation::Precommit(ref equivocation) => equivocation.round_number,
		}
	}

	/// Returns the authority id of the equivocator.
	pub fn offender(&self) -> &AuthorityId {
		self.equivocation.offender()
	}
}

/// Wrapper object for GRANDPA equivocation proofs, useful for unifying prevote
/// and precommit equivocations under a common type.
#[derive(Clone, Debug, Decode, DecodeWithMemTracking, Encode, PartialEq, Eq, TypeInfo)]
pub enum Equivocation<H, N> {
	/// Proof of equivocation at prevote stage.
	Prevote(
		finality_grandpa::Equivocation<
			AuthorityId,
			finality_grandpa::Prevote<H, N>,
			AuthoritySignature,
		>,
	),
	/// Proof of equivocation at precommit stage.
	Precommit(
		finality_grandpa::Equivocation<
			AuthorityId,
			finality_grandpa::Precommit<H, N>,
			AuthoritySignature,
		>,
	),
}

impl<H, N>
	From<
		finality_grandpa::Equivocation<
			AuthorityId,
			finality_grandpa::Prevote<H, N>,
			AuthoritySignature,
		>,
	> for Equivocation<H, N>
{
	fn from(
		equivocation: finality_grandpa::Equivocation<
			AuthorityId,
			finality_grandpa::Prevote<H, N>,
			AuthoritySignature,
		>,
	) -> Self {
		Equivocation::Prevote(equivocation)
	}
}

impl<H, N>
	From<
		finality_grandpa::Equivocation<
			AuthorityId,
			finality_grandpa::Precommit<H, N>,
			AuthoritySignature,
		>,
	> for Equivocation<H, N>
{
	fn from(
		equivocation: finality_grandpa::Equivocation<
			AuthorityId,
			finality_grandpa::Precommit<H, N>,
			AuthoritySignature,
		>,
	) -> Self {
		Equivocation::Precommit(equivocation)
	}
}

impl<H, N> Equivocation<H, N> {
	/// Returns the authority id of the equivocator.
	pub fn offender(&self) -> &AuthorityId {
		match self {
			Equivocation::Prevote(ref equivocation) => &equivocation.identity,
			Equivocation::Precommit(ref equivocation) => &equivocation.identity,
		}
	}

	/// Returns the round number when the equivocation happened.
	pub fn round_number(&self) -> RoundNumber {
		match self {
			Equivocation::Prevote(ref equivocation) => equivocation.round_number,
			Equivocation::Precommit(ref equivocation) => equivocation.round_number,
		}
	}
}

/// Verifies the equivocation proof by making sure that both votes target
/// different blocks and that its signatures are valid.
pub fn check_equivocation_proof<H, N>(report: EquivocationProof<H, N>) -> bool
where
	H: Clone + Encode + PartialEq,
	N: Clone + Encode + PartialEq,
{
	// NOTE: the bare `Prevote` and `Precommit` types don't share any trait,
	// this is implemented as a macro to avoid duplication.
	macro_rules! check {
		( $equivocation:expr, $message:expr ) => {
			// if both votes have the same target the equivocation is invalid.
			if $equivocation.first.0.target_hash == $equivocation.second.0.target_hash &&
				$equivocation.first.0.target_number == $equivocation.second.0.target_number
			{
				return false;
			}

			// check signatures on both votes are valid
			let valid_first = check_message_signature(
				&$message($equivocation.first.0),
				&$equivocation.identity,
				&$equivocation.first.1,
				$equivocation.round_number,
				report.set_id,
			)
			.is_valid();

			let valid_second = check_message_signature(
				&$message($equivocation.second.0),
				&$equivocation.identity,
				&$equivocation.second.1,
				$equivocation.round_number,
				report.set_id,
			)
			.is_valid();

			return valid_first && valid_second
		};
	}

	match report.equivocation {
		Equivocation::Prevote(equivocation) => {
			check!(equivocation, finality_grandpa::Message::Prevote);
		},
		Equivocation::Precommit(equivocation) => {
			check!(equivocation, finality_grandpa::Message::Precommit);
		},
	}
}

/// Encode round message localized to a given round and set id.
pub fn localized_payload<E: Encode>(round: RoundNumber, set_id: SetId, message: &E) -> Vec<u8> {
	let mut buf = Vec::new();
	localized_payload_with_buffer(round, set_id, message, &mut buf);
	buf
}

/// Encode round message localized to a given round and set id using the given
/// buffer. The given buffer will be cleared and the resulting encoded payload
/// will always be written to the start of the buffer.
pub fn localized_payload_with_buffer<E: Encode>(
	round: RoundNumber,
	set_id: SetId,
	message: &E,
	buf: &mut Vec<u8>,
) {
	buf.clear();
	(message, round, set_id).encode_to(buf)
}

/// Result of checking a message signature.
#[derive(Clone, Encode, Decode, PartialEq, Eq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum SignatureResult {
	/// Valid signature.
	Valid,

	/// Invalid signature.
	Invalid,

	/// Valid signature, but the message was signed in the previous set.
	OutdatedSet,
}

impl SignatureResult {
	/// Returns `true` if the signature is valid.
	pub fn is_valid(&self) -> bool {
		matches!(self, SignatureResult::Valid)
	}
}

/// Check a message signature by encoding the message as a localized payload and
/// verifying the provided signature using the expected authority id.
pub fn check_message_signature<H, N>(
	message: &finality_grandpa::Message<H, N>,
	id: &AuthorityId,
	signature: &AuthoritySignature,
	round: RoundNumber,
	set_id: SetId,
) -> SignatureResult
where
	H: Encode,
	N: Encode,
{
	check_message_signature_with_buffer(message, id, signature, round, set_id, &mut Vec::new())
}

/// Check a message signature by encoding the message as a localized payload and
/// verifying the provided signature using the expected authority id.
/// The encoding necessary to verify the signature will be done using the given
/// buffer, the original content of the buffer will be cleared.
pub fn check_message_signature_with_buffer<H, N>(
	message: &finality_grandpa::Message<H, N>,
	id: &AuthorityId,
	signature: &AuthoritySignature,
	round: RoundNumber,
	set_id: SetId,
	buf: &mut Vec<u8>,
) -> SignatureResult
where
	H: Encode,
	N: Encode,
{
	use sp_application_crypto::RuntimeAppPublic;

	localized_payload_with_buffer(round, set_id, message, buf);

	if id.verify(&buf, signature) {
		return SignatureResult::Valid;
	}

	let log_target = if cfg!(feature = "std") { CLIENT_LOG_TARGET } else { RUNTIME_LOG_TARGET };
	log::debug!(
		target: log_target,
		"Bad signature on message from id={id:?} round={round:?} set_id={set_id:?}",
	);

	// Check if the signature is valid in the previous set.
	if set_id == 0 {
		return SignatureResult::Invalid;
	}

	let prev_set_id = set_id - 1;
	localized_payload_with_buffer(round, prev_set_id, message, buf);
	let valid = id.verify(&buf, signature);
	log::debug!(
		target: log_target,
		"Previous set signature check for id={id:?} round={round:?} previous_set={prev_set_id:?} valid={valid:?}"
	);

	if valid {
		SignatureResult::OutdatedSet
	} else {
		SignatureResult::Invalid
	}
}

/// Localizes the message to the given set and round and signs the payload.
#[cfg(feature = "std")]
pub fn sign_message<H, N>(
	keystore: KeystorePtr,
	message: finality_grandpa::Message<H, N>,
	public: AuthorityId,
	round: RoundNumber,
	set_id: SetId,
) -> Option<finality_grandpa::SignedMessage<H, N, AuthoritySignature, AuthorityId>>
where
	H: Encode,
	N: Encode,
{
	use sp_application_crypto::AppCrypto;

	let encoded = localized_payload(round, set_id, &message);
	let signature = keystore
		.sign_with(
			AuthorityId::ID,
			<AuthorityId as AppCrypto>::CRYPTO_ID,
			public.as_ref(),
			&encoded[..],
		)
		.ok()
		.flatten()?
		.as_slice()
		.try_into()
		.ok()?;

	Some(finality_grandpa::SignedMessage { message, signature, id: public })
}

/// An opaque type used to represent the key ownership proof at the runtime API
/// boundary. The inner value is an encoded representation of the actual key
/// ownership proof which will be parameterized when defining the runtime. At
/// the runtime API boundary this type is unknown and as such we keep this
/// opaque representation, implementors of the runtime API will have to make
/// sure that all usages of `OpaqueKeyOwnershipProof` refer to the same type.
pub type OpaqueKeyOwnershipProof = OpaqueValue;

sp_api::decl_runtime_apis! {
	/// APIs for integrating the GRANDPA finality gadget into runtimes.
	/// This should be implemented on the runtime side.
	///
	/// This is primarily used for negotiating authority-set changes for the
	/// gadget. GRANDPA uses a signaling model of changing authority sets:
	/// changes should be signaled with a delay of N blocks, and then automatically
	/// applied in the runtime after those N blocks have passed.
	///
	/// The consensus protocol will coordinate the handoff externally.
	#[api_version(3)]
	pub trait GrandpaApi {
		/// Get the current GRANDPA authorities and weights. This should not change except
		/// for when changes are scheduled and the corresponding delay has passed.
		///
		/// When called at block B, it will return the set of authorities that should be
		/// used to finalize descendants of this block (B+1, B+2, ...). The block B itself
		/// is finalized by the authorities from block B-1.
		fn grandpa_authorities() -> AuthorityList;

		/// Submits an unsigned extrinsic to report an equivocation. The caller
		/// must provide the equivocation proof and a key ownership proof
		/// (should be obtained using `generate_key_ownership_proof`). The
		/// extrinsic will be unsigned and should only be accepted for local
		/// authorship (not to be broadcast to the network). This method returns
		/// `None` when creation of the extrinsic fails, e.g. if equivocation
		/// reporting is disabled for the given runtime (i.e. this method is
		/// hardcoded to return `None`). Only useful in an offchain context.
		fn submit_report_equivocation_unsigned_extrinsic(
			equivocation_proof: EquivocationProof<Block::Hash, NumberFor<Block>>,
			key_owner_proof: OpaqueKeyOwnershipProof,
		) -> Option<()>;

		/// Generates a proof of key ownership for the given authority in the
		/// given set. An example usage of this module is coupled with the
		/// session historical module to prove that a given authority key is
		/// tied to a given staking identity during a specific session. Proofs
		/// of key ownership are necessary for submitting equivocation reports.
		/// NOTE: even though the API takes a `set_id` as parameter the current
		/// implementations ignore this parameter and instead rely on this
		/// method being called at the correct block height, i.e. any point at
		/// which the given set id is live on-chain. Future implementations will
		/// instead use indexed data through an offchain worker, not requiring
		/// older states to be available.
		fn generate_key_ownership_proof(
			set_id: SetId,
			authority_id: AuthorityId,
		) -> Option<OpaqueKeyOwnershipProof>;

		/// Get current GRANDPA authority set id.
		fn current_set_id() -> SetId;
	}
}
