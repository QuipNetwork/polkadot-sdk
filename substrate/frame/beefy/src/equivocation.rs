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

//! An opt-in utility module for reporting equivocations.
//!
//! This module defines an offence type for BEEFY equivocations
//! and some utility traits to wire together:
//! - a key ownership proof system (e.g. to prove that a given authority was part of a session);
//! - a system for reporting offences;
//! - a system for signing and submitting transactions;
//! - a way to get the current block author;
//!
//! These can be used in an offchain context in order to submit equivocation
//! reporting extrinsics (from the client that's running the BEEFY protocol).
//! And in a runtime context, so that the BEEFY pallet can validate the
//! equivocation proofs in the extrinsic and report the offences.
//!
//! IMPORTANT:
//! When using this module for enabling equivocation reporting it is required
//! that the `ValidateUnsigned` for the BEEFY pallet is used in the runtime
//! definition.

use alloc::{vec, vec::Vec};
use codec::{self as codec, Decode, Encode};
use frame_support::traits::{Get, KeyOwnerProofSystem};
use frame_system::pallet_prelude::{BlockNumberFor, HeaderFor};
use log::{error, info};
use sp_consensus_beefy::{
	check_commitment_signature, AncestryHelper, DoubleVotingProof, ForkVotingProof,
	FutureBlockVotingProof, ValidatorSetId, KEY_TYPE as BEEFY_KEY_TYPE,
};
use sp_runtime::{
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
		TransactionValidityError, ValidTransaction,
	},
	DispatchError, KeyTypeId, Perbill, RuntimeAppPublic,
};
use sp_session::{GetSessionNumber, GetValidatorCount};
use sp_staking::{
	offence::{Kind, Offence, OffenceReportSystem, ReportOffence},
	SessionIndex,
};

use super::{Call, Config, Error, Pallet, LOG_TARGET};

/// A set of fields which point to the time and type of an offence.
#[derive(Copy, Clone, PartialOrd, Ord, Eq, PartialEq, Encode, Decode)]
pub struct Slot<N: Copy + Clone + PartialOrd + Ord + Eq + PartialEq + Encode + Decode> {
	// The order of these matters for `derive(Ord)`.
	/// BEEFY Set ID.
	pub set_id: ValidatorSetId,
	/// Round number.
	pub round: N,
	/// Equivocation type
	pub equivocation_type: u8,
}

/// BEEFY equivocation offence report.
pub struct EquivocationOffence<Offender, N>
where
	N: Copy + Clone + PartialOrd + Ord + Eq + PartialEq + Encode + Decode,
{
	/// Time slot at which this incident happened.
	pub slot: Slot<N>,
	/// The session index in which the incident happened.
	pub session_index: SessionIndex,
	/// The size of the validator set at the time of the offence.
	pub validator_set_count: u32,
	/// The authority which produced this equivocation.
	pub offender: Offender,
	/// Optional slash fraction
	maybe_slash_fraction: Option<Perbill>,
}

impl<Offender: Clone, N> Offence<Offender> for EquivocationOffence<Offender, N>
where
	N: Copy + Clone + PartialOrd + Ord + Eq + PartialEq + Encode + Decode,
{
	const ID: Kind = *b"beefy:equivocati";
	type Slot = Slot<N>;

	fn offenders(&self) -> Vec<Offender> {
		vec![self.offender.clone()]
	}

	fn session_index(&self) -> SessionIndex {
		self.session_index
	}

	fn validator_set_count(&self) -> u32 {
		self.validator_set_count
	}

	fn slot(&self) -> Self::Slot {
		self.slot
	}

	fn slash_fraction(&self, offenders_count: u32) -> Perbill {
		if let Some(slash_fraction) = self.maybe_slash_fraction {
			return slash_fraction;
		}

		// `Perbill` type domain is [0, 1] by definition
		// The formula is min((3k / n)^2, 1)
		// where k = offenders_number and n = validators_number
		Perbill::from_rational(3 * offenders_count, self.validator_set_count).square()
	}
}

