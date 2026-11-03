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

//! ## The unsigned phase, and its miner.
//!
//! This pallet deals with unsigned submissions. These are backup, "possibly" multi-page submissions
//! from validators.
//!
//! This pallet has two miners, described in [`unsigned::miner`].
//!
//! As it stands, a validator can, during the unsigned phase, submit up to
//! [`unsigned::Config::MinerPages`] pages. While this can be more than 1, it can likely not be a
//! full, high quality solution. This is because unsigned validator solutions are verified on the
//! fly, all within a single block. The exact value of this parameter should be determined by the
//! benchmarks of a runtime.
//!
//! We could implement a protocol to allow multi-block, multi-page collaborative submissions from
//! different validators, but it is not trivial. Moreover, recall that the unsigned phase is merely
//! a backup and we should primarily rely on offchain staking miners to fulfill this role during
//! `Phase::Signed`.
//!
//! ## Future Idea: Multi-Page unsigned submission
//!
//! the following is the idea of how to implement multi-page unsigned, which we don't have.
//!
//! All validators will run their miners and compute the full paginated solution. They submit all
//! pages as individual unsigned transactions to their local tx-pool.
//!
//! Upon validation, if any page is now present the corresponding transaction is dropped.
//!
//! At each block, the first page that may be valid is included as a high priority operational
//! transaction. This page is validated on the fly to be correct. Since this transaction is sourced
//! from a validator, we can panic if they submit an invalid transaction.
//!
//! Then, once the final page is submitted, some extra checks are done, as explained in
//! [`crate::verifier`]:
//!
//! 1. bounds
//! 2. total score
//!
//! These checks might still fail. If they do, the solution is dropped. At this point, we don't know
//! which validator may have submitted a slightly-faulty solution.
//!
//! In order to prevent this, the transaction validation process always includes a check to ensure
//! all of the previous pages that have been submitted match what the local validator has computed.
//! If they match, the validator knows that they are putting skin in a game that is valid.
//!
//! If any bad paged are detected, the next validator can bail. This process means:
//!
//! * As long as all validators are honest, and run the same miner code, a correct solution is
//!   found.
//! * As little as one malicious validator can stall the process, but no one is accidentally
//!   slashed, and no panic happens.
//!
//! Alternatively, we can keep track of submitters, and report a slash if it occurs. Or, if
//! the signed process is bullet-proof, we can be okay with the status quo.

/// Export weights
pub use crate::weights::traits::pallet_election_provider_multi_block_unsigned::*;
/// Exports of this pallet
pub use pallet::*;
#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

/// The miner.
pub mod miner;

#[frame_support::pallet]
mod pallet {
	use super::WeightInfo;
	use crate::{
		types::*,
		unsigned::miner::{self},
		verifier::Verifier,
		CommonError,
	};
	use frame_support::pallet_prelude::*;
	use frame_system::{offchain::CreateBare, pallet_prelude::*};
	use sp_runtime::traits::SaturatedConversion;
	use sp_std::prelude::*;

	/// convert a [`crate::CommonError`] to a custom InvalidTransaction with the inner code being
	/// the index of the variant.
	fn base_error_to_invalid(error: CommonError) -> InvalidTransaction {
		let index = error.encode().pop().unwrap_or(0);
		InvalidTransaction::Custom(index)
	}

