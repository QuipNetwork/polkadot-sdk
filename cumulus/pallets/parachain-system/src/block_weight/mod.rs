// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Provides functionality to dynamically calculate the block weight for a parachain.
//!
//! With block bundling, parachains are relatively free to choose whatever block interval they want.
//! The block interval is the time between individual blocks. The available resources per block (max
//! block weight) depend on the number of cores allocated to the parachain on the relay chain. Each
//! relay chain cores provides an execution time of `2s` and a storage size of `10MiB`. Depending on
//! the desired number of blocks to produce, the resources need to be divided between the individual
//! blocks. With small blocks that do not have that many resources available, a problem may arises
//! for bigger transactions not fitting into blocks anymore, e.g. a runtime upgrade. For these cases
//! the weight of a block can be increased to use the weight of a full core. Only the first block of
//! a core is allowed to increase its weight to use the full core weight. In the case of the first
//! block using the full core weight, there will be no further block build on the same core. This is
//! signaled to the node by setting the [`CumulusDigestItem::UseFullCore`] digest item.`
//!
//! The [`MaxParachainBlockWeight`] provides a [`Get`] implementation that will return the max block
//! weight as determined by the [`DynamicMaxBlockWeight`] transaction extension.
//!
//! [`DynamicMaxBlockWeightHooks`] needs to be registered as a pre-inherent hook. It is used to
//! handle the weight consumption of `on_initialize` and change the block weight mode based on the
//! consumed weight.
//!
//! # Setup
//!
//! Setup the transaction extension:
#![doc = docify::embed!("src/block_weight/mock.rs", tx_extension_setup)]
//! Setting up `MaximumBlockWeight`:
#![doc = docify::embed!("src/block_weight/mock.rs", max_block_weight_setup)]
//! Registering of the `PreInherents` hook:
#![doc = docify::embed!("src/block_weight/mock.rs", pre_inherents_setup)]
//! # Weight per context
//!
//! Depending on the context, [`MaxParachainBlockWeight`] may return a different max weight. The
//! max weight is only allowed to change in the first block of a core. Otherwise, all blocks need to
//! follow the target block weight determined based on the number of cores and the target block
//! rate. In the case of a first block, the following contexts may allow to access the full core
//! weight:
//!
//! - `on_initialize`: All logic that runs in this context up to the execution of `inherents` will
//!   get access to the full core weight.
//! - `inherents`: Inherents also have access to the full core weight.
//! - `on_poll`: Only gets access to the target block weight.
//! - `transactions`: May get access to the full core weight, depends if they enable the access to
//!   the full core weight based on the logic of [`DynamicMaxBlockWeight`].
//! - `on_finalize`/`on_idle`: Only gets access to the target block weight.
//!
//! If any context that allows to use the full core weight, pushes the used block weight above the
//! target block weight, all other contexts will get access to the full core weight.

use crate::{Config, PreviousCoreCount};
use codec::{Decode, Encode};
use core::marker::PhantomData;
use cumulus_primitives_core::CumulusDigestItem;
use frame_support::{
	weights::{constants::WEIGHT_REF_TIME_PER_SECOND, Weight},
	CloneNoBound, DebugNoBound,
};
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_primitives::{executor_params::DEFAULT_BACKING_EXECUTION_TIMEOUT, MAX_POV_SIZE};
use scale_info::TypeInfo;
use sp_core::Get;
use sp_runtime::Digest;

#[cfg(test)]
mod mock;
pub mod pre_inherents_hook;
#[cfg(test)]
mod tests;
pub mod transaction_extension;

pub use pre_inherents_hook::DynamicMaxBlockWeightHooks;
pub use transaction_extension::DynamicMaxBlockWeight;

const LOG_TARGET: &str = "runtime::parachain-system::block-weight";

/// Maximum ref time per core
const MAX_REF_TIME_PER_CORE_NS: u64 =
	DEFAULT_BACKING_EXECUTION_TIMEOUT.as_secs() * WEIGHT_REF_TIME_PER_SECOND;

/// The available weight per core on the relay chain.
pub(crate) const FULL_CORE_WEIGHT: Weight =
	Weight::from_parts(MAX_REF_TIME_PER_CORE_NS, MAX_POV_SIZE as u64);

// Is set to `true` when we are currently inside of `pre_validate_extrinsic`.
//
// Forces `MaxParachainBlockWeight::get()` to return fractional weight, enabling detection of
// transactions that exceed the fractional target limit.
environmental::environmental!(inside_pre_validate: bool);

