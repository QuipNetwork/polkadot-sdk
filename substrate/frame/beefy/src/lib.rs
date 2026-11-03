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

extern crate alloc;

mod default_weights;
mod equivocation;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use alloc::{boxed::Box, vec::Vec};
use codec::{Encode, MaxEncodedLen};
use log;

use frame_support::{
	dispatch::{DispatchResultWithPostInfo, Pays},
	pallet_prelude::*,
	traits::{Get, OneSessionHandler},
	weights::{constants::RocksDbWeight as DbWeight, Weight},
	BoundedSlice, BoundedVec, Parameter,
};
use frame_system::{
	ensure_none, ensure_signed,
	pallet_prelude::{BlockNumberFor, HeaderFor, OriginFor},
};
use sp_consensus_beefy::{
	AncestryHelper, AncestryHelperWeightInfo, AuthorityIndex, BeefyAuthorityId, ConsensusLog,
	DoubleVotingProof, ForkVotingProof, FutureBlockVotingProof, OnNewValidatorSet, ValidatorSet,
	BEEFY_ENGINE_ID, GENESIS_AUTHORITY_SET_ID,
};
use sp_runtime::{
	generic::DigestItem,
	traits::{IsMember, Member, One},
	RuntimeAppPublic,
};
use sp_session::{GetSessionNumber, GetValidatorCount};
use sp_staking::{offence::OffenceReportSystem, SessionIndex};

use crate::equivocation::EquivocationEvidenceFor;
pub use crate::equivocation::{EquivocationOffence, EquivocationReportSystem, Slot};
pub use pallet::*;