/// BEEFY equivocation offence report system.
///
/// This type implements `OffenceReportSystem` such that:
/// - Equivocation reports are published on-chain as unsigned extrinsic via
///   `offchain::CreateTransactionBase`.
/// - On-chain validity checks and processing are mostly delegated to the user provided generic
///   types implementing `KeyOwnerProofSystem` and `ReportOffence` traits.
/// - Offence reporter for unsigned transactions is fetched via the authorship pallet.
pub struct EquivocationReportSystem<T, R, P, L>(core::marker::PhantomData<(T, R, P, L)>);

/// Equivocation evidence convenience alias.
pub enum EquivocationEvidenceFor<T: Config> {
	DoubleVotingProof(
		DoubleVotingProof<
			BlockNumberFor<T>,
			T::BeefyId,
			<T::BeefyId as RuntimeAppPublic>::Signature,
		>,
		T::KeyOwnerProof,
	),
	ForkVotingProof(
		ForkVotingProof<
			HeaderFor<T>,
			T::BeefyId,
			<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::Proof,
		>,
		T::KeyOwnerProof,
	),
	FutureBlockVotingProof(FutureBlockVotingProof<BlockNumberFor<T>, T::BeefyId>, T::KeyOwnerProof),
}

impl<T: Config> EquivocationEvidenceFor<T> {
	fn equivocation_type(&self) -> u8 {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(..) => 1,
			EquivocationEvidenceFor::ForkVotingProof(..) => 2,
			EquivocationEvidenceFor::FutureBlockVotingProof(..) => 3,
		}
	}

	/// Returns the authority id of the equivocator.
	fn offender_id(&self) -> &T::BeefyId {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(equivocation_proof, _) => {
				equivocation_proof.offender_id()
			},
			EquivocationEvidenceFor::ForkVotingProof(equivocation_proof, _) => {
				&equivocation_proof.vote.id
			},
			EquivocationEvidenceFor::FutureBlockVotingProof(equivocation_proof, _) => {
				&equivocation_proof.vote.id
			},
		}
	}

	/// Returns the round number at which the equivocation occurred.
	fn round_number(&self) -> &BlockNumberFor<T> {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(equivocation_proof, _) => {
				equivocation_proof.round_number()
			},
			EquivocationEvidenceFor::ForkVotingProof(equivocation_proof, _) => {
				&equivocation_proof.vote.commitment.block_number
			},
			EquivocationEvidenceFor::FutureBlockVotingProof(equivocation_proof, _) => {
				&equivocation_proof.vote.commitment.block_number
			},
		}
	}

	/// Returns the set id at which the equivocation occurred.
	fn set_id(&self) -> ValidatorSetId {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(equivocation_proof, _) => {
				equivocation_proof.set_id()
			},
			EquivocationEvidenceFor::ForkVotingProof(equivocation_proof, _) => {
				equivocation_proof.vote.commitment.validator_set_id
			},
			EquivocationEvidenceFor::FutureBlockVotingProof(equivocation_proof, _) => {
				equivocation_proof.vote.commitment.validator_set_id
			},
		}
	}

	/// Returns the set id at which the equivocation occurred.
	fn key_owner_proof(&self) -> &T::KeyOwnerProof {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(_, key_owner_proof) => key_owner_proof,
			EquivocationEvidenceFor::ForkVotingProof(_, key_owner_proof) => key_owner_proof,
			EquivocationEvidenceFor::FutureBlockVotingProof(_, key_owner_proof) => key_owner_proof,
		}
	}

	fn checked_offender<P>(&self) -> Option<P::IdentificationTuple>
	where
		P: KeyOwnerProofSystem<(KeyTypeId, T::BeefyId), Proof = T::KeyOwnerProof>,
	{
		let key = (BEEFY_KEY_TYPE, self.offender_id().clone());
		P::check_proof(key, self.key_owner_proof().clone())
	}

	fn check_equivocation_proof(self) -> Result<(), Error<T>> {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(equivocation_proof, _) => {
				// Validate equivocation proof (check votes are different and signatures are valid).
				if !sp_consensus_beefy::check_double_voting_proof(&equivocation_proof) {
					return Err(Error::<T>::InvalidDoubleVotingProof);
				}

				Ok(())
			},
			EquivocationEvidenceFor::ForkVotingProof(equivocation_proof, _) => {
				let ForkVotingProof { vote, ancestry_proof, header } = equivocation_proof;

				if !<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::is_proof_optimal(
					&ancestry_proof,
				) {
					return Err(Error::<T>::InvalidForkVotingProof);
				}

				let maybe_validation_context = <T::AncestryHelper as AncestryHelper<
					HeaderFor<T>,
				>>::extract_validation_context(header);
				let validation_context = match maybe_validation_context {
					Some(validation_context) => validation_context,
					None => {
						return Err(Error::<T>::InvalidForkVotingProof);
					},
				};

				let is_non_canonical =
					<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::is_non_canonical(
						&vote.commitment,
						ancestry_proof,
						validation_context,
					);
				if !is_non_canonical {
					return Err(Error::<T>::InvalidForkVotingProof);
				}

				let is_signature_valid =
					check_commitment_signature(&vote.commitment, &vote.id, &vote.signature);
				if !is_signature_valid {
					return Err(Error::<T>::InvalidForkVotingProof);
				}

				Ok(())
			},
			EquivocationEvidenceFor::FutureBlockVotingProof(equivocation_proof, _) => {
				let FutureBlockVotingProof { vote } = equivocation_proof;
				// Check if the commitment actually targets a future block
				if vote.commitment.block_number < frame_system::Pallet::<T>::block_number() {
					return Err(Error::<T>::InvalidFutureBlockVotingProof);
				}

				let is_signature_valid =
					check_commitment_signature(&vote.commitment, &vote.id, &vote.signature);
				if !is_signature_valid {
					return Err(Error::<T>::InvalidFutureBlockVotingProof);
				}

				Ok(())
			},
		}
	}

	fn slash_fraction(&self) -> Option<Perbill> {
		match self {
			EquivocationEvidenceFor::DoubleVotingProof(_, _) => None,
			EquivocationEvidenceFor::ForkVotingProof(_, _) |
			EquivocationEvidenceFor::FutureBlockVotingProof(_, _) => Some(Perbill::from_percent(50)),
		}
	}
}

