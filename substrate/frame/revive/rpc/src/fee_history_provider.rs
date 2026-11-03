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
use crate::{ClientError, FeeHistoryResult, ReceiptInfo, client::SubstrateBlockNumber};
use pallet_revive_types::runtime_api::BlockV1;
use sp_core::U256;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;

/// The size of the fee history cache.
const CACHE_SIZE: u32 = 1024;

/// The maximum number of reward percentiles accepted in a single `eth_feeHistory` request,
/// matching go-ethereum's query limit.
const MAX_REWARD_PERCENTILES: usize = 100;

#[derive(Default, Clone)]
struct FeeHistoryCacheItem {
	base_fee: u128,
	gas_used_ratio: f64,
	rewards: Vec<u128>,
}

/// Maps a reward percentile requested via `eth_feeHistory` to its bucket index in
/// [`FeeHistoryCacheItem::rewards`].
///
/// Rewards are cached at half-percentile resolution, i.e. one bucket per `0.5` percent over
/// `0.0..=100.0` (201 buckets). The index is therefore the percentile, clamped to that range and
/// scaled to the bucket resolution, rounded to the nearest bucket.
fn reward_bucket_index(percentile: f64) -> usize {
	// Two buckets per whole percentile, hence the factor of two.
	(percentile.clamp(0.0, 100.0) * 2f64).round() as usize
}

/// Validates the reward percentiles requested via `eth_feeHistory`.
///
/// Matching go-ethereum, the list may hold at most [`MAX_REWARD_PERCENTILES`] entries, each
/// percentile must lie within `0.0..=100.0`, and the list must be strictly increasing; otherwise
/// the request is rejected rather than silently clamped or approximated at the wrong bucket.
/// Returns the rejection reason as the error message.
pub(crate) fn validate_reward_percentiles(percentiles: &[f64]) -> Result<(), String> {
	if percentiles.len() > MAX_REWARD_PERCENTILES {
		return Err(format!(
			"invalid reward percentile: over the query limit {MAX_REWARD_PERCENTILES}"
		));
	}
	for (i, &p) in percentiles.iter().enumerate() {
		if !(0.0..=100.0).contains(&p) {
			return Err(format!("invalid reward percentile: {p}"));
		}
		if i > 0 && p <= percentiles[i - 1] {
			return Err(format!(
				"invalid reward percentile: #{}:{} >= #{}:{}",
				i - 1,
				percentiles[i - 1],
				i,
				p
			));
		}
	}
	Ok(())
}

/// Manages the fee history cache.
#[derive(Default, Clone)]
pub struct FeeHistoryProvider {
	fee_history_cache: Arc<RwLock<BTreeMap<SubstrateBlockNumber, FeeHistoryCacheItem>>>,
}

impl FeeHistoryProvider {
	/// Update the fee history cache with the given block and receipts.
	pub async fn update_fee_history(&self, block: &BlockV1, receipts: &[ReceiptInfo]) {
		// Evenly spaced percentile list from 0.0 to 100.0 with a 0.5 resolution.
		// This means we cache 200 percentile points.
		// Later in request handling we will approximate by rounding percentiles that
		// fall in between with `(round(n*2)/2)`.
		let reward_percentiles: Vec<f64> = (0..=200).map(|i| i as f64 * 0.5).collect();
		let block_number: SubstrateBlockNumber =
			block.number.try_into().expect("Block number is always valid");

		let base_fee = block.base_fee_per_gas.as_u128();
		let gas_used = block.gas_used.as_u128();
		let gas_used_ratio = (gas_used as f64) / (block.gas_limit.as_u128() as f64);
		let mut result = FeeHistoryCacheItem { base_fee, gas_used_ratio, rewards: vec![] };

		let mut receipts = receipts
			.iter()
			.map(|receipt| {
				let gas_used = receipt.gas_used.as_u128();
				let effective_reward =
					receipt.effective_gas_price.as_u128().saturating_sub(base_fee);
				(gas_used, effective_reward)
			})
			.collect::<Vec<_>>();
		receipts.sort_by(|(_, a), (_, b)| a.cmp(b));

		// Calculate percentile rewards.
		result.rewards = reward_percentiles
			.into_iter()
			.filter_map(|p| {
				let target_gas = (p * gas_used as f64 / 100f64) as u128;
				let mut sum_gas = 0u128;
				for (gas_used, reward) in &receipts {
					sum_gas += gas_used;
					if target_gas <= sum_gas {
						return Some(*reward);
					}
				}
				None
			})
			.collect();

		let mut cache = self.fee_history_cache.write().await;
		if cache.len() >= CACHE_SIZE as usize {
			cache.pop_first();
		}
		cache.insert(block_number, result);
	}