const LOG_TARGET: &str = "runtime::beefy";

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_system::{ensure_root, pallet_prelude::BlockNumberFor};

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Authority identifier type
		type BeefyId: Member
			+ Parameter
			+ BeefyAuthorityId
			+ MaybeSerializeDeserialize
			+ MaxEncodedLen;

		/// The maximum number of authorities that can be added.
		#[pallet::constant]
		type MaxAuthorities: Get<u32>;

		/// The maximum number of nominators for each validator.
		#[pallet::constant]
		type MaxNominators: Get<u32>;

		/// The maximum number of entries to keep in the set id to session index mapping.
		///
		/// Since the `SetIdSession` map is only used for validating equivocations this
		/// value should relate to the bonding duration of whatever staking system is
		/// being used (if any). If equivocation handling is not enabled then this value
		/// can be zero.
		#[pallet::constant]
		type MaxSetIdSessionEntries: Get<u64>;

		/// A hook to act on the new BEEFY validator set.
		///
		/// For some applications it might be beneficial to make the BEEFY validator set available
		/// externally apart from having it in the storage. For instance you might cache a light
		/// weight MMR root over validators and make it available for Light Clients.
		type OnNewValidatorSet: OnNewValidatorSet<<Self as Config>::BeefyId>;

		/// Hook for checking commitment canonicity.
		type AncestryHelper: AncestryHelper<HeaderFor<Self>>
			+ AncestryHelperWeightInfo<HeaderFor<Self>>;

		/// Weights for this pallet.
		type WeightInfo: WeightInfo;

		/// The proof of key ownership, used for validating equivocation reports
		/// The proof must include the session index and validator count of the
		/// session at which the equivocation occurred.
		type KeyOwnerProof: Parameter + GetSessionNumber + GetValidatorCount;

		/// The equivocation handling subsystem.
		///
		/// Defines methods to publish, check and process an equivocation offence.
		type EquivocationReportSystem: OffenceReportSystem<
			Option<Self::AccountId>,
			EquivocationEvidenceFor<Self>,
		>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// The current authorities set
	#[pallet::storage]
	pub type Authorities<T: Config> =
		StorageValue<_, BoundedVec<T::BeefyId, T::MaxAuthorities>, ValueQuery>;

	/// The current validator set id
	#[pallet::storage]
	pub type ValidatorSetId<T: Config> =
		StorageValue<_, sp_consensus_beefy::ValidatorSetId, ValueQuery>;

	/// Authorities set scheduled to be used with the next session
	#[pallet::storage]
	pub type NextAuthorities<T: Config> =
		StorageValue<_, BoundedVec<T::BeefyId, T::MaxAuthorities>, ValueQuery>;

	/// A mapping from BEEFY set ID to the index of the *most recent* session for which its
	/// members were responsible.
	///
	/// This is only used for validating equivocation proofs. An equivocation proof must
	/// contains a key-ownership proof for a given session, therefore we need a way to tie
	/// together sessions and BEEFY set ids, i.e. we need to validate that a validator
	/// was the owner of a given key on a given session, and what the active set ID was
	/// during that session.
	///
	/// TWOX-NOTE: `ValidatorSetId` is not under user control.
	#[pallet::storage]
	pub type SetIdSession<T: Config> =
		StorageMap<_, Twox64Concat, sp_consensus_beefy::ValidatorSetId, SessionIndex>;

	/// Block number where BEEFY consensus is enabled/started.
	/// By changing this (through privileged `set_new_genesis()`), BEEFY consensus is effectively
	/// restarted from the newly set block number.
	#[pallet::storage]
	pub type GenesisBlock<T: Config> = StorageValue<_, Option<BlockNumberFor<T>>, ValueQuery>;

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		/// Initial set of BEEFY authorities.
		pub authorities: Vec<T::BeefyId>,
		/// Block number where BEEFY consensus should start.
		/// Should match the session where initial authorities are active.
		/// *Note:* Ideally use block number where GRANDPA authorities are changed,
		/// to guarantee the client gets a finality notification for exactly this block.
		pub genesis_block: Option<BlockNumberFor<T>>,
	}

	impl<T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			// BEEFY genesis will be first BEEFY-MANDATORY block,
			// use block number one instead of chain-genesis.
			let genesis_block = Some(One::one());
			Self { authorities: Vec::new(), genesis_block }
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			Pallet::<T>::initialize(&self.authorities)
				// we panic here as runtime maintainers can simply reconfigure genesis and restart
				// the chain easily
				.expect("Authorities vec too big");
			GenesisBlock::<T>::put(&self.genesis_block);
		}
	}

	#[pallet::error]
	pub enum Error<T> {
		/// A key ownership proof provided as part of an equivocation report is invalid.
		InvalidKeyOwnershipProof,
		/// A double voting proof provided as part of an equivocation report is invalid.
		InvalidDoubleVotingProof,
		/// A fork voting proof provided as part of an equivocation report is invalid.
		InvalidForkVotingProof,
		/// A future block voting proof provided as part of an equivocation report is invalid.
		InvalidFutureBlockVotingProof,
		/// The session of the equivocation proof is invalid
		InvalidEquivocationProofSession,
		/// The session of the equivocation proof is not in the mapping (anymore)
		InvalidEquivocationProofSessionMember,
		/// A given equivocation report is valid but already previously reported.
		DuplicateOffenceReport,
		/// Submitted configuration is invalid.
		InvalidConfiguration,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Report voter equivocation/misbehavior. This method will verify the
		/// equivocation proof and validate the given key ownership proof
		/// against the extracted offender. If both are valid, the offence
		/// will be reported.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::report_double_voting(
			key_owner_proof.validator_count(),
			T::MaxNominators::get(),
		))]
		pub fn report_double_voting(
			origin: OriginFor<T>,
			equivocation_proof: Box<
				DoubleVotingProof<
					BlockNumberFor<T>,
					T::BeefyId,
					<T::BeefyId as RuntimeAppPublic>::Signature,
				>,
			>,
			key_owner_proof: T::KeyOwnerProof,
		) -> DispatchResultWithPostInfo {
			let reporter = ensure_signed(origin)?;

			T::EquivocationReportSystem::process_evidence(
				Some(reporter),
				EquivocationEvidenceFor::DoubleVotingProof(*equivocation_proof, key_owner_proof),
			)?;
			// Waive the fee since the report is valid and beneficial
			Ok(Pays::No.into())
		}

		/// Report voter equivocation/misbehavior. This method will verify the
		/// equivocation proof and validate the given key ownership proof
		/// against the extracted offender. If both are valid, the offence
		/// will be reported.
		///
		/// This extrinsic must be called unsigned and it is expected that only
		/// block authors will call it (validated in `ValidateUnsigned`), as such
		/// if the block author is defined it will be defined as the equivocation
		/// reporter.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::report_double_voting(
			key_owner_proof.validator_count(),
			T::MaxNominators::get(),
		))]
		pub fn report_double_voting_unsigned(
			origin: OriginFor<T>,
			equivocation_proof: Box<
				DoubleVotingProof<
					BlockNumberFor<T>,
					T::BeefyId,
					<T::BeefyId as RuntimeAppPublic>::Signature,
				>,
			>,
			key_owner_proof: T::KeyOwnerProof,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			T::EquivocationReportSystem::process_evidence(
				None,
				EquivocationEvidenceFor::DoubleVotingProof(*equivocation_proof, key_owner_proof),
			)?;
			Ok(Pays::No.into())
		}

		/// Reset BEEFY consensus by setting a new BEEFY genesis at `delay_in_blocks` blocks in the
		/// future.
		///
		/// Note: `delay_in_blocks` has to be at least 1.
		#[pallet::call_index(2)]
		#[pallet::weight(<T as Config>::WeightInfo::set_new_genesis())]
		pub fn set_new_genesis(
			origin: OriginFor<T>,
			delay_in_blocks: BlockNumberFor<T>,
		) -> DispatchResult {
			ensure_root(origin)?;
			ensure!(delay_in_blocks >= One::one(), Error::<T>::InvalidConfiguration);
			let genesis_block = frame_system::Pallet::<T>::block_number() + delay_in_blocks;
			GenesisBlock::<T>::put(Some(genesis_block));
			Ok(())
		}

		/// Report fork voting equivocation. This method will verify the equivocation proof
		/// and validate the given key ownership proof against the extracted offender.
		/// If both are valid, the offence will be reported.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::report_fork_voting::<T>(
			key_owner_proof.validator_count(),
			T::MaxNominators::get(),
			&equivocation_proof.ancestry_proof
		))]
		pub fn report_fork_voting(
			origin: OriginFor<T>,
			equivocation_proof: Box<
				ForkVotingProof<
					HeaderFor<T>,
					T::BeefyId,
					<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::Proof,
				>,
			>,
			key_owner_proof: T::KeyOwnerProof,
		) -> DispatchResultWithPostInfo {
			let reporter = ensure_signed(origin)?;

			T::EquivocationReportSystem::process_evidence(
				Some(reporter),
				EquivocationEvidenceFor::ForkVotingProof(*equivocation_proof, key_owner_proof),
			)?;
			// Waive the fee since the report is valid and beneficial
			Ok(Pays::No.into())
		}

		/// Report fork voting equivocation. This method will verify the equivocation proof
		/// and validate the given key ownership proof against the extracted offender.
		/// If both are valid, the offence will be reported.
		///
		/// This extrinsic must be called unsigned and it is expected that only
		/// block authors will call it (validated in `ValidateUnsigned`), as such
		/// if the block author is defined it will be defined as the equivocation
		/// reporter.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::report_fork_voting::<T>(
			key_owner_proof.validator_count(),
			T::MaxNominators::get(),
			&equivocation_proof.ancestry_proof
		))]
		pub fn report_fork_voting_unsigned(
			origin: OriginFor<T>,
			equivocation_proof: Box<
				ForkVotingProof<
					HeaderFor<T>,
					T::BeefyId,
					<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::Proof,
				>,
			>,
			key_owner_proof: T::KeyOwnerProof,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			T::EquivocationReportSystem::process_evidence(
				None,
				EquivocationEvidenceFor::ForkVotingProof(*equivocation_proof, key_owner_proof),
			)?;
			// Waive the fee since the report is valid and beneficial
			Ok(Pays::No.into())
		}

		/// Report future block voting equivocation. This method will verify the equivocation proof
		/// and validate the given key ownership proof against the extracted offender.
		/// If both are valid, the offence will be reported.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::report_future_block_voting(
			key_owner_proof.validator_count(),
			T::MaxNominators::get(),
		))]
		pub fn report_future_block_voting(
			origin: OriginFor<T>,
			equivocation_proof: Box<FutureBlockVotingProof<BlockNumberFor<T>, T::BeefyId>>,
			key_owner_proof: T::KeyOwnerProof,
		) -> DispatchResultWithPostInfo {
			let reporter = ensure_signed(origin)?;

			T::EquivocationReportSystem::process_evidence(
				Some(reporter),
				EquivocationEvidenceFor::FutureBlockVotingProof(
					*equivocation_proof,
					key_owner_proof,
				),
			)?;
			// Waive the fee since the report is valid and beneficial
			Ok(Pays::No.into())
		}

		/// Report future block voting equivocation. This method will verify the equivocation proof
		/// and validate the given key ownership proof against the extracted offender.
		/// If both are valid, the offence will be reported.
		///
		/// This extrinsic must be called unsigned and it is expected that only
		/// block authors will call it (validated in `ValidateUnsigned`), as such
		/// if the block author is defined it will be defined as the equivocation
		/// reporter.
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::report_future_block_voting(
			key_owner_proof.validator_count(),
			T::MaxNominators::get(),
		))]
		pub fn report_future_block_voting_unsigned(
			origin: OriginFor<T>,
			equivocation_proof: Box<FutureBlockVotingProof<BlockNumberFor<T>, T::BeefyId>>,
			key_owner_proof: T::KeyOwnerProof,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			T::EquivocationReportSystem::process_evidence(
				None,
				EquivocationEvidenceFor::FutureBlockVotingProof(
					*equivocation_proof,
					key_owner_proof,
				),
			)?;
			// Waive the fee since the report is valid and beneficial
			Ok(Pays::No.into())
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}

	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		fn pre_dispatch(call: &Self::Call) -> Result<(), TransactionValidityError> {
			Self::pre_dispatch(call)
		}

		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			Self::validate_unsigned(source, call)
		}
	}

	impl<T: Config> Call<T> {
		pub fn to_equivocation_evidence_for(&self) -> Option<EquivocationEvidenceFor<T>> {
			match self {
				Call::report_double_voting_unsigned { equivocation_proof, key_owner_proof } => {
					Some(EquivocationEvidenceFor::<T>::DoubleVotingProof(
						*equivocation_proof.clone(),
						key_owner_proof.clone(),
					))
				},
				Call::report_fork_voting_unsigned { equivocation_proof, key_owner_proof } => {
					Some(EquivocationEvidenceFor::<T>::ForkVotingProof(
						*equivocation_proof.clone(),
						key_owner_proof.clone(),
					))
				},
				Call::report_future_block_voting_unsigned {
					equivocation_proof,
					key_owner_proof,
				} => Some(EquivocationEvidenceFor::<T>::FutureBlockVotingProof(
					*equivocation_proof.clone(),
					key_owner_proof.clone(),
				)),
				_ => None,
			}
		}
	}

	impl<T: Config> From<EquivocationEvidenceFor<T>> for Call<T> {
		fn from(evidence: EquivocationEvidenceFor<T>) -> Self {
			match evidence {
				EquivocationEvidenceFor::DoubleVotingProof(equivocation_proof, key_owner_proof) => {
					Call::report_double_voting_unsigned {
						equivocation_proof: Box::new(equivocation_proof),
						key_owner_proof,
					}
				},
				EquivocationEvidenceFor::ForkVotingProof(equivocation_proof, key_owner_proof) => {
					Call::report_fork_voting_unsigned {
						equivocation_proof: Box::new(equivocation_proof),
						key_owner_proof,
					}
				},
				EquivocationEvidenceFor::FutureBlockVotingProof(
					equivocation_proof,
					key_owner_proof,
				) => Call::report_future_block_voting_unsigned {
					equivocation_proof: Box::new(equivocation_proof),
					key_owner_proof,
				},
			}
		}
	}
}