impl<T, R, P, L> OffenceReportSystem<Option<T::AccountId>, EquivocationEvidenceFor<T>>
	for EquivocationReportSystem<T, R, P, L>
where
	T: Config + pallet_authorship::Config + frame_system::offchain::CreateBare<Call<T>>,
	R: ReportOffence<
		T::AccountId,
		P::IdentificationTuple,
		EquivocationOffence<P::IdentificationTuple, BlockNumberFor<T>>,
	>,
	P: KeyOwnerProofSystem<(KeyTypeId, T::BeefyId), Proof = T::KeyOwnerProof>,
	P::IdentificationTuple: Clone,
	L: Get<u64>,
{
	type Longevity = L;

	fn publish_evidence(evidence: EquivocationEvidenceFor<T>) -> Result<(), ()> {
		use frame_system::offchain::SubmitTransaction;

		let call: Call<T> = evidence.into();
		let xt = T::create_bare(call.into());
		let res = SubmitTransaction::<T, Call<T>>::submit_transaction(xt);
		match res {
			Ok(_) => info!(target: LOG_TARGET, "Submitted equivocation report."),
			Err(e) => error!(target: LOG_TARGET, "Error submitting equivocation report: {:?}", e),
		}
		res
	}

	fn check_evidence(
		evidence: EquivocationEvidenceFor<T>,
	) -> Result<(), TransactionValidityError> {
		let offender = evidence.checked_offender::<P>().ok_or(InvalidTransaction::BadProof)?;

		// Check if the offence has already been reported, and if so then we can discard the report.
		let slot = Slot {
			set_id: evidence.set_id(),
			round: *evidence.round_number(),
			equivocation_type: evidence.equivocation_type(),
		};
		if R::is_known_offence(&[offender], &slot) {
			Err(InvalidTransaction::Stale.into())
		} else {
			Ok(())
		}
	}

	fn process_evidence(
		reporter: Option<T::AccountId>,
		evidence: EquivocationEvidenceFor<T>,
	) -> Result<(), DispatchError> {
		let maybe_slash_fraction = evidence.slash_fraction();
		let reporter = reporter.or_else(|| pallet_authorship::Pallet::<T>::author());

		// We check the equivocation within the context of its set id (and associated session).
		let set_id = evidence.set_id();
		let set_id_session_index = crate::SetIdSession::<T>::get(set_id)
			.ok_or(Error::<T>::InvalidEquivocationProofSessionMember)?;

		// Check that the session id for the membership proof is within the bounds
		// of the set id reported in the equivocation.
		let key_owner_proof = evidence.key_owner_proof();
		let validator_count = key_owner_proof.validator_count();
		let session_index = key_owner_proof.session();
		if session_index != set_id_session_index {
			return Err(Error::<T>::InvalidEquivocationProofSession.into());
		}

		// Validate the key ownership proof extracting the id of the offender.
		let offender =
			evidence.checked_offender::<P>().ok_or(Error::<T>::InvalidKeyOwnershipProof)?;

		let round = *evidence.round_number();
		let equivocation_type = evidence.equivocation_type();

		evidence.check_equivocation_proof()?;

		let offence = EquivocationOffence {
			slot: Slot { set_id, round, equivocation_type },
			session_index,
			validator_set_count: validator_count,
			offender,
			maybe_slash_fraction,
		};
		R::report_offence(reporter.into_iter().collect(), offence)
			.map_err(|_| Error::<T>::DuplicateOffenceReport.into())
	}
}