	pub(crate) type UnsignedWeightsOf<T> = <T as Config>::WeightInfo;

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config: crate::Config + CreateBare<Call<Self>> {
		/// The repeat threshold of the offchain worker.
		///
		/// For example, if it is `5`, that means that at least 5 blocks will elapse between
		/// attempts to submit the worker's solution.
		type OffchainRepeat: Get<BlockNumberFor<Self>>;

		/// The solver used in hte offchain worker miner
		type OffchainSolver: frame_election_provider_support::NposSolver<
			AccountId = Self::AccountId,
		>;

		/// Whether the offchain worker miner would attempt to store the solutions in a local
		/// database and reuse then. If set to `false`, it will try and re-mine solutions every
		/// time.
		type OffchainStorage: Get<bool>;

		/// The priority of the unsigned transaction submitted in the unsigned-phase
		type MinerTxPriority: Get<TransactionPriority>;

		/// The number of pages that the offchain miner will try and submit.
		type MinerPages: Get<PageIndex>;

		/// Runtime weight information of this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Submit an unsigned solution.
		///
		/// This works very much like an inherent, as only the validators are permitted to submit
		/// anything. By default validators will compute this call in their `offchain_worker` hook
		/// and try and submit it back.
		///
		/// This is different from signed page submission mainly in that the solution page is
		/// verified on the fly.
		///
		/// The `paged_solution` may contain at most [`Config::MinerPages`] pages. They are
		/// interpreted as msp -> lsp, as per [`crate::Pallet::msp_range_for`].
		///
		/// For example, if `Pages = 4`, and `MinerPages = 2`, our full snapshot range would be [0,
		/// 1, 2, 3], with 3 being msp. But, in this case, then the `paged_raw_solution.pages` is
		/// expected to correspond to `[snapshot(2), snapshot(3)]`.
		#[pallet::weight((UnsignedWeightsOf::<T>::submit_unsigned(), DispatchClass::Operational))]
		#[pallet::call_index(0)]
		pub fn submit_unsigned(
			origin: OriginFor<T>,
			paged_solution: Box<PagedRawSolution<T::MinerConfig>>,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;
			let error_message = "Invalid unsigned submission must produce invalid block and \
				 deprive validator from their authoring reward.";

			// phase, round, claimed score, page-count and hash are checked in pre-dispatch. we
			// don't check them here anymore.
			debug_assert!(Self::validate_unsigned_checks(&paged_solution).is_ok());

			let claimed_score = paged_solution.score;

			// we select the most significant pages, based on `T::MinerPages`.
			let page_indices = crate::Pallet::<T>::msp_range_for(T::MinerPages::get() as usize);
			<T::Verifier as Verifier>::verify_synchronous_multi(
				paged_solution.solution_pages,
				page_indices,
				claimed_score,
			)
			.expect(error_message);

			Ok(None.into())
		}
	}

	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;
		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			if let Call::submit_unsigned { paged_solution, .. } = call {
				match source {
					TransactionSource::Local | TransactionSource::InBlock => { /* allowed */ },
					_ => return InvalidTransaction::Call.into(),
				}

				let _ = Self::validate_unsigned_checks(paged_solution.as_ref())
					.map_err(|err| {
						sublog!(
							debug,
							"unsigned",
							"unsigned transaction validation failed due to {:?}",
							err
						);
						err
					})
					.map_err(base_error_to_invalid)?;

				ValidTransaction::with_tag_prefix("OffchainElection")
					// The higher the score.minimal_stake, the better a paged_solution is.
					.priority(
						T::MinerTxPriority::get()
							.saturating_add(paged_solution.score.minimal_stake.saturated_into()),
					)
					// Used to deduplicate unsigned solutions: each validator should produce one
					// paged_solution per round at most, and solutions are not propagate.
					.and_provides(paged_solution.round)
					// Transaction should stay in the pool for the duration of the unsigned phase.
					.longevity(T::UnsignedPhase::get().saturated_into::<u64>())
					// We don't propagate this. This can never be validated at a remote node.
					.propagate(false)
					.build()
			} else {
				InvalidTransaction::Call.into()
			}
		}

		fn pre_dispatch(call: &Self::Call) -> Result<(), TransactionValidityError> {
			if let Call::submit_unsigned { paged_solution, .. } = call {
				Self::validate_unsigned_checks(paged_solution.as_ref())
					.map_err(base_error_to_invalid)
					.map_err(Into::into)
			} else {
				Err(InvalidTransaction::Call.into())
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			assert!(
				UnsignedWeightsOf::<T>::submit_unsigned().all_lte(T::BlockWeights::get().max_block),
				"weight of `submit_unsigned` is too high"
			);
			assert!(
				<T as Config>::MinerPages::get() as usize <=
					<T as crate::Config>::Pages::get() as usize,
				"number of pages in the unsigned phase is too high"
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(now: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state(now)
		}

		fn offchain_worker(now: BlockNumberFor<T>) {
			use sp_runtime::offchain::storage_lock::{BlockAndTime, StorageLock};

			// Create a lock with the maximum deadline of number of blocks in the unsigned phase.
			// This should only come useful in an **abrupt** termination of execution, otherwise the
			// guard will be dropped upon successful execution.
			let mut lock =
				StorageLock::<BlockAndTime<frame_system::Pallet<T>>>::with_block_deadline(
					miner::OffchainWorkerMiner::<T>::OFFCHAIN_LOCK,
					T::UnsignedPhase::get().saturated_into(),
				);

			match lock.try_lock() {
				Ok(_guard) => {
					Self::do_synchronized_offchain_worker(now);
				},
				Err(deadline) => {
					sublog!(
						trace,
						"unsigned",
						"offchain worker lock not released, deadline is {:?}",
						deadline
					);
				},
			};
		}
	}

	impl<T: Config> Pallet<T> {
		/// Internal logic of the offchain worker, to be executed only when the offchain lock is
		/// acquired with success.
		fn do_synchronized_offchain_worker(now: BlockNumberFor<T>) {
			use miner::OffchainWorkerMiner;
			let current_phase = crate::Pallet::<T>::current_phase();
			sublog!(
				trace,
				"unsigned",
				"lock for offchain worker acquired. Phase = {:?}",
				current_phase
			);

			// do the repeat frequency check just one, if we are in unsigned phase.
			if current_phase.is_unsigned() {
				if let Err(reason) = OffchainWorkerMiner::<T>::ensure_offchain_repeat_frequency(now)
				{
					sublog!(
						debug,
						"unsigned",
						"offchain worker repeat frequency check failed: {:?}",
						reason
					);
					return;
				}
			}

			if current_phase.is_unsigned_opened_now() {
				// Mine a new solution, (maybe) cache it, and attempt to submit it
				let initial_output = if T::OffchainStorage::get() {
					OffchainWorkerMiner::<T>::mine_check_maybe_save_submit(true)
				} else {
					OffchainWorkerMiner::<T>::mine_check_maybe_save_submit(false)
				};
				sublog!(debug, "unsigned", "initial offchain worker output: {:?}", initial_output);
			} else if current_phase.is_unsigned() {
				// Maybe resubmit the cached solution, else re-compute.
				let resubmit_output = if T::OffchainStorage::get() {
					OffchainWorkerMiner::<T>::restore_or_compute_then_maybe_submit()
				} else {
					OffchainWorkerMiner::<T>::mine_check_maybe_save_submit(false)
				};
				sublog!(debug, "unsigned", "later offchain worker output: {:?}", resubmit_output);
			};
		}

		/// The checks that should happen in the `ValidateUnsigned`'s `pre_dispatch` and
		/// `validate_unsigned` functions.
		///
		/// These check both for snapshot independent checks, and some checks that are specific to
		/// the unsigned phase.
		pub(crate) fn validate_unsigned_checks(
			paged_solution: &PagedRawSolution<T::MinerConfig>,
		) -> Result<(), CommonError> {
			Self::unsigned_specific_checks(paged_solution)
				.and(crate::Pallet::<T>::snapshot_independent_checks(paged_solution, None))
				.map_err(Into::into)
		}

		/// The checks that are specific to the (this) unsigned pallet.
		///
		/// ensure solution has the correct phase, and it has only 1 page.
		pub fn unsigned_specific_checks(
			paged_solution: &PagedRawSolution<T::MinerConfig>,
		) -> Result<(), CommonError> {
			ensure!(
				crate::Pallet::<T>::current_phase().is_unsigned(),
				CommonError::EarlySubmission
			);
			ensure!(
				paged_solution.solution_pages.len() == T::MinerPages::get() as usize,
				CommonError::WrongPageCount
			);
			ensure!(
				paged_solution.solution_pages.len() <= <T as crate::Config>::Pages::get() as usize,
				CommonError::WrongPageCount
			);

			Ok(())
		}

		#[cfg(any(test, feature = "runtime-benchmarks", feature = "try-runtime"))]
		pub(crate) fn do_try_state(
			_now: BlockNumberFor<T>,
		) -> Result<(), sp_runtime::TryRuntimeError> {
			Ok(())
		}
	}
}

#[cfg(test)]
#[allow(deprecated)]
mod validate_unsigned {
	use frame_election_provider_support::Support;
	use frame_support::{
		pallet_prelude::InvalidTransaction,
		unsigned::{TransactionSource, TransactionValidityError, ValidateUnsigned},
	};

	use super::Call;
	use crate::{mock::*, types::*, verifier::Verifier};

	#[test]
	fn retracts_weak_score_accepts_better() {
		ExtBuilder::mock_signed().build_and_execute(|| {
			roll_to_snapshot_created();

			let base_minimal_stake = 55;
			let solution = mine_full_solution().unwrap();
			load_mock_signed_and_start(solution.clone());
			roll_to_full_verification();

			// Some good solution is queued now.
			assert_eq!(
				<VerifierPallet as Verifier>::queued_score(),
				Some(ElectionScore {
					minimal_stake: base_minimal_stake,
					sum_stake: 130,
					sum_stake_squared: 8650
				})
			);

			roll_to_unsigned_open();

			// This is just worse.
			let attempt = fake_solution(ElectionScore {
				minimal_stake: base_minimal_stake - 1,
				..Default::default()
			});
			let call = Call::submit_unsigned { paged_solution: Box::new(attempt) };
			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(2)),
			);

			// This is better, but the number of winners is incorrect.
			let attempt = fake_solution(ElectionScore {
				minimal_stake: base_minimal_stake + 1,
				..Default::default()
			});
			let call = Call::submit_unsigned { paged_solution: Box::new(attempt) };
			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(4)),
			);

			// Note that we now have to use a solution with 2 winners, just to pass all of the
			// snapshot independent checks.
			let mut paged = raw_paged_from_supports(
				vec![vec![
					(40, Support { total: 10, voters: vec![(3, 5)] }),
					(30, Support { total: 10, voters: vec![(3, 5)] }),
				]],
				0,
			);

			paged.score =
				ElectionScore { minimal_stake: base_minimal_stake + 1, ..Default::default() };
			let call = Call::submit_unsigned { paged_solution: Box::new(paged) };
			assert!(UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).is_ok());
		})
	}

	#[test]
	fn retracts_wrong_round() {
		ExtBuilder::mock_signed().build_and_execute(|| {
			roll_to_unsigned_open();

			let mut attempt =
				fake_solution(ElectionScore { minimal_stake: 5, ..Default::default() });
			attempt.round += 1;
			let call = Call::submit_unsigned { paged_solution: Box::new(attempt) };

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				// WrongRound is index 1
				TransactionValidityError::Invalid(InvalidTransaction::Custom(1)),
			);
		})
	}

	#[test]
	fn retracts_too_many_pages_unsigned() {
		ExtBuilder::mock_signed().build_and_execute(|| {
			// NOTE: unsigned solutions should have just 1 page, regardless of the configured
			// page count.
			roll_to_unsigned_open();
			let attempt = mine_full_solution().unwrap();
			let call = Call::submit_unsigned { paged_solution: Box::new(attempt) };

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				// WrongPageCount is index 3
				TransactionValidityError::Invalid(InvalidTransaction::Custom(3)),
			);

			let attempt = mine_solution(2).unwrap();
			let call = Call::submit_unsigned { paged_solution: Box::new(attempt) };

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(3)),
			);

			let attempt = mine_solution(1).unwrap();
			let call = Call::submit_unsigned { paged_solution: Box::new(attempt) };

			assert!(UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).is_ok(),);
		})
	}

	#[test]
	fn retracts_wrong_winner_count() {
		ExtBuilder::mock_signed().desired_targets(2).build_and_execute(|| {
			roll_to_unsigned_open();

			let paged = raw_paged_from_supports(
				vec![vec![(40, Support { total: 10, voters: vec![(3, 10)] })]],
				0,
			);

			let call = Call::submit_unsigned { paged_solution: Box::new(paged) };

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				// WrongWinnerCount is index 4
				TransactionValidityError::Invalid(InvalidTransaction::Custom(4)),
			);
		});
	}

	#[test]
	fn retracts_wrong_phase() {
		ExtBuilder::mock_signed().signed_phase(5, 6).build_and_execute(|| {
			let solution = raw_paged_solution_low_score();
			let call = Call::submit_unsigned { paged_solution: Box::new(solution.clone()) };

			// initial
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
					TransactionSource::Local,
					&call
				)
				.unwrap_err(),
				// because EarlySubmission is index 0.
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));

			// signed
			roll_to_signed_open();
			assert!(MultiBlock::current_phase().is_signed());
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
					TransactionSource::Local,
					&call
				)
				.unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));

			// unsigned
			roll_to_unsigned_open();
			assert!(MultiBlock::current_phase().is_unsigned());

			assert_ok!(<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
				TransactionSource::Local,
				&call
			));
			assert_ok!(<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call));
		})
	}

	#[test]
	fn priority_is_set() {
		ExtBuilder::mock_signed()
			.miner_tx_priority(20)
			.desired_targets(0)
			.build_and_execute(|| {
				roll_to_unsigned_open();
				assert!(MultiBlock::current_phase().is_unsigned());

				let solution =
					fake_solution(ElectionScore { minimal_stake: 5, ..Default::default() });
				let call = Call::submit_unsigned { paged_solution: Box::new(solution.clone()) };

				assert_eq!(
					<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
						TransactionSource::Local,
						&call
					)
					.unwrap()
					.priority,
					25
				);
			})
	}
}