#[cfg(any(feature = "try-runtime", test))]
impl<T: Config> Pallet<T> {
	/// Ensure the correctness of the state of this pallet.
	///
	/// This should be valid before or after each state transition of this pallet.
	pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
		Self::try_state_authorities()?;
		Self::try_state_validators()?;

		Ok(())
	}

	/// # Invariants
	///
	/// * `Authorities` should not exceed the `MaxAuthorities` capacity.
	/// * `NextAuthorities` should not exceed the `MaxAuthorities` capacity.
	fn try_state_authorities() -> Result<(), sp_runtime::TryRuntimeError> {
		if let Some(authorities_len) = <Authorities<T>>::decode_len() {
			ensure!(
				authorities_len as u32 <= T::MaxAuthorities::get(),
				"Authorities number exceeds what the pallet config allows."
			);
		} else {
			return Err(sp_runtime::TryRuntimeError::Other(
				"Failed to decode length of authorities",
			));
		}

		if let Some(next_authorities_len) = <NextAuthorities<T>>::decode_len() {
			ensure!(
				next_authorities_len as u32 <= T::MaxAuthorities::get(),
				"Next authorities number exceeds what the pallet config allows."
			);
		} else {
			return Err(sp_runtime::TryRuntimeError::Other(
				"Failed to decode length of next authorities",
			));
		}
		Ok(())
	}

	/// # Invariants
	///
	/// `ValidatorSetId` must be present in `SetIdSession`
	fn try_state_validators() -> Result<(), sp_runtime::TryRuntimeError> {
		let validator_set_id = <ValidatorSetId<T>>::get();
		ensure!(
			SetIdSession::<T>::get(validator_set_id).is_some(),
			"Validator set id must be present in SetIdSession"
		);
		Ok(())
	}
}

