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

//! Remote mining tests for Kusama and Polkadot.
//!
//! Run like this:
//!
//! ```ignore
//! RUST_LOG=remote-ext=info,runtime::election-provider=debug cargo test --release --features remote-mining -p pallet-election-provider-multi-phase mine_for_ -- --test-threads 1
//! ```
//!
//! See the comments below on how to feed specific hash.

use crate::{ElectionCompute, Miner, MinerConfig, RawSolution, RoundSnapshot};
use codec::Decode;
use core::marker::PhantomData;
use frame_election_provider_support::generate_solution_type;
use frame_support::{
	traits::Get,
	weights::constants::{WEIGHT_PROOF_SIZE_PER_MB, WEIGHT_REF_TIME_PER_SECOND},
};
use remote_externalities::{Builder, Mode, OnlineConfig};
use sp_core::{ConstU32, H256};
use sp_npos_elections::BalancingConfig;
use sp_runtime::{Perbill, Weight};

pub mod polkadot {
	use super::*;

	pub struct MinerConfig;

	pub struct MaxWeight;
	impl Get<Weight> for MaxWeight {
		fn get() -> Weight {
			Weight::from_parts(
				WEIGHT_REF_TIME_PER_SECOND.saturating_mul(2),
				WEIGHT_PROOF_SIZE_PER_MB.saturating_mul(5),
			)
		}
	}

	generate_solution_type!(
		#[compact]
		pub struct PolkadotSolution::<
			VoterIndex = u32,
			TargetIndex = u16,
			Accuracy = sp_runtime::PerU16,
			MaxVoters = ConstU32<22_500>,
		>(16)
	);

	/// Some configs are a bit inconsistent, but we don't care about them for now.
	impl crate::MinerConfig for MinerConfig {
		type AccountId = sp_runtime::AccountId32;
		type MaxBackersPerWinner = ConstU32<1024>;
		type MaxLength = ConstU32<{ 4 * 1024 * 1024 }>;
		type MaxVotesPerVoter = ConstU32<16>;
		type MaxWeight = MaxWeight;
		type MaxWinners = ConstU32<1000>;
		type Solution = PolkadotSolution;

		fn solution_weight(
			_voters: u32,
			_targets: u32,
			_active_voters: u32,
			_degree: u32,
		) -> Weight {
			Default::default()
		}
	}
}

pub mod kusama {
	use super::*;
	pub struct MinerConfig;

	pub struct MaxWeight;
	impl Get<Weight> for MaxWeight {
		fn get() -> Weight {
			Weight::from_parts(
				WEIGHT_REF_TIME_PER_SECOND.saturating_mul(2),
				WEIGHT_PROOF_SIZE_PER_MB.saturating_mul(5),
			)
		}
	}

	generate_solution_type!(
		#[compact]
		pub struct PolkadotSolution::<
			VoterIndex = u32,
			TargetIndex = u16,
			Accuracy = sp_runtime::PerU16,
			MaxVoters = ConstU32<12_500>,
		>(24)
	);

	/// Some configs are a bit inconsistent, but we don't care about them for now.
	impl crate::MinerConfig for MinerConfig {
		type AccountId = sp_runtime::AccountId32;
		type MaxBackersPerWinner = ConstU32<1024>;
		type MaxLength = ConstU32<{ 4 * 1024 * 1024 }>;
		type MaxVotesPerVoter = ConstU32<24>;
		type MaxWeight = MaxWeight;
		type MaxWinners = ConstU32<1000>;
		type Solution = PolkadotSolution;

		fn solution_weight(
			_voters: u32,
			_targets: u32,
			_active_voters: u32,
			_degree: u32,
		) -> Weight {
			Default::default()
		}
	}
}

pub struct HackyGetSnapshot<T: MinerConfig>(PhantomData<T>);

type UntypedSnapshotOf<T> = RoundSnapshot<
	<T as MinerConfig>::AccountId,
	frame_election_provider_support::Voter<
		<T as MinerConfig>::AccountId,
		<T as MinerConfig>::MaxVotesPerVoter,
	>,
>;

