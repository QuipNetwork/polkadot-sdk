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

//! # Executive Module
//!
//! The Executive module acts as the orchestration layer for the runtime. It dispatches incoming
//! extrinsic calls to the respective modules in the runtime.
//!
//! ## Overview
//!
//! The executive module is not a typical pallet providing functionality around a specific feature.
//! It is a cross-cutting framework component for the FRAME. It works in conjunction with the
//! [FRAME System module](../frame_system/index.html) to perform these cross-cutting functions.
//!
//! The Executive module provides functions to:
//!
//! - Check transaction validity.
//! - Initialize a block.
//! - Apply extrinsics.
//! - Execute a block.
//! - Finalize a block.
//! - Start an off-chain worker.
//!
//! The flow of their application in a block is explained in the [block flowchart](block_flowchart).
//!
//! ### Implementations
//!
//! The Executive module provides the following implementations:
//!
//! - `ExecuteBlock`: Trait that can be used to execute a block.
//! - `Executive`: Type that can be used to make the FRAME available from the runtime.
//!
//! ## Usage
//!
//! The default Substrate node template declares the [`Executive`](./struct.Executive.html) type in
//! its library.
//!
//! ### Example
//!
//! `Executive` type declaration from the node template.
//!
//! ```
//! # use sp_runtime::generic;
//! # use frame_executive as executive;
//! # pub struct UncheckedExtrinsic {};
//! # pub struct Header {};
//! # type Context = frame_system::ChainContext<Runtime>;
//! # pub type Block = generic::Block<Header, UncheckedExtrinsic>;
//! # pub type Balances = u64;
//! # pub type AllPalletsWithSystem = u64;
//! # pub enum Runtime {};
//! # use sp_runtime::transaction_validity::{
//! #    TransactionValidity, UnknownTransaction, TransactionSource,
//! # };
//! /// Executive: handles dispatch to the various modules.
//! pub type Executive = executive::Executive<Runtime, Block, Context, Runtime, AllPalletsWithSystem>;
//! ```

#[cfg(doc)]
#[cfg_attr(doc, aquamarine::aquamarine)]
/// # Block Execution
///
/// These are the steps of block execution as done by [`Executive::execute_block`]. A block is
/// invalid if any of them fail.
///
/// ```mermaid
/// flowchart TD
///     Executive::execute_block --> on_runtime_upgrade
///     on_runtime_upgrade --> System::initialize
///     Executive::initialize_block --> System::initialize
///     System::initialize --> on_initialize
///     on_initialize --> PreInherents[System::PreInherents]
///     PreInherents --> Inherents[Apply Inherents]
///     Inherents --> PostInherents[System::PostInherents]
///     PostInherents --> Check{MBM ongoing?}
///     Check -->|No| poll
///     Check -->|Yes| post_transactions_2[System::PostTransaction]
///     post_transactions_2 --> Step[MBMs::step]
///     Step --> on_finalize
///     poll --> transactions[Apply Transactions]
///     transactions --> post_transactions_1[System::PostTransaction]
///     post_transactions_1 --> CheckIdle{Weight remaining?}
///     CheckIdle -->|Yes| on_idle
///     CheckIdle -->|No| on_finalize
///     on_idle --> on_finalize
/// ```
pub mod block_flowchart {}

#[cfg(test)]
mod tests;

extern crate alloc;

use codec::{Codec, Encode};
use core::marker::PhantomData;
use frame_support::{
	defensive_assert,
	dispatch::{DispatchClass, DispatchInfo, GetDispatchInfo, PostDispatchInfo},
	migrations::MultiStepMigrator,
	pallet_prelude::InvalidTransaction,
	traits::{
		BeforeAllRuntimeMigrations, ExecuteBlock, Get, IsInherent, OffchainWorker, OnFinalize,
		OnIdle, OnInitialize, OnPoll, OnRuntimeUpgrade, PostInherents, PostTransactions,
		PreInherents,
	},
	weights::{Weight, WeightMeter},
	MAX_EXTRINSIC_DEPTH,
};
use frame_system::pallet_prelude::BlockNumberFor;
use sp_runtime::{
	generic::Digest,
	traits::{
		self, Applyable, CheckEqual, Checkable, Dispatchable, Header, LazyBlock, NumberFor, One,
		Zero,
	},
	transaction_validity::{TransactionSource, TransactionValidity, TransactionValidityError},
	ApplyExtrinsicResult, ExtrinsicInclusionMode,
};

#[allow(deprecated)]
use sp_runtime::traits::ValidateUnsigned;

#[cfg(feature = "try-runtime")]
use ::{
	frame_support::{
		traits::{TryDecodeEntireStorage, TryDecodeEntireStorageError, TryState},
		StorageNoopGuard,
	},
	frame_try_runtime::{TryStateSelect, UpgradeCheckSelect},
	log,
	sp_runtime::TryRuntimeError,
};

