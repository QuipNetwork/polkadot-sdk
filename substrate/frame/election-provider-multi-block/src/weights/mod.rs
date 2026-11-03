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

//! WeightInfo for the election provider multi-block pallet group.

mod pallet_election_provider_multi_block_dot_size;
mod pallet_election_provider_multi_block_signed_dot_size;
mod pallet_election_provider_multi_block_unsigned_dot_size;
mod pallet_election_provider_multi_block_verifier_dot_size;

mod pallet_election_provider_multi_block_ksm_size;
mod pallet_election_provider_multi_block_signed_ksm_size;
mod pallet_election_provider_multi_block_unsigned_ksm_size;
mod pallet_election_provider_multi_block_verifier_ksm_size;

use frame_support::pallet_prelude::Weight;

pub mod traits {
	use super::*;
	pub mod pallet_election_provider_multi_block_signed {
		use super::*;

		/// Weight functions needed for `pallet_election_provider_multi_block_signed`.
		pub trait WeightInfo {
			fn register_not_full() -> Weight;
			fn register_eject() -> Weight;
			fn submit_page() -> Weight;
			fn unset_page() -> Weight;
			fn bail() -> Weight;
			fn clear_old_round_data(p: u32) -> Weight;
			fn claim_unpaid_reward() -> Weight;
		}

		#[cfg(feature = "std")]
		impl WeightInfo for () {
			fn bail() -> Weight {
				Default::default()
			}
			fn clear_old_round_data(_p: u32) -> Weight {
				Default::default()
			}
			fn register_eject() -> Weight {
				Default::default()
			}
			fn register_not_full() -> Weight {
				Default::default()
			}
			fn submit_page() -> Weight {
				Default::default()
			}
			fn unset_page() -> Weight {
				Default::default()
			}
			fn claim_unpaid_reward() -> Weight {
				Default::default()
			}
		}
	}

	pub mod pallet_election_provider_multi_block_unsigned {
		use super::*;

		/// Weight functions needed for `pallet_election_provider_multi_block::unsigned`.
		pub trait WeightInfo {
			fn validate_unsigned() -> Weight;
			fn submit_unsigned() -> Weight;
			// This has an auto-impl as the associated benchmark is `#[extra]`.
			fn mine_solution(_p: u32) -> Weight {
				Default::default()
			}
		}

		#[cfg(feature = "std")]
		impl WeightInfo for () {
			fn validate_unsigned() -> Weight {
				Default::default()
			}
			fn submit_unsigned() -> Weight {
				Default::default()
			}
		}
	}

	pub mod pallet_election_provider_multi_block_verifier {
		use super::*;

		/// Weight functions needed for `pallet_election_provider_multi_block_verifier`.
		pub trait WeightInfo {
			fn verification_valid_non_terminal() -> Weight;
			fn verification_valid_terminal() -> Weight;
			fn verification_invalid_terminal() -> Weight;
			fn verification_invalid_non_terminal(v: u32) -> Weight;
		}

		#[cfg(feature = "std")]
		impl WeightInfo for () {
			fn verification_valid_non_terminal() -> Weight {
				Weight::from_parts(0, 7)
			}
			fn verification_valid_terminal() -> Weight {
				Weight::from_parts(0, 7)
			}
			fn verification_invalid_terminal() -> Weight {
				Weight::from_parts(0, 7)
			}
			fn verification_invalid_non_terminal(_v: u32) -> Weight {
				Weight::from_parts(0, 7)
			}
		}
	}

	pub mod pallet_election_provider_multi_block {
		use super::*;

		/// Weight functions needed for `pallet_election_provider_multi_block`.
		pub trait WeightInfo {
			fn per_block_nothing() -> Weight;
			fn per_block_snapshot_msp() -> Weight;
			fn per_block_snapshot_rest() -> Weight;
			fn per_block_start_signed_validation() -> Weight;
			fn export_non_terminal() -> Weight;
			fn export_terminal() -> Weight;
			fn admin_set() -> Weight;
			fn manage_fallback() -> Weight;
		}

		#[cfg(feature = "std")]
		impl WeightInfo for () {
			fn per_block_nothing() -> Weight {
				Default::default()
			}
			fn per_block_snapshot_msp() -> Weight {
				Weight::from_parts(0, 5)
			}
			fn per_block_snapshot_rest() -> Weight {
				Weight::from_parts(0, 5)
			}
			fn per_block_start_signed_validation() -> Weight {
				Weight::from_parts(0, 3)
			}
			fn export_non_terminal() -> Weight {
				Default::default()
			}
			fn export_terminal() -> Weight {
				Default::default()
			}
			fn admin_set() -> Weight {
				Default::default()
			}
			fn manage_fallback() -> Weight {
				Default::default()
			}
		}
	}
}

/// Kusama-esque weights only be used in testing runtimes.
pub mod kusama {
	pub use super::{
		pallet_election_provider_multi_block_ksm_size::WeightInfo as MultiBlockWeightInfo,
		pallet_election_provider_multi_block_signed_ksm_size::WeightInfo as MultiBlockSignedWeightInfo,
		pallet_election_provider_multi_block_unsigned_ksm_size::WeightInfo as MultiBlockUnsignedWeightInfo,
		pallet_election_provider_multi_block_verifier_ksm_size::WeightInfo as MultiBlockVerifierWeightInfo,
	};
}

/// Polkadot-esque weights only be used in testing runtimes.
pub mod polkadot {
	pub use super::{
		pallet_election_provider_multi_block_dot_size::WeightInfo as MultiBlockWeightInfo,
		pallet_election_provider_multi_block_signed_dot_size::WeightInfo as MultiBlockSignedWeightInfo,
		pallet_election_provider_multi_block_unsigned_dot_size::WeightInfo as MultiBlockUnsignedWeightInfo,
		pallet_election_provider_multi_block_verifier_dot_size::WeightInfo as MultiBlockVerifierWeightInfo,
	};
}