/// Methods for the `ValidateUnsigned` implementation:
/// It restricts calls to `report_equivocation_unsigned` to local calls (i.e. extrinsics generated
/// on this node) or that already in a block. This guarantees that only block authors can include
/// unsigned equivocation reports.
impl<T: Config> Pallet<T> {
	pub fn validate_unsigned(source: TransactionSource, call: &Call<T>) -> TransactionValidity {
		// discard equivocation report not coming from the local node
		match source {
			TransactionSource::Local | TransactionSource::InBlock => { /* allowed */ },
			_ => {
				log::warn!(
					target: LOG_TARGET,
					"rejecting unsigned report equivocation transaction because it is not local/in-block."
				);
				return InvalidTransaction::Call.into();
			},
		}

		let evidence = call.to_equivocation_evidence_for().ok_or(InvalidTransaction::Call)?;
		let tag = (evidence.offender_id().clone(), evidence.set_id(), *evidence.round_number());
		T::EquivocationReportSystem::check_evidence(evidence)?;

		let longevity =
			<T::EquivocationReportSystem as OffenceReportSystem<_, _>>::Longevity::get();
		ValidTransaction::with_tag_prefix("BeefyEquivocation")
			// We assign the maximum priority for any equivocation report.
			.priority(TransactionPriority::MAX)
			// Only one equivocation report for the same offender at the same slot.
			.and_provides(tag)
			.longevity(longevity)
			// We don't propagate this. This can never be included on a remote node.
			.propagate(false)
			.build()
	}

	pub fn pre_dispatch(call: &Call<T>) -> Result<(), TransactionValidityError> {
		let evidence = call.to_equivocation_evidence_for().ok_or(InvalidTransaction::Call)?;
		T::EquivocationReportSystem::check_evidence(evidence)
	}
}