#[allow(dead_code)]
const LOG_TARGET: &str = "runtime::executive";

pub type CheckedOf<E, C> = <E as Checkable<C>>::Checked;
pub type CallOf<E, C> = <CheckedOf<E, C> as Applyable>::Call;
pub type OriginOf<E, C> = <CallOf<E, C> as Dispatchable>::RuntimeOrigin;

/// Configuration for try-runtime upgrade checks.
#[cfg(feature = "try-runtime")]
#[derive(Debug, Clone)]
pub struct TryRuntimeUpgradeConfig {
	/// Whether to execute `pre/post_upgrade` and `try_state` hooks.
	pub checks: UpgradeCheckSelect,
	/// Which pallets' try_state hooks to execute.
	pub try_state_select: TryStateSelect,
}

#[cfg(feature = "try-runtime")]
impl TryRuntimeUpgradeConfig {
	/// Create a new config with default settings (run all checks for all pallets).
	pub fn new(checks: UpgradeCheckSelect) -> Self {
		Self { checks, try_state_select: TryStateSelect::All }
	}

	/// Set which pallets' try_state hooks to execute.
	pub fn with_try_state_select(mut self, try_state_select: TryStateSelect) -> Self {
		self.try_state_select = try_state_select;
		self
	}
}

#[derive(PartialEq)]
pub enum ExecutiveError {
	UnableToDecodeExtrinsic,
	InvalidInherentPosition(usize),
	OnlyInherentsAllowed,
	ApplyExtrinsic(TransactionValidityError),
	Custom(&'static str),
}

impl core::fmt::Debug for ExecutiveError {
	fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
		match self {
			ExecutiveError::UnableToDecodeExtrinsic => {
				write!(fmt, "The extrinsic could not be decoded correctly")
			},
			ExecutiveError::InvalidInherentPosition(i) => {
				write!(fmt, "Invalid inherent position for extrinsic at index {}", i)
			},
			ExecutiveError::OnlyInherentsAllowed => {
				write!(fmt, "Only inherents are allowed in this block")
			},
			ExecutiveError::ApplyExtrinsic(e) => write!(
				fmt,
				"ExecuteBlockError applying extrinsic: {}",
				Into::<&'static str>::into(*e)
			),
			ExecutiveError::Custom(err) => write!(fmt, "{err}"),
		}
	}
}

/// Main entry point for certain runtime actions as e.g. `execute_block`.
///
/// Generic parameters:
/// - `System`: Something that implements `frame_system::Config`
/// - `Block`: The block type of the runtime
/// - `Context`: The context that is used when checking an extrinsic.
/// - `UnsignedValidator`: The unsigned transaction validator of the runtime.
/// - `AllPalletsWithSystem`: Tuple that contains all pallets including frame system pallet. Will be
///   used to call hooks e.g. `on_initialize`.
/// - [**DEPRECATED** `OnRuntimeUpgrade`]: This parameter is deprecated and will be removed after
///   September 2026. Use type `SingleBlockMigrations` in frame_system::Config instead.
#[allow(deprecated)]
pub struct Executive<
	System,
	Block,
	Context,
	UnsignedValidator,
	AllPalletsWithSystem,
	OnRuntimeUpgrade = (),
>(
	PhantomData<(
		System,
		Block,
		Context,
		UnsignedValidator,
		AllPalletsWithSystem,
		OnRuntimeUpgrade,
	)>,
);

/// TODO: The `OnRuntimeUpgrade` generic parameter in `Executive` is deprecated and will be
/// removed in a future version. Once removed, this `#[allow(deprecated)]` attribute
/// can be safely deleted.
#[allow(deprecated)]
impl<
		System: frame_system::Config + IsInherent<Block::Extrinsic>,
		Block: traits::Block<
			Header = frame_system::pallet_prelude::HeaderFor<System>,
			Hash = System::Hash,
		>,
		Context: Default,
		UnsignedValidator,
		AllPalletsWithSystem: OnRuntimeUpgrade
			+ BeforeAllRuntimeMigrations
			+ OnInitializeWithWeightRegistration<System>
			+ OnIdle<BlockNumberFor<System>>
			+ OnFinalize<BlockNumberFor<System>>
			+ OffchainWorker<BlockNumberFor<System>>
			+ OnPoll<BlockNumberFor<System>>,
		COnRuntimeUpgrade: OnRuntimeUpgrade,
	> ExecuteBlock<Block>
	for Executive<System, Block, Context, UnsignedValidator, AllPalletsWithSystem, COnRuntimeUpgrade>