/// The current block weight mode.
///
/// Based on this mode [`MaxParachainBlockWeight`] determines the current allowed block weight.
#[derive(DebugNoBound, Encode, Decode, CloneNoBound, TypeInfo, PartialEq)]
#[scale_info(skip_type_params(T))]
pub enum BlockWeightMode<T: Config> {
	/// The block is allowed to use the weight of a full core.
	FullCore {
		/// The block in which this mode was set. Is used to determine if this is maybe stale mode
		/// setting, e.g. when running `validate_block`.
		context: BlockNumberFor<T>,
	},
	/// The current active transaction is allowed to use the weight of a full core.
	PotentialFullCore {
		/// The block in which this mode was set. Is used to determine if this is maybe stale mode
		/// setting, e.g. when running `validate_block`.
		context: BlockNumberFor<T>,
		/// The index of the first transaction.
		///
		/// Stays `None` for all inherents until there is the first transaction.
		first_transaction_index: Option<u32>,
		/// The target weight that was used to determine that the extrinsic is above this limit.
		target_weight: Weight,
	},
	/// The block is only allowed to consume its fraction of the core.
	///
	/// How much each block is allowed to consume, depends on the target number of blocks and the
	/// available cores on the relay chain.
	FractionOfCore {
		/// The block in which this mode was set. Is used to determine if this is maybe stale mode
		/// setting, e.g. when running `validate_block`.
		context: BlockNumberFor<T>,
		/// The index of the first transaction.
		///
		/// Stays `None` for all inherents until there is the first transaction.
		first_transaction_index: Option<u32>,
	},
}

impl<T: Config> BlockWeightMode<T> {
	/// Check if this mode is stale, aka was set in a previous block.
	fn is_stale(&self) -> bool {
		let context = self.context();

		context < frame_system::Pallet::<T>::block_number()
	}

	/// Returns the context (block) in which this mode was set.
	fn context(&self) -> BlockNumberFor<T> {
		match self {
			Self::FullCore { context } |
			Self::PotentialFullCore { context, .. } |
			Self::FractionOfCore { context, .. } => *context,
		}
	}

	/// Create a new instance of `Self::FullCore`.
	pub(crate) fn full_core() -> Self {
		Self::FullCore { context: frame_system::Pallet::<T>::block_number() }
	}

	/// Create new instance of `Self::FractionOfCore`.
	pub(crate) fn fraction_of_core(first_transaction_index: Option<u32>) -> Self {
		Self::FractionOfCore {
			context: frame_system::Pallet::<T>::block_number(),
			first_transaction_index,
		}
	}

	/// Create new instance of `Self::PotentialFullCore`.
	pub(crate) fn potential_full_core(
		first_transaction_index: Option<u32>,
		target_weight: Weight,
	) -> Self {
		Self::PotentialFullCore {
			context: frame_system::Pallet::<T>::block_number(),
			first_transaction_index,
			target_weight,
		}
	}
}

/// Calculates the maximum block weight for a parachain.
///
/// Based on the available cores and the number of desired blocks a block weight is calculated.
///
/// The max block weight is partly dynamic and controlled via the [`DynamicMaxBlockWeight`]
/// transaction extension. The transaction extension is communicating the desired max block weight
/// using the [`BlockWeightMode`].
pub struct MaxParachainBlockWeight<Config, TargetBlockRate>(PhantomData<(Config, TargetBlockRate)>);