impl<T: Config> Pallet<T> {
	/// Return the current active BEEFY validator set.
	pub fn validator_set() -> Option<ValidatorSet<T::BeefyId>> {
		let validators: BoundedVec<T::BeefyId, T::MaxAuthorities> = Authorities::<T>::get();
		let id: sp_consensus_beefy::ValidatorSetId = ValidatorSetId::<T>::get();
		ValidatorSet::<T::BeefyId>::new(validators, id)
	}

	/// Submits an extrinsic to report a double voting equivocation. This method will create
	/// an unsigned extrinsic with a call to `report_double_voting_unsigned` and
	/// will push the transaction to the pool. Only useful in an offchain context.
	pub fn submit_unsigned_double_voting_report(
		equivocation_proof: DoubleVotingProof<
			BlockNumberFor<T>,
			T::BeefyId,
			<T::BeefyId as RuntimeAppPublic>::Signature,
		>,
		key_owner_proof: T::KeyOwnerProof,
	) -> Option<()> {
		T::EquivocationReportSystem::publish_evidence(EquivocationEvidenceFor::DoubleVotingProof(
			equivocation_proof,
			key_owner_proof,
		))
		.ok()
	}

	/// Submits an extrinsic to report a fork voting equivocation. This method will create
	/// an unsigned extrinsic with a call to `report_fork_voting_unsigned` and
	/// will push the transaction to the pool. Only useful in an offchain context.
	pub fn submit_unsigned_fork_voting_report(
		equivocation_proof: ForkVotingProof<
			HeaderFor<T>,
			T::BeefyId,
			<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::Proof,
		>,
		key_owner_proof: T::KeyOwnerProof,
	) -> Option<()> {
		T::EquivocationReportSystem::publish_evidence(EquivocationEvidenceFor::ForkVotingProof(
			equivocation_proof,
			key_owner_proof,
		))
		.ok()
	}