where
	Block::Extrinsic: Checkable<Context> + Codec,
	CheckedOf<Block::Extrinsic, Context>: Applyable + GetDispatchInfo,
	CallOf<Block::Extrinsic, Context>:
		Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	OriginOf<Block::Extrinsic, Context>: From<Option<System::AccountId>>,
	UnsignedValidator: ValidateUnsigned<Call = CallOf<Block::Extrinsic, Context>>,
{
	fn verify_and_remove_seal(_: &mut <Block as traits::Block>::LazyBlock) {
		// Nothing to do here.
	}

	fn execute_verified_block(block: Block::LazyBlock) {
		Executive::<
			System,
			Block,
			Context,
			UnsignedValidator,
			AllPalletsWithSystem,
			COnRuntimeUpgrade,
		>::execute_block(block);
	}
}

/// TODO: The `OnRuntimeUpgrade` generic parameter in `Executive` is deprecated and will be
/// removed in a future version. Once removed, this `#[allow(deprecated)]` attribute
/// can be safely deleted.
#[allow(deprecated)]
#[cfg(feature = "try-runtime")]
impl<
		System: frame_system::Config + IsInherent<Block::Extrinsic>,
		Block: traits::Block<
			Header = frame_system::pallet_prelude::HeaderFor<System>,
			Hash = System::Hash,
		>,
		Context: Default,
		UnsignedValidator,
		AllPalletsWithSystem: OnRuntimeUpgrade
			+ BeforeAllRuntimeMigrations
			+ OnInitializeWithWeightRegistration<System>
			+ OnIdle<BlockNumberFor<System>>
			+ OnFinalize<BlockNumberFor<System>>
			+ OffchainWorker<BlockNumberFor<System>>
			+ OnPoll<BlockNumberFor<System>>
			+ TryState<BlockNumberFor<System>>
			+ TryDecodeEntireStorage,
		COnRuntimeUpgrade: OnRuntimeUpgrade,
	> Executive<System, Block, Context, UnsignedValidator, AllPalletsWithSystem, COnRuntimeUpgrade>
