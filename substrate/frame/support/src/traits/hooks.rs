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

//! Traits relating to pallet hooks.
//!
//! See [`Hooks`] as the main entry-point.

#![deny(missing_docs)]

use crate::{impl_for_tuples_attr, weights::Weight};
use sp_runtime::traits::AtLeast32BitUnsigned;
use sp_weights::WeightMeter;

#[cfg(feature = "try-runtime")]
use alloc::vec::Vec;
#[cfg(feature = "try-runtime")]
use sp_runtime::TryRuntimeError;

/// Provides a callback to execute logic before the all inherents.
pub trait PreInherents {
	/// Called before all inherents were applied but after `on_initialize`.
	fn pre_inherents() {}
}

impl_for_tuples_attr! {
	impl PreInherents for Tuple {
		fn pre_inherents() {
			for_tuples!( #( Tuple::pre_inherents(); )* );
		}
	}
}

/// Provides a callback to execute logic after the all inherents.
pub trait PostInherents {
	/// Called after all inherents were applied.
	fn post_inherents() {}
}

impl_for_tuples_attr! {
	impl PostInherents for Tuple {
		fn post_inherents() {
			for_tuples!( #( Tuple::post_inherents(); )* );
		}
	}
}

/// Provides a callback to execute logic after the all transactions.
pub trait PostTransactions {
	/// Called after all transactions were applied but before `on_finalize`.
	fn post_transactions() {}
}

impl_for_tuples_attr! {
	impl PostTransactions for Tuple {
		fn post_transactions() {
			for_tuples!( #( Tuple::post_transactions(); )* );
		}
	}
}

/// Periodically executes logic. Is not guaranteed to run within a specific timeframe and should
/// only be used on logic that has no deadline.
pub trait OnPoll<BlockNumber> {
	/// Code to execute every now and then at the beginning of the block after inherent application.
	///
	/// The remaining weight limit must be respected.
	fn on_poll(_n: BlockNumber, _weight: &mut WeightMeter) {}
}

impl_for_tuples_attr! {
	impl<BlockNumber: Clone> OnPoll<BlockNumber> for Tuple {
		fn on_poll(n: BlockNumber, weight: &mut WeightMeter) {
			for_tuples!( #( Tuple::on_poll(n.clone(), weight); )* );
		}
	}
}

/// See [`Hooks::on_initialize`].
pub trait OnInitialize<BlockNumber> {
	/// See [`Hooks::on_initialize`].
	fn on_initialize(_n: BlockNumber) -> Weight {
		Weight::zero()
	}
}

impl_for_tuples_attr! {
	impl<BlockNumber: Clone> OnInitialize<BlockNumber> for Tuple {
		fn on_initialize(n: BlockNumber) -> Weight {
			let mut weight = Weight::zero();
			for_tuples!( #( weight = weight.saturating_add(Tuple::on_initialize(n.clone())); )* );
			weight
		}
	}
}

impl_for_tuples_attr! {
	/// See [`Hooks::on_finalize`].
	pub trait OnFinalize<BlockNumber> {
		/// See [`Hooks::on_finalize`].
		fn on_finalize(_n: BlockNumber) {}
	}
}

/// See [`Hooks::on_idle`].
pub trait OnIdle<BlockNumber> {
	/// See [`Hooks::on_idle`].
	fn on_idle(_n: BlockNumber, _remaining_weight: Weight) -> Weight {
		Weight::zero()
	}
}

impl_for_tuples_attr! {
	impl<BlockNumber: Copy + AtLeast32BitUnsigned> OnIdle<BlockNumber> for Tuple {
		fn on_idle(n: BlockNumber, remaining_weight: Weight) -> Weight {
			let on_idle_functions: &[fn(BlockNumber, Weight) -> Weight] =
				&[for_tuples!( #( Tuple::on_idle ),* )];

			let mut weight = Weight::zero();
			let len = on_idle_functions.len();

			if len == 0 {
				return Weight::zero()
			}

			let start_index = n % (len as u32).into();
			let start_index = start_index.try_into().ok().expect(
				"`start_index % len` always fits into `usize`, because `len` can be in maximum `usize::MAX`; qed"
			);
			for on_idle_fn in on_idle_functions.iter().cycle().skip(start_index).take(len) {
				let adjusted_remaining_weight = remaining_weight.saturating_sub(weight);
				weight = weight.saturating_add(on_idle_fn(n, adjusted_remaining_weight));
			}
			weight
		}
	}
}

impl_for_tuples_attr! {
	/// A trait that will be called at genesis.
	///
	/// Implementing this trait for a pallet let's you express operations that should
	/// happen at genesis. It will be called in an externalities provided environment and
	/// will set the genesis state after all pallets have written their genesis state.
	pub trait OnGenesis {
		/// Something that should happen at genesis.
		fn on_genesis() {}
	}
}

/// Implemented by pallets, allows defining logic to run prior to any [`OnRuntimeUpgrade`] logic.
///
/// This hook is intended to be used internally in FRAME and not be exposed to FRAME developers.
///
/// It is defined as a separate trait from [`OnRuntimeUpgrade`] precisely to not pollute the public
/// API.
pub trait BeforeAllRuntimeMigrations {
	/// Something that should happen before runtime migrations are executed.
	fn before_all_runtime_migrations() -> Weight {
		Weight::zero()
	}
}

/// See [`Hooks::on_runtime_upgrade`].
pub trait OnRuntimeUpgrade {
	/// See [`Hooks::on_runtime_upgrade`].
	fn on_runtime_upgrade() -> Weight {
		Weight::zero()
	}

	/// The expected and default behavior of this method is to handle executing `pre_upgrade` ->
	/// `on_runtime_upgrade` -> `post_upgrade` hooks for a migration.
	///
	/// Internally, the default implementation
	/// - Handles passing data from `pre_upgrade` to `post_upgrade`
	/// - Ensure storage is not modified in `pre_upgrade` and `post_upgrade` hooks.
	///
	/// Combining the `pre_upgrade` -> `on_runtime_upgrade` -> `post_upgrade` logic flow into a
	/// single method call is helpful for scenarios like testing a tuple of migrations, where the
	/// tuple contains order-dependent migrations.
	#[cfg(feature = "try-runtime")]
	fn try_on_runtime_upgrade(checks: bool) -> Result<Weight, TryRuntimeError> {
		let maybe_state = if checks {
			let _guard = frame_support::StorageNoopGuard::default();
			let state = Self::pre_upgrade()?;
			Some(state)
		} else {
			None
		};

		let weight = Self::on_runtime_upgrade();

		if let Some(state) = maybe_state {
			let _guard = frame_support::StorageNoopGuard::default();
			// we want to panic if any checks fail right here right now.
			Self::post_upgrade(state)?
		}

		Ok(weight)
	}

	/// See [`Hooks::pre_upgrade`].
	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
		Ok(Vec::new())
	}

	/// See [`Hooks::post_upgrade`].
	#[cfg(feature = "try-runtime")]
	fn post_upgrade(_state: Vec<u8>) -> Result<(), TryRuntimeError> {
		Ok(())
	}
}

/// This trait is intended for use within `VersionedMigration` to execute storage migrations without
/// automatic version checks. Implementations should ensure migration logic is safe and idempotent.
pub trait UncheckedOnRuntimeUpgrade {
	/// Called within `VersionedMigration` to execute the actual migration. It is also
	/// expected that no version checks are performed within this function.
	///
	/// See also [`Hooks::on_runtime_upgrade`].
	fn on_runtime_upgrade() -> Weight {
		Weight::zero()
	}

	/// See [`Hooks::pre_upgrade`].
	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
		Ok(Vec::new())
	}

	/// See [`Hooks::post_upgrade`].
	#[cfg(feature = "try-runtime")]
	fn post_upgrade(_state: Vec<u8>) -> Result<(), TryRuntimeError> {
		Ok(())
	}
}

impl_for_tuples_attr! {
	impl BeforeAllRuntimeMigrations for Tuple {
		/// Implements the default behavior of
		/// [`BeforeAllRuntimeMigrations::before_all_runtime_migrations`] for tuples.
		fn before_all_runtime_migrations() -> Weight {
			let mut weight = Weight::zero();
			for_tuples!( #( weight = weight.saturating_add(Tuple::before_all_runtime_migrations()); )* );
			weight
		}
	}
}

impl_for_tuples_attr! {
	impl OnRuntimeUpgrade for Tuple {
		/// Implements the default behavior of [`OnRuntimeUpgrade::on_runtime_upgrade`] for tuples.
		fn on_runtime_upgrade() -> Weight {
			let mut weight = Weight::zero();
			for_tuples!( #( weight = weight.saturating_add(Tuple::on_runtime_upgrade()); )* );
			weight
		}

		/// Implements the default behavior of `try_on_runtime_upgrade` for tuples, logging any errors
		/// that occur.
		#[cfg(feature = "try-runtime")]
		fn try_on_runtime_upgrade(checks: bool) -> Result<Weight, TryRuntimeError> {
			let mut cumulative_weight = Weight::zero();

			let mut errors = Vec::new();

			for_tuples!(#(
				match Tuple::try_on_runtime_upgrade(checks) {
					Ok(weight) => { cumulative_weight.saturating_accrue(weight); },
					Err(err) => { errors.push(err); },
				}
			)*);

			if errors.len() == 1 {
				return Err(errors[0])
			} else if !errors.is_empty() {
				log::error!(
					target: "try-runtime",
					"Detected multiple errors while executing `try_on_runtime_upgrade`:",
				);

				errors.iter().for_each(|err| {
					log::error!(
						target: "try-runtime",
						"{:?}",
						err
					);
				});

				return Err("Detected multiple errors while executing `try_on_runtime_upgrade`, check the logs!".into())
			}

			Ok(cumulative_weight)
		}

		/// [`OnRuntimeUpgrade::pre_upgrade`] should not be used on a tuple.
		///
		/// Instead, implementors should use [`OnRuntimeUpgrade::try_on_runtime_upgrade`] which
		/// internally calls `pre_upgrade` -> `on_runtime_upgrade` -> `post_upgrade` for each tuple
		/// member in sequence, enabling testing of order-dependent migrations.
		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
			Err("Usage of `pre_upgrade` with Tuples is not expected. Please use `try_on_runtime_upgrade` instead, which internally calls `pre_upgrade` -> `on_runtime_upgrade` -> `post_upgrade` for each tuple member.".into())
		}

		/// [`OnRuntimeUpgrade::post_upgrade`] should not be used on a tuple.
		///
		/// Instead, implementors should use [`OnRuntimeUpgrade::try_on_runtime_upgrade`] which
		/// internally calls `pre_upgrade` -> `on_runtime_upgrade` -> `post_upgrade` for each tuple
		/// member in sequence, enabling testing of order-dependent migrations.
		#[cfg(feature = "try-runtime")]
		fn post_upgrade(_state: Vec<u8>) -> Result<(), TryRuntimeError> {
			Err("Usage of `post_upgrade` with Tuples is not expected. Please use `try_on_runtime_upgrade` instead, which internally calls `pre_upgrade` -> `on_runtime_upgrade` -> `post_upgrade` for each tuple member.".into())
		}
	}
}

impl_for_tuples_attr! {
	/// See [`Hooks::integrity_test`].
	pub trait IntegrityTest {
		/// See [`Hooks::integrity_test`].
		fn integrity_test() {}
	}
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// The pallet hooks trait. This is merely an umbrella trait for:
///
/// - [`OnInitialize`]
/// - [`OnFinalize`]
/// - [`OnRuntimeUpgrade`]
/// - [`crate::traits::misc::OffchainWorker`]
/// - [`OnIdle`]
/// - [`IntegrityTest`]
/// - [`OnPoll`]
///
/// ## Ordering
///
/// For all hooks, except [`OnIdle`] the order of execution is derived from how the pallets are
/// ordered in [`crate::construct_runtime`].
///
/// ## Summary
///
/// In short, the following diagram shows the flow of hooks in a pallet
///
/// ```mermaid
/// graph LR
/// 	Optional --> Mandatory
/// 	Mandatory --> ExtrinsicsMandatory
/// 	ExtrinsicsMandatory --> Poll
/// 	Poll --> Extrinsics
/// 	Extrinsics --> AfterMandatory
/// 	AfterMandatory --> onIdle
///
/// subgraph Optional
/// 	OnRuntimeUpgrade
/// end
///
/// subgraph Mandatory
/// 	OnInitialize
/// end
///
/// subgraph ExtrinsicsMandatory
/// 	Inherent1 --> Inherent2
/// end
///
/// subgraph Extrinsics
/// 	direction TB
/// 	Extrinsic1 --> Extrinsic2
/// end
///
/// subgraph AfterMandatory
/// 	OnFinalize
/// end
/// ```
///
/// * [`OnRuntimeUpgrade`](Hooks::OnRuntimeUpgrade) hooks are only executed when a code change is
///   detected.
/// * [`OnRuntimeUpgrade`](Hooks::OnRuntimeUpgrade) hooks are mandatorily executed at the very
///   beginning of the block body, before any extrinsics are processed.
/// * [`Inherents`](sp_inherents) are always executed before any other other signed or unsigned
///   extrinsics.
/// * [`OnIdle`](Hooks::OnIdle) hooks are executed after extrinsics if there is weight remaining in
///   the block.
/// * [`OnFinalize`](Hooks::OnFinalize) hooks are mandatorily executed after
///   [`OnIdle`](Hooks::OnIdle).
///
/// > [`OffchainWorker`](crate::traits::misc::OffchainWorker) hooks are not part of this flow,
/// > because they are not part of the consensus/main block building logic. See
/// > [`OffchainWorker`](crate::traits::misc::OffchainWorker) for more information.
///
/// To learn more about the execution of hooks see the FRAME `Executive` pallet which is in charge
/// of dispatching extrinsics and calling hooks in the correct order.
pub trait Hooks<BlockNumber> {
	/// Block initialization hook. This is called at the very beginning of block execution.
	///
	/// Must return the non-negotiable weight of both itself and whatever [`Hooks::on_finalize`]
	/// wishes to consume.
	///
	/// ## Warning
	///
	/// The weight returned by this is treated as `DispatchClass::Mandatory`, meaning that
	/// it MUST BE EXECUTED. If this is not the case, consider using [`Hooks::on_idle`] instead.
	///
	/// Try to keep any arbitrary execution __deterministic__ and within __minimal__ time
	/// complexity. For example, do not execute any unbounded iterations.
	///
	/// NOTE: This function is called BEFORE ANY extrinsic in a block is applied, including inherent
	/// extrinsics. Hence for instance, if you runtime includes `pallet-timestamp`, the `timestamp`
	/// is not yet up to date at this point.
	fn on_initialize(_n: BlockNumber) -> Weight {
		Weight::zero()
	}

	/// Block finalization hook. This is called at the very end of block execution.
	///
	/// Note that this has nothing to do with finality in the "consensus" sense.
	///
	/// Note that the non-negotiable weight for this has must have already been returned by
	/// [`Hooks::on_initialize`]. It usage alone is not permitted.
	///
	/// Similar to [`Hooks::on_initialize`] it should only be used when execution is absolutely
	/// necessary. In other cases, consider using [`Hooks::on_idle`] instead.
	fn on_finalize(_n: BlockNumber) {}

	/// Hook to consume a block's idle time. This will run when the block is being finalized (before
	/// [`Hooks::on_finalize`]).
	///
	/// Given that all dispatchables are already executed and noted (and the weight for
	/// [`Hooks::on_finalize`], which comes next, is also already accounted for via
	/// `on_initialize`), this hook consumes anything that is leftover.
	///
	/// Each pallet's `on_idle` is chosen to be the first to execute in a round-robin fashion
	/// indexed by the block number.
	///
	/// Return the weight used, the caller will use this to calculate the remaining weight and then
	/// call the next pallet `on_idle` hook if there is still weight left.
	///
	/// Any implementation should always respect `_remaining_weight` and never consume (and
	/// therefore return) more than this amount.
	fn on_idle(_n: BlockNumber, _remaining_weight: Weight) -> Weight {
		Weight::zero()
	}

	/// A hook to run logic after inherent application.
	///
	/// Is not guaranteed to execute in a block and should therefore only be used in no-deadline
	/// scenarios.
	///
	/// This is the non-mandatory version of [`Hooks::on_initialize`].
	fn on_poll(_n: BlockNumber, _weight: &mut WeightMeter) {}

	/// Hook executed when a code change (aka. a "runtime upgrade") is detected by the FRAME
	/// `Executive` pallet.
	///
	/// Be aware that this is called before [`Hooks::on_initialize`] of any pallet; therefore, a lot
	/// of the critical storage items such as `block_number` in system pallet might have not been
	/// set yet.
	///
	/// Similar to [`Hooks::on_initialize`], any code in this block is mandatory and MUST execute.
	/// It is strongly recommended to dry-run the execution of these hooks using
	/// [try-runtime-cli](https://github.com/paritytech/try-runtime-cli) to ensure they will not
	/// produce and overweight block which can brick your chain. Use with care!
	///
	/// ## Implementation Note: Standalone Migrations
	///
	/// Additional migrations can be created by directly implementing [`OnRuntimeUpgrade`] on
	/// structs and passing them to `Executive`. Or alternatively, by implementing
	/// [`UncheckedOnRuntimeUpgrade`], passing it to [`crate::migrations::VersionedMigration`],
	/// which already implements [`OnRuntimeUpgrade`].
	///
	/// ## Implementation Note: Pallet Versioning
	///
	/// Implementations of this hook are typically wrapped in
	/// [`crate::migrations::VersionedMigration`] to ensure the migration is executed exactly
	/// once and only when it is supposed to.
	///
	/// Alternatively, developers can manually implement version checks.
	///
	/// Failure to adequately check storage versions can result in accidental repetitive execution
	/// of the hook, which can be catastrophic.
	///
	/// ## Implementation Note: Weight
	///
	/// Typically, implementations of this method are simple enough that weights can be calculated
	/// manually. However, if required, a benchmark can also be used.
	fn on_runtime_upgrade() -> Weight {
		Weight::zero()
	}

	/// Execute the sanity checks of this pallet, per block.
	///
	/// It should focus on certain checks to ensure that the state is sensible. This is never
	/// executed in a consensus code-path, therefore it can consume as much weight as it needs.
	///
	/// This hook must not alter any storage.
	#[cfg(feature = "try-runtime")]
	fn try_state(_n: BlockNumber) -> Result<(), TryRuntimeError> {
		Ok(())
	}

	/// Execute some pre-checks prior to a runtime upgrade.
	///
	/// Return a `Vec<u8>` that can contain arbitrary encoded data (usually some pre-upgrade state),
	/// which will be passed to `post_upgrade` after upgrading for post-check. An empty vector
	/// should be returned if there is no such need.
	///
	/// This hook is never executed on-chain but instead used by testing tools.
	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
		Ok(Vec::new())
	}

	/// Execute some post-checks after a runtime upgrade.
	///
	/// The `state` parameter is the `Vec<u8>` returned by `pre_upgrade` before upgrading, which
	/// can be used for post-check. NOTE: if `pre_upgrade` is not implemented an empty vector will
	/// be passed in, in such case `post_upgrade` should ignore it.
	///
	/// This hook is never meant to be executed on-chain but is meant to be used by testing tools.
	#[cfg(feature = "try-runtime")]
	fn post_upgrade(_state: Vec<u8>) -> Result<(), TryRuntimeError> {
		Ok(())
	}

	/// Implementing this function on a pallet allows you to perform long-running tasks that are
	/// dispatched as separate threads, and entirely independent of the main blockchain execution.
	///
	/// This function can freely read from the state, but any change it makes to the state is
	/// meaningless. Writes can be pushed back to the chain by submitting extrinsics from the
	/// offchain worker to the transaction pool. See `pallet-example-offchain-worker` for more
	/// details on this.
	///
	/// Moreover, the code in this function has access to a wider range of host functions in
	/// [`sp_io`], namely [`sp_io::offchain`]. This includes exotic operations such as HTTP calls
	/// that are not really possible in the rest of the runtime code.
	///
	/// The execution of this hook is entirely optional and is left at the discretion of the
	/// node-side software and its configuration. In a normal substrate-cli, look for the CLI
	/// flags related to offchain-workers to learn more.
	fn offchain_worker(_n: BlockNumber) {}

	/// Check the integrity of this pallet's configuration.
	///
	/// Any code located in this hook is placed in an auto-generated test, and generated as a part
	/// of [`crate::construct_runtime`]'s expansion. Look for a test case with a name along the
	/// lines of: `__construct_runtime_integrity_test`.
	///
	/// This hook is the location where the values/types provided to the `Config` trait
	/// of the pallet can be tested for correctness. For example, if two `type Foo: Get<u32>` and
	/// `type Bar: Get<u32>` where `Foo::get()` must always be greater than `Bar::get()`, such
	/// checks can be asserted upon here.
	///
	/// Note that this hook is executed in an externality environment, provided by
	/// `sp_io::TestExternalities`. This makes it possible to access the storage.
	fn integrity_test() {}
}

/// A trait to define the build function of a genesis config for both runtime and pallets.
///
/// A trait to define the build function of a genesis config.
pub trait BuildGenesisConfig: sp_runtime::traits::MaybeSerializeDeserialize {
	/// The build function puts initial `GenesisConfig` keys/values pairs into the storage.
	fn build(&self);
}

impl BuildGenesisConfig for () {
	fn build(&self) {}
}

impl_for_tuples_attr! {
	/// A trait which is called when the timestamp is set in the runtime.
	pub trait OnTimestampSet<Moment> {
		/// Called when the timestamp is set.
		fn on_timestamp_set(moment: Moment);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::parameter_types;
	use alloc::vec::Vec;
	use sp_io::TestExternalities;

	#[cfg(feature = "try-runtime")]
	#[test]
	fn on_runtime_upgrade_pre_post_executed_tuple() {
		crate::parameter_types! {
			pub static Pre: Vec<&'static str> = Default::default();
			pub static Post: Vec<&'static str> = Default::default();
		}

		macro_rules! impl_test_type {
			($name:ident) => {
				struct $name;
				impl OnRuntimeUpgrade for $name {
					fn on_runtime_upgrade() -> Weight {
						Default::default()
					}

					#[cfg(feature = "try-runtime")]
					fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
						Pre::mutate(|s| s.push(stringify!($name)));
						Ok(Vec::new())
					}

					#[cfg(feature = "try-runtime")]
					fn post_upgrade(_: Vec<u8>) -> Result<(), TryRuntimeError> {
						Post::mutate(|s| s.push(stringify!($name)));
						Ok(())
					}
				}
			};
		}

		impl_test_type!(Foo);
		impl_test_type!(Bar);
		impl_test_type!(Baz);

		TestExternalities::default().execute_with(|| {
			// try_on_runtime_upgrade works
			Foo::try_on_runtime_upgrade(true).unwrap();
			assert_eq!(Pre::take(), vec!["Foo"]);
			assert_eq!(Post::take(), vec!["Foo"]);

			<(Foo, Bar, Baz)>::try_on_runtime_upgrade(true).unwrap();
			assert_eq!(Pre::take(), vec!["Foo", "Bar", "Baz"]);
			assert_eq!(Post::take(), vec!["Foo", "Bar", "Baz"]);

			<((Foo, Bar), Baz)>::try_on_runtime_upgrade(true).unwrap();
			assert_eq!(Pre::take(), vec!["Foo", "Bar", "Baz"]);
			assert_eq!(Post::take(), vec!["Foo", "Bar", "Baz"]);

			<(Foo, (Bar, Baz))>::try_on_runtime_upgrade(true).unwrap();
			assert_eq!(Pre::take(), vec!["Foo", "Bar", "Baz"]);
			assert_eq!(Post::take(), vec!["Foo", "Bar", "Baz"]);

			// calling pre_upgrade and post_upgrade directly on tuple of pallets fails
			assert!(<(Foo, (Bar, Baz))>::pre_upgrade().is_err());
			assert!(<(Foo, (Bar, Baz))>::post_upgrade(vec![]).is_err());
		});
	}

	#[test]
	fn on_initialize_and_on_runtime_upgrade_weight_merge_works() {
		struct Test;

		impl OnInitialize<u8> for Test {
			fn on_initialize(_n: u8) -> Weight {
				Weight::from_parts(10, 0)
			}
		}
		impl OnRuntimeUpgrade for Test {
			fn on_runtime_upgrade() -> Weight {
				Weight::from_parts(20, 0)
			}
		}

		TestExternalities::default().execute_with(|| {
			assert_eq!(<(Test, Test)>::on_initialize(0), Weight::from_parts(20, 0));
			assert_eq!(<(Test, Test)>::on_runtime_upgrade(), Weight::from_parts(40, 0));
		});
	}

	#[test]
	fn on_idle_round_robin_works() {
		parameter_types! {
			static OnIdleInvocationOrder: Vec<&'static str> = Vec::new();
		}

		struct Test1;
		struct Test2;
		struct Test3;
		type TestTuple = (Test1, Test2, Test3);
		impl OnIdle<u32> for Test1 {
			fn on_idle(_n: u32, _weight: Weight) -> Weight {
				OnIdleInvocationOrder::mutate(|o| o.push("Test1"));
				Weight::zero()
			}
		}
		impl OnIdle<u32> for Test2 {
			fn on_idle(_n: u32, _weight: Weight) -> Weight {
				OnIdleInvocationOrder::mutate(|o| o.push("Test2"));
				Weight::zero()
			}
		}
		impl OnIdle<u32> for Test3 {
			fn on_idle(_n: u32, _weight: Weight) -> Weight {
				OnIdleInvocationOrder::mutate(|o| o.push("Test3"));
				Weight::zero()
			}
		}

		TestTuple::on_idle(0, Weight::zero());
		assert_eq!(OnIdleInvocationOrder::get(), ["Test1", "Test2", "Test3"].to_vec());
		OnIdleInvocationOrder::mutate(|o| o.clear());

		TestTuple::on_idle(1, Weight::zero());
		assert_eq!(OnIdleInvocationOrder::get(), ["Test2", "Test3", "Test1"].to_vec());
		OnIdleInvocationOrder::mutate(|o| o.clear());

		TestTuple::on_idle(2, Weight::zero());
		assert_eq!(OnIdleInvocationOrder::get(), ["Test3", "Test1", "Test2"].to_vec());
		OnIdleInvocationOrder::mutate(|o| o.clear());

		TestTuple::on_idle(3, Weight::zero());
		assert_eq!(OnIdleInvocationOrder::get(), ["Test1", "Test2", "Test3"].to_vec());
		OnIdleInvocationOrder::mutate(|o| o.clear());

		TestTuple::on_idle(4, Weight::zero());
		assert_eq!(OnIdleInvocationOrder::get(), ["Test2", "Test3", "Test1"].to_vec());
		OnIdleInvocationOrder::mutate(|o| o.clear());
	}
}