	/// Submits an extrinsic to report a future block voting equivocation. This method will create
	/// an unsigned extrinsic with a call to `report_future_block_voting_unsigned` and
	/// will push the transaction to the pool. Only useful in an offchain context.
	pub fn submit_unsigned_future_block_voting_report(
		equivocation_proof: FutureBlockVotingProof<BlockNumberFor<T>, T::BeefyId>,
		key_owner_proof: T::KeyOwnerProof,
	) -> Option<()> {
		T::EquivocationReportSystem::publish_evidence(
			EquivocationEvidenceFor::FutureBlockVotingProof(equivocation_proof, key_owner_proof),
		)
		.ok()
	}

	fn change_authorities(
		new: BoundedVec<T::BeefyId, T::MaxAuthorities>,
		queued: BoundedVec<T::BeefyId, T::MaxAuthorities>,
	) {
		Authorities::<T>::put(&new);

		let new_id = ValidatorSetId::<T>::get() + 1u64;
		ValidatorSetId::<T>::put(new_id);

		NextAuthorities::<T>::put(&queued);

		if let Some(validator_set) = ValidatorSet::<T::BeefyId>::new(new, new_id) {
			let log = DigestItem::Consensus(
				BEEFY_ENGINE_ID,
				ConsensusLog::AuthoritiesChange(validator_set.clone()).encode(),
			);
			frame_system::Pallet::<T>::deposit_log(log);

			let next_id = new_id + 1;
			if let Some(next_validator_set) = ValidatorSet::<T::BeefyId>::new(queued, next_id) {
				<T::OnNewValidatorSet as OnNewValidatorSet<_>>::on_new_validator_set(
					&validator_set,
					&next_validator_set,
				);
			}
		}
	}