where
	Block::Extrinsic: Checkable<Context> + Codec,
	CheckedOf<Block::Extrinsic, Context>: Applyable + GetDispatchInfo,
	CallOf<Block::Extrinsic, Context>:
		Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	OriginOf<Block::Extrinsic, Context>: From<Option<System::AccountId>>,
	UnsignedValidator: ValidateUnsigned<Call = CallOf<Block::Extrinsic, Context>>,
{
	/// Execute given block, but don't as strict is the normal block execution.
	///
	/// Some checks can be disabled via:
	///
	/// - `state_root_check`
	/// - `signature_check`
	///
	/// Should only be used for testing ONLY.
	pub fn try_execute_block(
		block: Block::LazyBlock,
		state_root_check: bool,
		signature_check: bool,
		select: frame_try_runtime::TryStateSelect,
	) -> Result<Weight, ExecutiveError> {
		log::info!(
			target: LOG_TARGET,
			"try-runtime: executing block #{:?} / state root check: {:?} / signature check: {:?} / try-state-select: {:?}",
			block.header().number(),
			state_root_check,
			signature_check,
			select,
		);

		let mode = Self::initialize_block(block.header());
		Self::initial_checks(block.header());

		// Apply extrinsics:
		let signature_check = if signature_check {
			Block::Extrinsic::check
		} else {
			Block::Extrinsic::unchecked_into_checked_i_know_what_i_am_doing
		};
		Self::apply_extrinsics(mode, block.extrinsics(), |uxt, is_inherent| {
			Self::do_apply_extrinsic(uxt, is_inherent, signature_check)
		})?;

		// In this case there were no transactions to trigger this state transition:
		if !<frame_system::Pallet<System>>::inherents_applied() {
			Self::inherents_applied();
		}

		// post-extrinsics book-keeping
		<frame_system::Pallet<System>>::note_finished_extrinsics();
		<System as frame_system::Config>::PostTransactions::post_transactions();

		let header = block.header();
		Self::on_idle_hook(*header.number());
		Self::on_finalize_hook(*header.number());

		// run the try-state checks of all pallets, ensuring they don't alter any state.
		let _guard = frame_support::StorageNoopGuard::default();
		<AllPalletsWithSystem as frame_support::traits::TryState<
			BlockNumberFor<System>,
		>>::try_state(*header.number(), select.clone())
		.map_err(|e| {
			log::error!(target: LOG_TARGET, "failure: {:?}", e);
			ExecutiveError::Custom(e.into())
		})?;
		if select.any() {
			let res = AllPalletsWithSystem::try_decode_entire_state();
			Self::log_decode_result(res).map_err(|e| ExecutiveError::Custom(e.into()))?;
		}
		drop(_guard);

		// do some of the checks that would normally happen in `final_checks`, but perhaps skip
		// the state root check.
		{
			let new_header = <frame_system::Pallet<System>>::finalize();
			let items_zip = header.digest().logs().iter().zip(new_header.digest().logs().iter());
			for (header_item, computed_item) in items_zip {
				header_item.check_equal(computed_item);
				assert!(header_item == computed_item, "Digest item must match that calculated.");
			}

			if state_root_check {
				let storage_root = new_header.state_root();
				header.state_root().check_equal(storage_root);
				assert!(
					header.state_root() == storage_root,
					"Storage root must match that calculated."
				);
			}

			assert!(
				header.extrinsics_root() == new_header.extrinsics_root(),
				"Transaction trie root must be valid.",
			);
		}

		log::info!(
			target: LOG_TARGET,
			"try-runtime: Block #{:?} successfully executed",
			header.number(),
		);

		Ok(frame_system::Pallet::<System>::block_weight().total())
	}

	/// Execute all Migrations of this runtime.
	///
	/// The `checks` param determines whether to execute `pre/post_upgrade` and `try_state` hooks.
	///
	/// [`frame_system::LastRuntimeUpgrade`] is set to the current runtime version after
	/// migrations execute. This is important for idempotency checks, because some migrations use
	/// this value to determine whether or not they should execute.
	///
	/// This function runs `try_state` hooks for all pallets. Use
	/// [`Self::try_runtime_upgrade_with_config`] if you need more control over which pallets'
	/// `try_state` hooks to execute.
	pub fn try_runtime_upgrade(checks: UpgradeCheckSelect) -> Result<Weight, TryRuntimeError> {
		Self::try_runtime_upgrade_with_config(TryRuntimeUpgradeConfig::new(checks))
	}

	/// Execute all Migrations of this runtime with custom configuration.
	///
	/// This function provides more granular control over runtime upgrade testing compared to
	/// [`Self::try_runtime_upgrade`]. Use [`TryRuntimeUpgradeConfig`] to specify which checks
	/// to run and which pallets' try_state hooks to execute.
	///
	/// [`frame_system::LastRuntimeUpgrade`] is set to the current runtime version after
	/// migrations execute. This is important for idempotency checks, because some migrations use
	/// this value to determine whether or not they should execute.
	pub fn try_runtime_upgrade_with_config(
		config: TryRuntimeUpgradeConfig,
	) -> Result<Weight, TryRuntimeError> {
		let checks = config.checks;
		let try_state_select = config.try_state_select;
		let before_all_weight =
			<AllPalletsWithSystem as BeforeAllRuntimeMigrations>::before_all_runtime_migrations();

		let try_on_runtime_upgrade_weight =
			<(
				COnRuntimeUpgrade,
				<System as frame_system::Config>::SingleBlockMigrations,
				// We want to run the migrations before we call into the pallets as they may
				// access any state that would then not be migrated.
				AllPalletsWithSystem,
			) as OnRuntimeUpgrade>::try_on_runtime_upgrade(checks.pre_and_post())?;

		frame_system::LastRuntimeUpgrade::<System>::put(
			frame_system::LastRuntimeUpgradeInfo::from(
				<System::Version as frame_support::traits::Get<_>>::get(),
			),
		);

		// Nothing should modify the state after the migrations ran:
		let _guard = StorageNoopGuard::default();

		// The state must be decodable:
		if checks.any() {
			let res = AllPalletsWithSystem::try_decode_entire_state();
			Self::log_decode_result(res)?;
		}

		// Check all storage invariants:
		if checks.try_state() {
			AllPalletsWithSystem::try_state(
				frame_system::Pallet::<System>::block_number(),
				try_state_select,
			)?;
		}

		Ok(before_all_weight.saturating_add(try_on_runtime_upgrade_weight))
	}

	/// Logs the result of trying to decode the entire state.
	fn log_decode_result(
		res: Result<usize, alloc::vec::Vec<TryDecodeEntireStorageError>>,
	) -> Result<(), TryRuntimeError> {
		match res {
			Ok(bytes) => {
				log::info!(
					target: LOG_TARGET,
					"✅ Entire runtime state decodes without error. {} bytes total.",
					bytes
				);

				Ok(())
			},
			Err(errors) => {
				log::error!(
					target: LOG_TARGET,
					"`try_decode_entire_state` failed with {} errors",
					errors.len(),
				);

				for (i, err) in errors.iter().enumerate() {
					// We log the short version to `error` and then the full debug info to `debug`:
					log::error!(target: LOG_TARGET, "- {i}. error: {err}");
					log::debug!(target: LOG_TARGET, "- {i}. error: {err:?}");
				}

				Err("`try_decode_entire_state` failed".into())
			},
		}
	}
}

/// Extension trait for [`OnInitialize`].
///
/// It takes care to register the weight of each pallet directly after executing its
/// `on_initialize`.
///
/// The trait is sealed.
pub trait OnInitializeWithWeightRegistration<T: frame_system::Config> {
	/// The actual logic that calls `on_initialize` and registers the weight.
	fn on_initialize_with_weight_registration(_n: BlockNumberFor<T>) -> Weight;
}

