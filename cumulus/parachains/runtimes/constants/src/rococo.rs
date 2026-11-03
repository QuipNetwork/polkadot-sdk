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

pub mod currency {
	use polkadot_core_primitives::Balance;
	use rococo_runtime_constants as constants;

	/// The existential deposit. Set to 1/10 of its parent Relay Chain (v9010).
	pub const EXISTENTIAL_DEPOSIT: Balance = constants::currency::EXISTENTIAL_DEPOSIT / 10;

	pub const UNITS: Balance = constants::currency::UNITS;
	pub const CENTS: Balance = constants::currency::CENTS;
	pub const MILLICENTS: Balance = constants::currency::MILLICENTS;

	pub const fn deposit(items: u32, bytes: u32) -> Balance {
		// map to 1/100 of what the rococo relay chain charges
		constants::currency::deposit(items, bytes) / 100
	}
}

pub mod fee {
	use frame_support::{
		pallet_prelude::Weight,
		weights::{
			constants::ExtrinsicBaseWeight, FeePolynomial, WeightToFeeCoefficient,
			WeightToFeeCoefficients, WeightToFeePolynomial,
		},
	};
	use polkadot_core_primitives::Balance;
	use smallvec::smallvec;
	pub use sp_runtime::Perbill;

	/// The block saturation level. Fees will be updates based on this value.
	pub const TARGET_BLOCK_FULLNESS: Perbill = Perbill::from_percent(25);

	/// Handles converting a weight scalar to a fee value, based on the scale and granularity of the
	/// node's balance type.
	///
	/// This should typically create a mapping between the following ranges:
	///   - `[0, MAXIMUM_BLOCK_WEIGHT]`
	///   - `[Balance::min, Balance::max]`
	///
	/// Yet, it can be used for any other sort of change to weight-fee. Some examples being:
	///   - Setting it to `0` will essentially disable the weight fee.
	///   - Setting it to `1` will cause the literal `#[weight = x]` values to be charged.
	pub struct WeightToFee;
	impl frame_support::weights::WeightToFee for WeightToFee {
		type Balance = Balance;

		fn weight_to_fee(weight: &Weight) -> Self::Balance {
			let time_poly: FeePolynomial<Balance> = RefTimeToFee::polynomial().into();
			let proof_poly: FeePolynomial<Balance> = ProofSizeToFee::polynomial().into();

			// Take the maximum instead of the sum to charge by the more scarce resource.
			time_poly.eval(weight.ref_time()).max(proof_poly.eval(weight.proof_size()))
		}
	}

	/// Maps the reference time component of `Weight` to a fee.
	pub struct RefTimeToFee;
	impl WeightToFeePolynomial for RefTimeToFee {
		type Balance = Balance;
		fn polynomial() -> WeightToFeeCoefficients<Self::Balance> {
			// In Rococo, extrinsic base weight (smallest non-zero weight) is mapped to 1/10 CENT:
			// The standard system parachain configuration is 1/10 of that, as in 1/100 CENT.
			let p = super::currency::CENTS;
			let q = 100 * Balance::from(ExtrinsicBaseWeight::get().ref_time());

			smallvec![WeightToFeeCoefficient {
				degree: 1,
				negative: false,
				coeff_frac: Perbill::from_rational(p % q, q),
				coeff_integer: p / q,
			}]
		}
	}

	/// Maps the proof size component of `Weight` to a fee.
	pub struct ProofSizeToFee;
	impl WeightToFeePolynomial for ProofSizeToFee {
		type Balance = Balance;
		fn polynomial() -> WeightToFeeCoefficients<Self::Balance> {
			// Map 10kb proof to 1 CENT.
			let p = super::currency::CENTS;
			let q = 10_000;

			smallvec![WeightToFeeCoefficient {
				degree: 1,
				negative: false,
				coeff_frac: Perbill::from_rational(p % q, q),
				coeff_integer: p / q,
			}]
		}
	}
}

/// Consensus-related.
pub mod consensus {
	use frame_support::weights::{constants::WEIGHT_REF_TIME_PER_SECOND, Weight};

	/// Maximum number of blocks simultaneously accepted by the Runtime, not yet included
	/// into the relay chain.
	pub const UNINCLUDED_SEGMENT_CAPACITY: u32 = 36;
	/// How many parachain blocks are processed by the relay chain per parent. Limits the
	/// number of blocks authored per slot.
	pub const BLOCK_PROCESSING_VELOCITY: u32 = 12;
	/// Relay chain slot duration, in milliseconds.
	pub const RELAY_CHAIN_SLOT_DURATION_MILLIS: u32 = 6000;

	/// We allow for 2 seconds of compute with a 6 second average block.
	pub const MAXIMUM_BLOCK_WEIGHT: Weight = Weight::from_parts(
		WEIGHT_REF_TIME_PER_SECOND.saturating_mul(2),
		cumulus_primitives_core::relay_chain::MAX_POV_SIZE as u64,
	);

	/// This determines the average expected block time that we are targeting.
	/// Blocks will be produced at a minimum duration defined by `SLOT_DURATION`.
	/// `SLOT_DURATION` is picked up by `pallet_timestamp` which is in turn picked
	/// up by `pallet_aura` to implement `fn slot_duration()`.
	///
	/// Change this to adjust the block time.
	pub const MILLISECS_PER_BLOCK: u64 = 6000;
	pub const SLOT_DURATION: u64 = MILLISECS_PER_BLOCK;
}

/// Time-related
pub mod time {
	use polkadot_core_primitives::BlockNumber;

	// Time is measured by number of blocks.
	pub const MINUTES: BlockNumber =
		60_000 / (super::consensus::MILLISECS_PER_BLOCK as BlockNumber);
	pub const HOURS: BlockNumber = MINUTES * 60;
	pub const DAYS: BlockNumber = HOURS * 24;
}

pub mod snowbridge {
	use frame_support::parameter_types;
	use xcm::prelude::{Location, NetworkId};

	/// The pallet index of the Ethereum inbound queue pallet in the bridge hub runtime.
	pub const INBOUND_QUEUE_PALLET_INDEX: u8 = 80;

	parameter_types! {
		/// Network and location for the Ethereum chain. On Rococo, the Ethereum chain bridged
		/// to is the Sepolia Ethereum testnet, with chain ID 11155111.
		/// <https://chainlist.org/chain/11155111>
		/// <https://ethereum.org/en/developers/docs/apis/json-rpc/#net_version>
		pub EthereumNetwork: NetworkId = NetworkId::Ethereum { chain_id: 11155111 };
		pub EthereumLocation: Location = Location::new(2, EthereumNetwork::get());
	}
}

pub mod xcm_version {
	/// The default XCM version to set in genesis config.
	pub const SAFE_XCM_VERSION: u32 = xcm::prelude::XCM_VERSION;
}

pub mod locations {
	use frame_support::parameter_types;
	pub use rococo_runtime_constants::system_parachain::{AssetHubParaId, PeopleParaId};
	use xcm::latest::prelude::{Location, Parachain};

	parameter_types! {
		pub AssetHubLocation: Location = Location::new(1, Parachain(rococo_runtime_constants::system_parachain::ASSET_HUB_ID));
		pub PeopleLocation: Location = Location::new(1, Parachain(rococo_runtime_constants::system_parachain::PEOPLE_ID));
	}
}