#[cfg(test)]
mod call {
	use crate::{mock::*, verifier::Verifier, Snapshot};

	#[test]
	fn unsigned_submission_e2e() {
		let (mut ext, pool) = ExtBuilder::mock_signed().build_offchainify();
		ext.execute_with_sanity_checks(|| {
			roll_to_unsigned_open();

			// snapshot is created..
			assert_full_snapshot();
			// ..txpool is empty..
			assert_eq!(pool.read().transactions.len(), 0);
			// ..but nothing queued.
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			// now the OCW should submit something.
			roll_next_with_ocw(Some(pool.clone()));
			assert_eq!(pool.read().transactions.len(), 1);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			// and now it should be applied.
			roll_next_with_ocw(Some(pool.clone()));
			assert_eq!(pool.read().transactions.len(), 0);
			assert!(matches!(<VerifierPallet as Verifier>::queued_score(), Some(_)));
		})
	}

	#[test]
	#[should_panic(
		expected = "Invalid unsigned submission must produce invalid block and deprive validator from their authoring reward."
	)]
	fn unfeasible_solution_panics() {
		let (mut ext, pool) = ExtBuilder::mock_signed().build_offchainify();
		ext.execute_with_sanity_checks(|| {
			roll_to_unsigned_open();

			// snapshot is created..
			assert_full_snapshot();
			// ..txpool is empty..
			assert_eq!(pool.read().transactions.len(), 0);
			// ..but nothing queued.
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			// now the OCW should submit something.
			roll_next_with_ocw(Some(pool.clone()));
			assert_eq!(pool.read().transactions.len(), 1);
			assert_eq!(<VerifierPallet as Verifier>::queued_score(), None);

			// now we change the snapshot -- this should ensure that the solution becomes invalid.
			// Note that we don't change the known fingerprint of the solution.
			Snapshot::<Runtime>::remove_target(2);

			// and now it should be applied.
			roll_next_with_ocw(Some(pool.clone()));
			assert_eq!(pool.read().transactions.len(), 0);
			assert!(matches!(<VerifierPallet as Verifier>::queued_score(), Some(_)));
		})
	}
}