frame_support::impl_for_tuples_attr! {
	#[tuple_types_custom_trait_bound(OnInitialize<frame_system::pallet_prelude::BlockNumberFor<T>>)]
	impl<T: frame_system::Config> OnInitializeWithWeightRegistration<T> for Tuple {
		fn on_initialize_with_weight_registration(n: BlockNumberFor<T>) -> Weight {
			let mut weight = Weight::zero();
			for_tuples!( #(
				let individual_weight = Tuple::on_initialize(n);

				<frame_system::Pallet<T>>::register_extra_weight_unchecked(
					individual_weight,
					DispatchClass::Mandatory,
				);

				weight = weight.saturating_add(individual_weight);
			)* );

			weight
		}
	}
}

/// TODO: The `OnRuntimeUpgrade` generic parameter in `Executive` is deprecated and will be
/// removed in a future version. Once removed, this `#[allow(deprecated)]` attribute
/// can be safely deleted.
#[allow(deprecated)]
impl<
		System: frame_system::Config + IsInherent<Block::Extrinsic>,
		Block: traits::Block<
			Header = frame_system::pallet_prelude::HeaderFor<System>,
			Hash = System::Hash,
		>,
		Context: Default,
		UnsignedValidator,
		AllPalletsWithSystem: OnRuntimeUpgrade
			+ BeforeAllRuntimeMigrations
			+ OnInitializeWithWeightRegistration<System>
			+ OnIdle<BlockNumberFor<System>>
			+ OnFinalize<BlockNumberFor<System>>
			+ OffchainWorker<BlockNumberFor<System>>
			+ OnPoll<BlockNumberFor<System>>,
		COnRuntimeUpgrade: OnRuntimeUpgrade,
	> Executive<System, Block, Context, UnsignedValidator, AllPalletsWithSystem, COnRuntimeUpgrade>