	fn initialize(authorities: &Vec<T::BeefyId>) -> Result<(), ()> {
		if authorities.is_empty() {
			return Ok(());
		}

		if !Authorities::<T>::get().is_empty() {
			return Err(());
		}

		let bounded_authorities =
			BoundedSlice::<T::BeefyId, T::MaxAuthorities>::try_from(authorities.as_slice())
				.map_err(|_| ())?;

		let id = GENESIS_AUTHORITY_SET_ID;
		Authorities::<T>::put(bounded_authorities);
		ValidatorSetId::<T>::put(id);
		// Like `pallet_session`, initialize the next validator set as well.
		NextAuthorities::<T>::put(bounded_authorities);

		if let Some(validator_set) = ValidatorSet::<T::BeefyId>::new(authorities.clone(), id) {
			let next_id = id + 1;
			if let Some(next_validator_set) =
				ValidatorSet::<T::BeefyId>::new(authorities.clone(), next_id)
			{
				<T::OnNewValidatorSet as OnNewValidatorSet<_>>::on_new_validator_set(
					&validator_set,
					&next_validator_set,
				);
			}
		}

		// NOTE: initialize first session of first set. this is necessary for
		// the genesis set and session since we only update the set -> session
		// mapping whenever a new session starts, i.e. through `on_new_session`.
		SetIdSession::<T>::insert(0, 0);

		Ok(())
	}
}

impl<T: Config> sp_runtime::BoundToRuntimeAppPublic for Pallet<T> {
	type Public = T::BeefyId;
}