impl<Config: crate::Config, TargetBlockRate: Get<u32>>
	MaxParachainBlockWeight<Config, TargetBlockRate>
{
	/// Returns the target block weight for one block.
	pub(crate) fn target_block_weight() -> Weight {
		let digest = frame_system::Pallet::<Config>::digest();
		Self::target_block_weight_with_digest(&digest)
	}

	/// Same as [`Self::target_block_weight`], but takes the `digests` directly.
	fn target_block_weight_with_digest(digest: &Digest) -> Weight {
		let number_of_cores = CumulusDigestItem::find_core_info(&digest).map_or_else(
			|| PreviousCoreCount::<Config>::get().map_or(1, |pc| pc.0),
			|ci| ci.number_of_cores.0,
		) as u64;

		let target_blocks = TargetBlockRate::get() as u64;

		// Ensure we have at least one core and valid target blocks
		if number_of_cores == 0 || target_blocks == 0 {
			return FULL_CORE_WEIGHT;
		}

		let blocks_per_core = target_blocks.div_ceil(number_of_cores);

		// At maximum we want to allow `6s` of ref time, because we don't want to overload nodes
		// that are running with standard hardware. These nodes need to be able to import all the
		// blocks in `6s`.
		let ref_time_per_block = core::cmp::min(
			MAX_REF_TIME_PER_CORE_NS / blocks_per_core, // Core allocation limit
			(6 * WEIGHT_REF_TIME_PER_SECOND) / target_blocks, // Full node import limit
		);

		// PoV size we can use as much as we can get from the cores, but at maximum it is one block
		// per core. Or in other words, one block can not span across multiple cores.
		let proof_size_per_block = MAX_POV_SIZE as u64 / blocks_per_core;

		Weight::from_parts(ref_time_per_block, proof_size_per_block)
	}
}

impl<Config: crate::Config, TargetBlockRate: Get<u32>> Get<Weight>
	for MaxParachainBlockWeight<Config, TargetBlockRate>
{
	fn get() -> Weight {
		let digest = frame_system::Pallet::<Config>::digest();
		let target_block_weight = Self::target_block_weight_with_digest(&digest);

		let maybe_full_core_weight = if is_first_block_in_core_with_digest(&digest).unwrap_or(false)
		{
			FULL_CORE_WEIGHT
		} else {
			target_block_weight
		};

		// Check if we are inside `pre_validate_extrinsic` of the transaction extension.
		//
		// When `pre_validate_extrinsic` calls this code, it is interested to know the
		// fractional `target_block_weight` which is then used to calculate the weight for each
		// dispatch class. Fractional weight is returned to detect transactions exceeding the
		// fractional target, enabling proper transition to `PotentialFullCore` mode.
		//
		// If `FullCore` mode is already enabled, the fractional target weight is not important
		// anymore.
		let in_pre_validate = inside_pre_validate::with(|v| *v).unwrap_or(false);

		match crate::BlockWeightMode::<Config>::get().filter(|m| !m.is_stale()) {
			// We allow the full core.
			Some(
				BlockWeightMode::<Config>::FullCore { .. } |
				BlockWeightMode::<Config>::PotentialFullCore { .. },
			) => FULL_CORE_WEIGHT,
			// We are in `pre_validate`.
			_ if in_pre_validate => target_block_weight,
			// Only use the fraction of a core.
			Some(BlockWeightMode::<Config>::FractionOfCore { first_transaction_index, .. }) => {
				let is_phase_finalization = frame_system::Pallet::<Config>::execution_phase()
					.map_or(false, |p| matches!(p, frame_system::Phase::Finalization));
				let inherents_applied = frame_system::Pallet::<Config>::inherents_applied();

				if first_transaction_index.is_none() && !is_phase_finalization && !inherents_applied
				{
					// We are running in the context of inherents, here we allow the
					// full core weight.
					maybe_full_core_weight
				} else {
					// If we are finalizing the block (e.g. `on_idle` is running and
					// `finalize_block`), running `on_poll` or nothing required more than the target
					// block weight, we only allow the target block weight.
					target_block_weight
				}
			},
			// We are in `on_initialize` or in an offchain context.
			None => maybe_full_core_weight,
		}
	}
}

/// Is this the first block in a core?
fn is_first_block_in_core<T: Config>() -> Option<bool> {
	let digest = frame_system::Pallet::<T>::digest();
	is_first_block_in_core_with_digest(&digest)
}

/// Is this the first block in a core? (takes digest as parameter)
///
/// Returns `None` if the [`CumulusDigestItem::BlockBundleInfo`] digest is not set.
fn is_first_block_in_core_with_digest(digest: &Digest) -> Option<bool> {
	CumulusDigestItem::find_block_bundle_info(digest).map(|bi| bi.index == 0)
}

/// Is the `BlockWeight` already above the target block weight?
///
/// Returns `None` if the [`CumulusDigestItem::BlockBundleInfo`] digest is not set.
fn block_weight_over_target_block_weight<T: Config, TargetBlockRate: Get<u32>>() -> bool {
	let target_block_weight = MaxParachainBlockWeight::<T, TargetBlockRate>::target_block_weight();

	frame_system::Pallet::<T>::remaining_block_weight()
		.consumed()
		.any_gt(target_block_weight)
}