impl<T: MinerConfig> HackyGetSnapshot<T> {
	fn snapshot() -> UntypedSnapshotOf<T>
	where
		UntypedSnapshotOf<T>: Decode,
	{
		let key = [
			sp_crypto_hashing::twox_128(b"ElectionProviderMultiPhase"),
			sp_crypto_hashing::twox_128(b"Snapshot"),
		]
		.concat();
		frame_support::storage::unhashed::get::<UntypedSnapshotOf<T>>(&key).unwrap()
	}

	fn desired_targets() -> u32 {
		let key = [
			sp_crypto_hashing::twox_128(b"ElectionProviderMultiPhase"),
			sp_crypto_hashing::twox_128(b"DesiredTargets"),
		]
		.concat();
		frame_support::storage::unhashed::get::<u32>(&key).unwrap()
	}
}

pub type FakeBlock = sp_runtime::testing::Block<sp_runtime::testing::TestXt<(), ()>>;

pub struct Balancing;
impl Get<Option<BalancingConfig>> for Balancing {
	fn get() -> Option<BalancingConfig> {
		Some(BalancingConfig { iterations: 10, tolerance: 0 })
	}
}
pub type SolverOf<T> = frame_election_provider_support::SequentialPhragmen<
	<T as MinerConfig>::AccountId,
	Perbill,
	Balancing,
>;

fn test_for_network<T: MinerConfig>()
where
	UntypedSnapshotOf<T>: Decode,
{
	let snapshot = HackyGetSnapshot::<T>::snapshot();
	let desired_targets = HackyGetSnapshot::<T>::desired_targets();

	let (solution, score, _size, _trimming) =
		Miner::<T>::mine_solution_with_snapshot::<SolverOf<T>>(
			snapshot.voters.clone(),
			snapshot.targets.clone(),
			desired_targets,
		)
		.unwrap();

	let raw_solution = RawSolution { round: 0, solution, score };

	let _ready_solution = Miner::<T>::feasibility_check(
		raw_solution,
		ElectionCompute::Signed,
		desired_targets,
		snapshot,
		0,
		Default::default(),
	)
	.unwrap();
}

#[tokio::test]
async fn mine_for_polkadot() {
	sp_tracing::try_init_simple();

	// good way to find good block hashes: https://polkadot.subscan.io/event?page=1&time_dimension=date&module=electionprovidermultiphase&event_id=solutionstored
	// we are just looking for blocks with snapshot present, that's all.
	let block_hash_str = std::option_env!("BLOCK_HASH")
		// known good polkadot hash
		.unwrap_or("047f1f5b1081fdaa72c9224d0ea302553738556758dc53269b1bfe6a069986bb")
		.to_string();
	let block_hash = H256::from_slice(hex::decode(block_hash_str).unwrap().as_ref());
	let online = OnlineConfig {
		at: Some(block_hash),
		pallets: vec!["ElectionProviderMultiPhase".to_string()],
		transport_uris: vec![std::option_env!("WS")
			.unwrap_or("wss://rpc.ibp.network/polkadot")
			.to_string()],
		..Default::default()
	};

	let _ = Builder::<FakeBlock>::default()
		.mode(Mode::Online(online))
		.build()
		.await
		.unwrap()
		.execute_with(|| {
			test_for_network::<polkadot::MinerConfig>();
		});
}

#[tokio::test]
async fn mine_for_kusama() {
	sp_tracing::try_init_simple();

	// good way to find good block hashes: https://kusama.subscan.io/event?page=1&time_dimension=date&module=electionprovidermultiphase&event_id=solutionstored
	// we are just looking for blocks with snapshot present, that's all.
	let block_hash_str = std::option_env!("BLOCK_HASH")
		// known good kusama hash
		.unwrap_or("d5d9f5e098fcb41915c85e6695eddc18c0bc4aa4976ad0d9bf5f4713039bca26")
		.to_string();
	let block_hash = H256::from_slice(hex::decode(block_hash_str).unwrap().as_ref());
	let online = OnlineConfig {
		at: Some(block_hash),
		pallets: vec!["ElectionProviderMultiPhase".to_string()],
		transport_uris: vec![std::option_env!("WS")
			.unwrap_or("wss://rpc.ibp.network/kusama")
			.to_string()],
		..Default::default()
	};

	let _ = Builder::<FakeBlock>::default()
		.mode(Mode::Online(online))
		.build()
		.await
		.unwrap()
		.execute_with(|| {
			test_for_network::<kusama::MinerConfig>();
		});
}