where
	Block::Extrinsic: Checkable<Context> + Codec,
	CheckedOf<Block::Extrinsic, Context>: Applyable + GetDispatchInfo,
	CallOf<Block::Extrinsic, Context>:
		Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	OriginOf<Block::Extrinsic, Context>: From<Option<System::AccountId>>,
	UnsignedValidator: ValidateUnsigned<Call = CallOf<Block::Extrinsic, Context>>,
{
	/// Execute all `OnRuntimeUpgrade` of this runtime, and return the aggregate weight.
	pub fn execute_on_runtime_upgrade() -> Weight {
		let before_all_weight =
			<AllPalletsWithSystem as BeforeAllRuntimeMigrations>::before_all_runtime_migrations();

		let runtime_upgrade_weight = <(
			COnRuntimeUpgrade,
			<System as frame_system::Config>::SingleBlockMigrations,
			// We want to run the migrations before we call into the pallets as they may
			// access any state that would then not be migrated.
			AllPalletsWithSystem,
		) as OnRuntimeUpgrade>::on_runtime_upgrade();

		before_all_weight.saturating_add(runtime_upgrade_weight)
	}

	/// Start the execution of a particular block.
	pub fn initialize_block(
		header: &frame_system::pallet_prelude::HeaderFor<System>,
	) -> ExtrinsicInclusionMode {
		sp_io::init_tracing();
		sp_tracing::enter_span!(sp_tracing::Level::TRACE, "init_block");
		let digests = Self::extract_pre_digest(header);
		Self::initialize_block_impl(header.number(), header.parent_hash(), &digests);

		Self::extrinsic_mode()
	}

	fn extrinsic_mode() -> ExtrinsicInclusionMode {
		if <System as frame_system::Config>::MultiBlockMigrator::ongoing() {
			ExtrinsicInclusionMode::OnlyInherents
		} else {
			ExtrinsicInclusionMode::AllExtrinsics
		}
	}

	fn extract_pre_digest(header: &frame_system::pallet_prelude::HeaderFor<System>) -> Digest {
		let mut digest = <Digest>::default();
		header.digest().logs().iter().for_each(|d| {
			if d.as_pre_runtime().is_some() {
				digest.push(d.clone())
			}
		});
		digest
	}

	fn initialize_block_impl(
		block_number: &BlockNumberFor<System>,
		parent_hash: &System::Hash,
		digest: &Digest,
	) {
		// Reset events before apply runtime upgrade hook.
		// This is required to preserve events from runtime upgrade hook.
		// This means the format of all the event related storage must always be compatible.
		<frame_system::Pallet<System>>::reset_events();

		let mut weight = Weight::zero();
		if Self::runtime_upgraded() {
			weight = weight.saturating_add(Self::execute_on_runtime_upgrade());

			frame_system::LastRuntimeUpgrade::<System>::put(
				frame_system::LastRuntimeUpgradeInfo::from(
					<System::Version as frame_support::traits::Get<_>>::get(),
				),
			);
		}
		<frame_system::Pallet<System>>::initialize(block_number, parent_hash, digest);

		weight = System::BlockWeights::get().base_block.saturating_add(weight);
		// Register the base block weight and optional `on_runtime_upgrade` weight.
		<frame_system::Pallet<System>>::register_extra_weight_unchecked(
			weight,
			DispatchClass::Mandatory,
		);

		weight = weight
			.saturating_add(<AllPalletsWithSystem as OnInitializeWithWeightRegistration<
			System,
		>>::on_initialize_with_weight_registration(*block_number));

		log::debug!(
			target: LOG_TARGET,
			"[{block_number:?}]: Block initialization weight consumption: {weight:?}",
		);

		frame_system::Pallet::<System>::note_finished_initialize();
		<System as frame_system::Config>::PreInherents::pre_inherents();
	}

	/// Returns if the runtime has been upgraded, based on [`frame_system::LastRuntimeUpgrade`].
	fn runtime_upgraded() -> bool {
		let last = frame_system::LastRuntimeUpgrade::<System>::get();
		let current = <System::Version as frame_support::traits::Get<_>>::get();

		last.map(|v| v.was_upgraded(&current)).unwrap_or(true)
	}

	fn initial_checks(header: &Block::Header) {
		sp_tracing::enter_span!(sp_tracing::Level::TRACE, "initial_checks");

		// Check that `parent_hash` is correct.
		let n = *header.number();
		assert!(
			n > BlockNumberFor::<System>::zero() &&
				<frame_system::Pallet<System>>::block_hash(n - BlockNumberFor::<System>::one()) ==
					*header.parent_hash(),
			"Parent hash should be valid.",
		);
	}

	/// Actually execute all transitions for `block`.
	pub fn execute_block(block: Block::LazyBlock) {
		sp_io::init_tracing();
		sp_tracing::within_span! {
			sp_tracing::info_span!("execute_block", ?block);
			// Execute `on_runtime_upgrade` and `on_initialize`.
			let mode = Self::initialize_block(block.header());
			Self::initial_checks(block.header());

			let extrinsics = block.extrinsics();
			if let Err(e) = Self::apply_extrinsics(
				mode,
				extrinsics,
				|uxt, is_inherent| {
					Self::do_apply_extrinsic(uxt, is_inherent, Block::Extrinsic::check)
				}
			) {
				panic!("{:?}", e)
			}

			// In this case there were no transactions to trigger this state transition:
			if !<frame_system::Pallet<System>>::inherents_applied() {
				Self::inherents_applied();
			}

			<frame_system::Pallet<System>>::note_finished_extrinsics();
			<System as frame_system::Config>::PostTransactions::post_transactions();

			let header = block.header();
			Self::on_idle_hook(*header.number());
			Self::on_finalize_hook(*header.number());
			<frame_system::Pallet<System>>::maybe_apply_pending_code_upgrade();
			Self::final_checks(&header);
		}
	}

	/// Logic that runs directly after inherent application.
	///
	/// It advances the Multi-Block-Migrations or runs the `on_poll` hook.
	pub fn inherents_applied() {
		<frame_system::Pallet<System>>::note_inherents_applied();
		<System as frame_system::Config>::PostInherents::post_inherents();

		if <System as frame_system::Config>::MultiBlockMigrator::ongoing() {
			let used_weight = <System as frame_system::Config>::MultiBlockMigrator::step();
			<frame_system::Pallet<System>>::register_extra_weight_unchecked(
				used_weight,
				DispatchClass::Mandatory,
			);
		} else {
			let block_number = <frame_system::Pallet<System>>::block_number();
			Self::on_poll_hook(block_number);
		}
	}

	/// Execute given extrinsics.
	fn apply_extrinsics(
		mode: ExtrinsicInclusionMode,
		extrinsics: impl Iterator<Item = Result<Block::Extrinsic, codec::Error>>,
		mut apply_extrinsic: impl FnMut(Block::Extrinsic, bool) -> ApplyExtrinsicResult,
	) -> Result<(), ExecutiveError> {
		let mut first_non_inherent_idx = 0;
		for (idx, maybe_uxt) in extrinsics.into_iter().enumerate() {
			let uxt = maybe_uxt.map_err(|_| ExecutiveError::UnableToDecodeExtrinsic)?;
			let is_inherent = System::is_inherent(&uxt);
			if is_inherent {
				// Check if inherents are first
				if first_non_inherent_idx != idx {
					return Err(ExecutiveError::InvalidInherentPosition(idx));
				}
				first_non_inherent_idx += 1;
			} else {
				// Check if there are any forbidden non-inherents in the block.
				if mode == ExtrinsicInclusionMode::OnlyInherents {
					return Err(ExecutiveError::OnlyInherentsAllowed);
				}
			}

			log::debug!(target: LOG_TARGET, "Executing transaction: {:?}", uxt);
			if let Err(e) = apply_extrinsic(uxt, is_inherent) {
				log::error!(
					target: LOG_TARGET,
					"Transaction({idx}) failed due to {e:?}. \
					Aborting the rest of the block execution.",
				);
				return Err(ExecutiveError::ApplyExtrinsic(e.into()));
			}
		}

		Ok(())
	}

	/// Finalize the block - it is up the caller to ensure that all header fields are valid
	/// except state-root.
	// Note: Only used by the block builder - not Executive itself.
	pub fn finalize_block() -> frame_system::pallet_prelude::HeaderFor<System> {
		sp_io::init_tracing();
		sp_tracing::enter_span!(sp_tracing::Level::TRACE, "finalize_block");

		// In this case there were no transactions to trigger this state transition:
		if !<frame_system::Pallet<System>>::inherents_applied() {
			Self::inherents_applied();
		}

		<frame_system::Pallet<System>>::note_finished_extrinsics();
		<System as frame_system::Config>::PostTransactions::post_transactions();
		let block_number = <frame_system::Pallet<System>>::block_number();
		Self::on_idle_hook(block_number);
		Self::on_finalize_hook(block_number);
		<frame_system::Pallet<System>>::maybe_apply_pending_code_upgrade();
		<frame_system::Pallet<System>>::finalize()
	}

	/// Run the `on_idle` hook of all pallet, but only if there is weight remaining and there are no
	/// ongoing MBMs.
	fn on_idle_hook(block_number: NumberFor<Block>) {
		if <System as frame_system::Config>::MultiBlockMigrator::ongoing() {
			return;
		}

		let weight = <frame_system::Pallet<System>>::block_weight();
		let max_weight = <System::BlockWeights as frame_support::traits::Get<_>>::get().max_block;
		let remaining_weight = max_weight.saturating_sub(weight.total());

		if remaining_weight.all_gt(Weight::zero()) {
			let used_weight = <AllPalletsWithSystem as OnIdle<BlockNumberFor<System>>>::on_idle(
				block_number,
				remaining_weight,
			);
			<frame_system::Pallet<System>>::register_extra_weight_unchecked(
				used_weight,
				DispatchClass::Mandatory,
			);
		}
	}

	fn on_poll_hook(block_number: NumberFor<Block>) {
		defensive_assert!(
			!<System as frame_system::Config>::MultiBlockMigrator::ongoing(),
			"on_poll should not be called during migrations"
		);

		let weight = <frame_system::Pallet<System>>::block_weight();
		let max_weight = <System::BlockWeights as frame_support::traits::Get<_>>::get().max_block;
		let remaining = max_weight.saturating_sub(weight.total());

		if remaining.all_gt(Weight::zero()) {
			let mut meter = WeightMeter::with_limit(remaining);
			<AllPalletsWithSystem as OnPoll<BlockNumberFor<System>>>::on_poll(
				block_number,
				&mut meter,
			);
			<frame_system::Pallet<System>>::register_extra_weight_unchecked(
				meter.consumed(),
				DispatchClass::Mandatory,
			);
		}
	}

	/// Run the `on_finalize` hook of all pallet.
	fn on_finalize_hook(block_number: NumberFor<Block>) {
		<AllPalletsWithSystem as OnFinalize<BlockNumberFor<System>>>::on_finalize(block_number);
	}

	/// Apply extrinsic outside of the block execution function.
	///
	/// This doesn't attempt to validate anything regarding the block, but it builds a list of uxt
	/// hashes.
	fn do_apply_extrinsic(
		uxt: Block::Extrinsic,
		is_inherent: bool,
		check: impl FnOnce(
			Block::Extrinsic,
			&Context,
		) -> Result<CheckedOf<Block::Extrinsic, Context>, TransactionValidityError>,
	) -> ApplyExtrinsicResult {
		sp_io::init_tracing();
		let encoded = uxt.encode();
		let encoded_len = encoded.len();
		sp_tracing::enter_span!(sp_tracing::info_span!(
			"apply_extrinsic",
			ext=?sp_core::hexdisplay::HexDisplay::from(&encoded)
		));

		let uxt = <Block::Extrinsic as codec::DecodeLimit>::decode_all_with_depth_limit(
			MAX_EXTRINSIC_DEPTH,
			&mut &encoded[..],
		)
		.map_err(|_| InvalidTransaction::Call)?;

		// Verify that the signature is good.
		let xt = check(uxt, &Context::default())?;

		let dispatch_info = xt.get_dispatch_info();

		if !is_inherent && !<frame_system::Pallet<System>>::inherents_applied() {
			Self::inherents_applied();
		}

		// We don't need to make sure to `note_extrinsic` only after we know it's going to be
		// executed to prevent it from leaking in storage since at this point, it will either
		// execute or panic (and revert storage changes).
		<frame_system::Pallet<System>>::note_extrinsic(encoded);

		// AUDIT: Under no circumstances may this function panic from here onwards.

		let r = Applyable::apply::<UnsignedValidator>(xt, &dispatch_info, encoded_len)?;

		// Mandatory(inherents) are not allowed to fail.
		//
		// The entire block should be discarded if an inherent fails to apply. Otherwise
		// it may open an attack vector.
		if r.is_err() && dispatch_info.class == DispatchClass::Mandatory {
			return Err(InvalidTransaction::BadMandatory.into());
		}

		<frame_system::Pallet<System>>::note_applied_extrinsic(&r, dispatch_info);

		Ok(r.map(|_| ()).map_err(|e| e.error))
	}

	/// Apply extrinsic outside of the block execution function.
	///
	/// This doesn't attempt to validate anything regarding the block, but it builds a list of uxt
	/// hashes.
	pub fn apply_extrinsic(uxt: Block::Extrinsic) -> ApplyExtrinsicResult {
		let is_inherent = System::is_inherent(&uxt);
		Self::do_apply_extrinsic(uxt, is_inherent, Block::Extrinsic::check)
	}

	fn final_checks(header: &frame_system::pallet_prelude::HeaderFor<System>) {
		sp_tracing::enter_span!(sp_tracing::Level::TRACE, "final_checks");
		// remove temporaries
		let new_header = <frame_system::Pallet<System>>::finalize();

		// check digest
		assert_eq!(
			header.digest().logs().len(),
			new_header.digest().logs().len(),
			"Number of digest items must match that calculated."
		);
		let items_zip = header.digest().logs().iter().zip(new_header.digest().logs().iter());
		for (header_item, computed_item) in items_zip {
			header_item.check_equal(computed_item);
			assert!(header_item == computed_item, "Digest item must match that calculated.");
		}

		// check storage root.
		let storage_root = new_header.state_root();
		header.state_root().check_equal(storage_root);
		assert!(header.state_root() == storage_root, "Storage root must match that calculated.");

		assert!(
			header.extrinsics_root() == new_header.extrinsics_root(),
			"Transaction trie root must be valid.",
		);
	}

	/// Check a given signed transaction for validity. This doesn't execute any
	/// side-effects; it merely checks whether the transaction would panic if it were included or
	/// not.
	///
	/// Changes made to storage should be discarded.
	pub fn validate_transaction(
		source: TransactionSource,
		uxt: Block::Extrinsic,
		block_hash: Block::Hash,
	) -> TransactionValidity {
		sp_io::init_tracing();
		use sp_tracing::{enter_span, within_span};

		<frame_system::Pallet<System>>::initialize(
			&(frame_system::Pallet::<System>::block_number() + One::one()),
			&block_hash,
			&Default::default(),
		);

		enter_span! { sp_tracing::Level::TRACE, "validate_transaction" };

		let encoded = within_span! { sp_tracing::Level::TRACE, "using_encoded";
			uxt.encode()
		};

		let uxt = <Block::Extrinsic as codec::DecodeLimit>::decode_all_with_depth_limit(
			MAX_EXTRINSIC_DEPTH,
			&mut &encoded[..],
		)
		.map_err(|_| InvalidTransaction::Call)?;

		let xt = within_span! { sp_tracing::Level::TRACE, "check";
			uxt.check(&Default::default())
		}?;

		let dispatch_info = within_span! { sp_tracing::Level::TRACE, "dispatch_info";
			xt.get_dispatch_info()
		};

		if dispatch_info.class == DispatchClass::Mandatory {
			return Err(InvalidTransaction::MandatoryValidation.into());
		}

		within_span! {
			sp_tracing::Level::TRACE, "validate";
			xt.validate::<UnsignedValidator>(source, &dispatch_info, encoded.len())
		}
	}

	/// Start an offchain worker and generate extrinsics.
	pub fn offchain_worker(header: &frame_system::pallet_prelude::HeaderFor<System>) {
		sp_io::init_tracing();
		// We need to keep events available for offchain workers,
		// hence we initialize the block manually.
		// OffchainWorker RuntimeApi should skip initialization.
		let digests = header.digest().clone();

		// Let's deposit all the logs we are not yet aware of. These are the logs set by the `node`.
		let existing_digest = frame_system::Pallet::<System>::digest();
		for digest in digests.logs().iter().filter(|d| !existing_digest.logs.contains(d)) {
			frame_system::Pallet::<System>::deposit_log(digest.clone());
		}

		// Initialize the intra block entropy, which is maybe used by offchain workers.
		frame_system::Pallet::<System>::initialize_intra_block_entropy(header.parent_hash());

		// Frame system only inserts the parent hash into the block hashes as normally we don't know
		// the hash for the header before. However, here we are aware of the hash and we can add it
		// as well.
		frame_system::BlockHash::<System>::insert(header.number(), header.hash());

		<AllPalletsWithSystem as OffchainWorker<BlockNumberFor<System>>>::offchain_worker(
			*header.number(),
		)
	}
}