impl<T: Config> OneSessionHandler<T::AccountId> for Pallet<T>
where
	T: pallet_session::Config,
{
	type Key = T::BeefyId;

	fn on_genesis_session<'a, I: 'a>(validators: I)
	where
		I: Iterator<Item = (&'a T::AccountId, T::BeefyId)>,
	{
		let authorities = validators.map(|(_, k)| k).collect::<Vec<_>>();
		// we panic here as runtime maintainers can simply reconfigure genesis and restart the
		// chain easily
		Self::initialize(&authorities).expect("Authorities vec too big");
	}

	fn on_new_session<'a, I: 'a>(_changed: bool, validators: I, queued_validators: I)
	where
		I: Iterator<Item = (&'a T::AccountId, T::BeefyId)>,
	{
		let next_authorities = validators.map(|(_, k)| k).collect::<Vec<_>>();
		if next_authorities.len() as u32 > T::MaxAuthorities::get() {
			log::error!(
				target: LOG_TARGET,
				"authorities list {:?} truncated to length {}",
				next_authorities,
				T::MaxAuthorities::get(),
			);
		}
		let bounded_next_authorities =
			BoundedVec::<_, T::MaxAuthorities>::truncate_from(next_authorities);

		let next_queued_authorities = queued_validators.map(|(_, k)| k).collect::<Vec<_>>();
		if next_queued_authorities.len() as u32 > T::MaxAuthorities::get() {
			log::error!(
				target: LOG_TARGET,
				"queued authorities list {:?} truncated to length {}",
				next_queued_authorities,
				T::MaxAuthorities::get(),
			);
		}
		let bounded_next_queued_authorities =
			BoundedVec::<_, T::MaxAuthorities>::truncate_from(next_queued_authorities);

		// Always issue a change on each `session`, even if validator set hasn't changed.
		// We want to have at least one BEEFY mandatory block per session.
		Self::change_authorities(bounded_next_authorities, bounded_next_queued_authorities);

		let validator_set_id = ValidatorSetId::<T>::get();
		// Update the mapping for the new set id that corresponds to the latest session (i.e. now).
		let session_index = pallet_session::Pallet::<T>::current_index();
		SetIdSession::<T>::insert(validator_set_id, &session_index);
		// Prune old entry if limit reached.
		let max_set_id_session_entries = T::MaxSetIdSessionEntries::get().max(1);
		if validator_set_id >= max_set_id_session_entries {
			SetIdSession::<T>::remove(validator_set_id - max_set_id_session_entries);
		}
	}

	fn on_disabled(i: u32) {
		let log = DigestItem::Consensus(
			BEEFY_ENGINE_ID,
			ConsensusLog::<T::BeefyId>::OnDisabled(i as AuthorityIndex).encode(),
		);

		frame_system::Pallet::<T>::deposit_log(log);
	}
}

impl<T: Config> IsMember<T::BeefyId> for Pallet<T> {
	fn is_member(authority_id: &T::BeefyId) -> bool {
		Authorities::<T>::get().iter().any(|id| id == authority_id)
	}
}

pub trait WeightInfo {
	fn report_voting_equivocation(
		votes_count: u32,
		validator_count: u32,
		max_nominators_per_validator: u32,
	) -> Weight;

	fn set_new_genesis() -> Weight;
}

pub(crate) trait WeightInfoExt: WeightInfo {
	fn report_double_voting(validator_count: u32, max_nominators_per_validator: u32) -> Weight {
		Self::report_voting_equivocation(2, validator_count, max_nominators_per_validator)
	}

	fn report_fork_voting<T: Config>(
		validator_count: u32,
		max_nominators_per_validator: u32,
		ancestry_proof: &<T::AncestryHelper as AncestryHelper<HeaderFor<T>>>::Proof,
	) -> Weight {
		<T::AncestryHelper as AncestryHelperWeightInfo<HeaderFor<T>>>::is_proof_optimal(&ancestry_proof)
			.saturating_add(<T::AncestryHelper as AncestryHelperWeightInfo<HeaderFor<T>>>::extract_validation_context())
			.saturating_add(
				<T::AncestryHelper as AncestryHelperWeightInfo<HeaderFor<T>>>::is_non_canonical(
					ancestry_proof,
				),
			)
			.saturating_add(Self::report_voting_equivocation(
				1,
				validator_count,
				max_nominators_per_validator,
			))
	}

	fn report_future_block_voting(
		validator_count: u32,
		max_nominators_per_validator: u32,
	) -> Weight {
		// checking if the report is for a future block
		DbWeight::get()
			.reads(1)
			// check and report the equivocated vote
			.saturating_add(Self::report_voting_equivocation(
				1,
				validator_count,
				max_nominators_per_validator,
			))
	}
}

impl<T> WeightInfoExt for T where T: WeightInfo {}