	/// Get the fee history for the given block range.
	pub async fn fee_history(
		&self,
		block_count: u32,
		highest: SubstrateBlockNumber,
		reward_percentiles: Option<Vec<f64>>,
	) -> Result<FeeHistoryResult, ClientError> {
		let block_count = block_count.min(CACHE_SIZE);

		let cache = self.fee_history_cache.read().await;
		let Some(lowest_in_cache) = cache.first_key_value().map(|(k, _)| *k) else {
			return Ok(FeeHistoryResult {
				oldest_block: U256::zero(),
				base_fee_per_gas: vec![],
				gas_used_ratio: vec![],
				reward: vec![],
			});
		};

		let lowest = highest
			.saturating_sub(SubstrateBlockNumber::from(block_count.saturating_sub(1)))
			.max(lowest_in_cache);

		let mut response = FeeHistoryResult {
			oldest_block: U256::from(lowest),
			base_fee_per_gas: Vec::new(),
			gas_used_ratio: Vec::new(),
			reward: Default::default(),
		};

		let rewards = &mut response.reward;
		// Iterate over the requested block range.
		for n in lowest..=highest {
			if let Some(block) = cache.get(&n) {
				response.base_fee_per_gas.push(U256::from(block.base_fee));
				response.gas_used_ratio.push(block.gas_used_ratio);
				// If the request includes reward percentiles, get them from the cache.
				if let Some(ref requested_percentiles) = reward_percentiles {
					let mut block_rewards = Vec::new();
					// Get cached reward for each provided percentile.
					for p in requested_percentiles {
						// Get and push the reward.
						let reward = if let Some(r) = block.rewards.get(reward_bucket_index(*p)) {
							U256::from(*r)
						} else {
							U256::zero()
						};
						block_rewards.push(reward);
					}
					// Push block rewards.
					if !block_rewards.is_empty() {
						rewards.push(block_rewards);
					}
				}
			}
		}

		// Next block base fee, use constant value for now
		let base_fee = cache
			.last_key_value()
			.map(|(_, block)| U256::from(block.base_fee))
			.unwrap_or_default();
		response.base_fee_per_gas.push(base_fee);
		Ok(response)
	}
}

#[tokio::test]
async fn test_update_fee_history() {
	let block = BlockV1 {
		number: U256::from(200u64),
		base_fee_per_gas: U256::from(1000u64),
		gas_used: U256::from(600u64),
		gas_limit: U256::from(1200u64),
		..Default::default()
	};

	let receipts = vec![
		ReceiptInfo {
			gas_used: U256::from(200u64),
			effective_gas_price: U256::from(1200u64),
			..Default::default()
		},
		ReceiptInfo {
			gas_used: U256::from(200u64),
			effective_gas_price: U256::from(1100u64),
			..Default::default()
		},
		ReceiptInfo {
			gas_used: U256::from(200u64),
			effective_gas_price: U256::from(1050u64),
			..Default::default()
		},
	];

	let provider = FeeHistoryProvider { fee_history_cache: Arc::new(RwLock::new(BTreeMap::new())) };
	provider.update_fee_history(&block, &receipts).await;

	let fee_history_result =
		provider.fee_history(1, 200, Some(vec![0.0f64, 50.0, 100.0])).await.unwrap();

	let expected_result = FeeHistoryResult {
		oldest_block: U256::from(200),
		base_fee_per_gas: vec![U256::from(1000), U256::from(1000)],
		gas_used_ratio: vec![0.5f64],
		reward: vec![vec![U256::from(50), U256::from(100), U256::from(200)]],
	};
	assert_eq!(fee_history_result, expected_result);
}

#[test]
fn reward_bucket_index_matches_half_percentile_resolution() {
	// Whole percentiles map to even buckets.
	assert_eq!(reward_bucket_index(0.0), 0);
	assert_eq!(reward_bucket_index(50.0), 100);
	assert_eq!(reward_bucket_index(100.0), 200);

	// Half percentiles are addressable and are not snapped to a neighbouring whole percentile.
	assert_eq!(reward_bucket_index(0.5), 1);
	assert_eq!(reward_bucket_index(50.5), 101);

	// Out-of-range percentiles are clamped to the valid range.
	assert_eq!(reward_bucket_index(-1.0), 0);
	assert_eq!(reward_bucket_index(150.0), 200);
}

#[test]
fn validate_reward_percentiles_matches_geth() {
	// Valid: within range and strictly increasing (including half-percentiles).
	assert!(validate_reward_percentiles(&[]).is_ok());
	assert!(validate_reward_percentiles(&[0.0, 20.0, 50.5, 100.0]).is_ok());

	// Out-of-range percentiles are rejected rather than clamped.
	assert!(validate_reward_percentiles(&[-0.1]).is_err());
	assert!(validate_reward_percentiles(&[100.1]).is_err());

	// A non-increasing list is rejected, including equal adjacent values.
	assert!(validate_reward_percentiles(&[50.0, 20.0]).is_err());
	assert!(validate_reward_percentiles(&[20.0, 20.0]).is_err());

	// The number of percentiles is capped: a list at the limit is accepted, one over is not.
	let at_limit: Vec<f64> = (0..MAX_REWARD_PERCENTILES).map(|i| i as f64 * 0.5).collect();
	assert!(validate_reward_percentiles(&at_limit).is_ok());
	let over_limit: Vec<f64> = (0..=MAX_REWARD_PERCENTILES).map(|i| i as f64 * 0.5).collect();
	assert!(validate_reward_percentiles(&over_limit).is_err());
}
