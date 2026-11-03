// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

use crate::collators::{
	slot_based::relay_chain_data_cache::{RelayChainData, RelayChainDataCache},
	RelayHeader,
};
use cumulus_client_consensus_common::get_relay_slot;
use cumulus_primitives_aura::Slot;
use cumulus_relay_chain_interface::RelayChainInterface;
use futures::{
	prelude::*,
	stream::{Fuse, FusedStream},
};
use polkadot_node_subsystem::gen::{stream::Stream, FutureExt};
use polkadot_primitives::Block as RelayBlock;
use sc_consensus_aura::SlotDuration;
use sp_runtime::traits::Header as HeaderT;
use sp_timestamp::Timestamp;
use std::{marker::PhantomData, pin::Pin, time::Duration};

fn get_current_relay_slot_at(
	now: Duration,
	slot_offset: Duration,
	relay_chain_slot_duration: Duration,
) -> Slot {
	let now = now.saturating_sub(slot_offset);
	Slot::from_timestamp(
		Timestamp::from(now),
		SlotDuration::from_millis(relay_chain_slot_duration.as_millis() as u64),
	)
}

fn get_current_relay_slot(slot_offset: Duration, relay_chain_slot_duration: Duration) -> Slot {
	get_current_relay_slot_at(
		Timestamp::current().as_duration(),
		slot_offset,
		relay_chain_slot_duration,
	)
}

/// Picks a scheduling parent for the next collation under V2 or V3 semantics.
///
/// The two policies differ in which relay block they build on, and consequently in
/// how they tolerate relay block propagation delay. Selected per-call based on whether
/// V3 is enabled on both the parachain (runtime API) and the relay chain
/// (`CandidateReceiptV3` node feature):
///
/// - **V2**: build on the *current* slot's relay block. Tolerated via a fixed 1s
///   `slot_offset` — the relay block must arrive within ~1s of the slot starting. If
///   not, we wait for it before building, so we don't end up using the previous slot's
///   relay block past our own slot. See
///   <https://github.com/paritytech/polkadot-sdk/pull/11453>.
/// - **V3**: build on the *last finished* slot's relay block. No offset hack, no waiting: the relay
///   block had a full slot to propagate, which is what slots are for. Matches the low-latency v2
///   design.
///
/// Owns the relay chain new-best notification stream so [`Self::wait_for_scheduling_parent`]
/// can block for a fresh leaf. Initial state is a terminated empty stream. The caller
/// must call [`Self::ensure_initialized`] before any call to [`Self::wait_for_scheduling_parent`],
/// in order to make sure that the stream is installed/re-installed if needed.
pub(crate) struct SchedulingInfo<RelayClient> {
	best_notifications: Fuse<Pin<Box<dyn Stream<Item = RelayHeader> + Send>>>,
	relay_slot_duration: Duration,
	slot_offset: Duration,
	maybe_best_relay_header: Option<RelayHeader>,

	_phantom: PhantomData<RelayClient>,
}

impl<RelayClient: RelayChainInterface + 'static> SchedulingInfo<RelayClient> {
	/// Create a new `SchedulingInfo` with no active notification stream.
	///
	/// The caller must call [`Self::ensure_initialized`] before the first
	/// `wait_for_scheduling_parent` invocation.
	pub fn new(relay_chain_slot_duration: Duration, slot_offset: Duration) -> Self {
		let stream: Pin<Box<dyn Stream<Item = RelayHeader> + Send>> =
			Box::pin(futures::stream::empty());
		let mut stream = stream.fuse();
		// Force the fused stream into the terminated state so the first
		// `should_reinit` call returns `true`.
		stream.next().now_or_never();

		Self {
			best_notifications: stream,
			relay_slot_duration: relay_chain_slot_duration,
			slot_offset,
			maybe_best_relay_header: None,
			_phantom: Default::default(),
		}
	}

	async fn get_best_relay_block_data<'a>(
		relay_client: &RelayClient,
		relay_chain_data_cache: &'a mut RelayChainDataCache<RelayClient>,
	) -> Result<&'a RelayChainData, ()> {
		let best_relay_hash = relay_client.best_block_hash().await.map_err(|_| ())?;
		relay_chain_data_cache.get_by_hash(best_relay_hash).await.map_err(|_| ())
	}

	/// `true` if the best-block notification stream is terminated and must be replaced
	/// before the next `wait_for_scheduling_parent` call.
	///
	/// Returns `true` both at startup (the initial stream is a terminated empty stream)
	/// and after the underlying subscription has ended.
	fn should_reinit(&self) -> bool {
		self.best_notifications.is_terminated()
	}

	pub async fn ensure_initialized<'a>(
		&'a mut self,
		relay_client: &RelayClient,
		relay_chain_data_cache: &'a mut RelayChainDataCache<RelayClient>,
	) -> Option<&'a RelayChainData> {
		if !self.should_reinit() {
			return None;
		}

		match relay_client.new_best_notification_stream().await {
			Ok(best_notifications) => {
				self.best_notifications = best_notifications.fuse();
			},
			Err(err) => {
				tracing::error!(
					target: crate::LOG_TARGET,
					?err,
					"Failed to reset the relay chain best block notification stream. \
					The next call to `wait_for_scheduling_parent` might fail."
				);
			},
		};

		let best_relay_block_data =
			match Self::get_best_relay_block_data(relay_client, relay_chain_data_cache).await {
				Ok(best_relay_block_data) => best_relay_block_data,
				Err(()) => {
					tracing::error!(
						target: crate::LOG_TARGET,
						"Failed to get the `RelayChainData` for the best relay chain block. \
						The next call to `wait_for_scheduling_parent` might fail."
					);
					return None;
				},
			};
		self.maybe_best_relay_header = Some(best_relay_block_data.relay_header.clone());

		Some(best_relay_block_data)
	}

	pub fn is_v3_enabled(
		v3_enabled_on_para: bool,
		relay_chain_data: Option<&RelayChainData>,
	) -> bool {
		v3_enabled_on_para && relay_chain_data.map_or(false, |data| data.is_v3_enabled())
	}

	/// Pick a scheduling parent under the policy described on [`SchedulingInfo`],
	/// blocking on the notification stream until one is available.
	///
	/// V3 is used iff `v3_enabled_on_para` is true *and* the relay chain has the
	/// `CandidateReceiptV3` node feature set at the candidate block; otherwise V2.
	/// Under V3, if the best leaf's slot is still in progress, walks back to its
	/// parent — and aborts when that crosses a BABE epoch boundary, since the
	/// scheduling parent must share a session with the active leaf.
	///
	/// Returns `Some((header, v3_used))`, or `None` on relay client error, a session
	/// boundary, or a terminated notification stream.
	pub async fn wait_for_scheduling_parent(
		&mut self,
		relay_chain_data_cache: &mut RelayChainDataCache<RelayClient>,
		v3_enabled_on_para: bool,
	) -> Option<(RelayHeader, bool)> {
		let mut maybe_best_relay_header = self.maybe_best_relay_header.take();
		let (best_relay_slot, best_relay_header_data) = loop {
			// Drain buffered notifications.
			while let Some(Some(header)) = self.best_notifications.next().now_or_never() {
				maybe_best_relay_header = Some(header);
			}

			let best_relay_header = match maybe_best_relay_header.take() {
				Some(header) => header,
				None => self.best_notifications.next().await?,
			};
			self.maybe_best_relay_header = Some(best_relay_header.clone());
			let best_relay_header_data =
				relay_chain_data_cache.get_by_header(best_relay_header).await.ok()?;
			let best_relay_slot = get_relay_slot(&best_relay_header_data.relay_header)?;

			let v3_enabled = Self::is_v3_enabled(v3_enabled_on_para, Some(&best_relay_header_data));
			if v3_enabled {
				// For scheduling v3 we don't need to loop since we need to return a
				// scheduling parent associated with a finished slot.
				break (best_relay_slot, best_relay_header_data);
			}

			// For v2, we need to loop until we find a scheduling parent associated with a
			// current slot.
			if best_relay_slot >= get_current_relay_slot(self.slot_offset, self.relay_slot_duration)
			{
				return Some((best_relay_header_data.relay_header.clone(), false));
			}
		};

		// v3: walk back to the first finished slot
		let mut scheduling_parent_data = best_relay_header_data;
		let mut scheduling_parent_slot = best_relay_slot;
		while scheduling_parent_slot >=
			get_current_relay_slot(Duration::ZERO, self.relay_slot_duration)
		{
			// The scheduling parent should be part of the same session as the best
			// relay block.
			if sc_consensus_babe::contains_epoch_change::<RelayBlock>(
				&scheduling_parent_data.relay_header,
			) {
				return None;
			}

			let ancestor_hash = *scheduling_parent_data.relay_header.parent_hash();
			scheduling_parent_data =
				relay_chain_data_cache.get_by_hash(ancestor_hash).await.ok()?;
			scheduling_parent_slot = get_relay_slot(&scheduling_parent_data.relay_header)?
		}

		Some((scheduling_parent_data.relay_header.clone(), true))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::collators::slot_based::{
		tests,
		tests::{babe_epoch_change_digest_item, TestRelayClient},
	};
	use polkadot_primitives::{node_features::FeatureIndex, NodeFeatures};
	use std::collections::HashMap;

	const RELAY_SLOT_DURATION: Duration = Duration::from_secs(6);

	/// Simulate the wall clock at a specific point within a relay slot.
	///
	/// `relay_slot` is the current relay chain slot number, `ms_into_slot` is
	/// how far into that slot we are (0..6000).
	fn now_at(relay_slot: u64, ms_into_slot: u64) -> Duration {
		Duration::from_millis(relay_slot * 6000 + ms_into_slot)
	}

	#[test]
	fn get_current_relay_slot_at_works_correctly() {
		// beginning of slot
		assert_eq!(
			get_current_relay_slot_at(
				now_at(804, 0),
				Duration::from_millis(0),
				RELAY_SLOT_DURATION
			),
			Slot::from(804)
		);

		// end of slot
		assert_eq!(
			get_current_relay_slot_at(
				now_at(804, 5999),
				Duration::from_millis(0),
				RELAY_SLOT_DURATION
			),
			Slot::from(804)
		);

		// offset, but still inside slot
		assert_eq!(
			get_current_relay_slot_at(
				now_at(805, 500),
				Duration::from_millis(500),
				RELAY_SLOT_DURATION
			),
			Slot::from(805)
		);

		// offset => previous slot
		assert_eq!(
			get_current_relay_slot_at(
				now_at(805, 500),
				Duration::from_millis(501),
				RELAY_SLOT_DURATION
			),
			Slot::from(804)
		);
	}

	fn build_mock_chain(
		relay_slot_duration: Duration,
		v3_enabled: bool,
	) -> (TestRelayClient, RelayChainDataCache<TestRelayClient>, Vec<RelayHeader>) {
		let current_slot = *get_current_relay_slot(Duration::ZERO, relay_slot_duration);
		let mut node_features = NodeFeatures::from_vec(vec![0; 5]);
		if v3_enabled {
			node_features.set(FeatureIndex::CandidateReceiptV3 as usize, true);
		}

		let mut headers = vec![];
		// very old header
		headers.push(tests::relay_header_with_slot(10, Default::default(), 0));
		// 2 more recent headers from finished slots
		headers.push(tests::relay_header_with_slot(
			50,
			headers.last().unwrap().hash(),
			current_slot - 2,
		));
		headers.push(tests::relay_header_with_slot(
			51,
			headers.last().unwrap().hash(),
			current_slot - 1,
		));
		// 2 future headers
		headers.push(tests::relay_header_with_slot(
			100,
			headers.last().unwrap().hash(),
			current_slot + 10,
		));
		headers.push(tests::relay_header_with_slot(
			101,
			headers.last().unwrap().hash(),
			current_slot + 11,
		));

		let mut headers_map = HashMap::new();
		for header in &headers {
			headers_map.insert(header.hash(), header.clone());
		}
		let client = TestRelayClient::new_with_best(headers_map, headers.last().unwrap().hash());

		let mut cache = RelayChainDataCache::new(client.clone(), 1.into());
		for header in &headers {
			cache.set_test_data(header.clone(), vec![], node_features.clone());
		}

		(client, cache, headers)
	}

	#[tokio::test]
	async fn reset_best_notifications_works() {
		let best_header = tests::relay_header_with_slot(10, Default::default(), 0);
		let mut client = TestRelayClient::new(Default::default());
		let mut cache = RelayChainDataCache::new(client.clone(), 1.into());
		cache.set_test_data(best_header.clone(), vec![], Default::default());

		let mut scheduling_info =
			SchedulingInfo::new(Duration::from_secs(6), Duration::from_secs(1));
		assert_eq!(scheduling_info.should_reinit(), true);
		assert_eq!(scheduling_info.maybe_best_relay_header, None);

		let (tx, rx) = futures::channel::mpsc::unbounded::<RelayHeader>();
		client.set_best_hash(Some(best_header.hash()));
		client.set_best_notifications(Box::pin(rx));
		scheduling_info.ensure_initialized(&client, &mut cache).await;
		assert_eq!(scheduling_info.maybe_best_relay_header.as_ref(), Some(&best_header));
		assert_eq!(scheduling_info.should_reinit(), false);

		let best_header_2 = tests::relay_header_with_slot(11, Default::default(), 100);
		client.set_best_hash(Some(best_header_2.hash()));
		client.set_best_notifications(Box::pin(futures::stream::empty()));
		cache.set_test_data(best_header_2.clone(), vec![], Default::default());
		scheduling_info.ensure_initialized(&client, &mut cache).await;
		assert_eq!(scheduling_info.maybe_best_relay_header.as_ref(), Some(&best_header));
		assert_eq!(scheduling_info.should_reinit(), false);

		tx.close_channel();
		scheduling_info.wait_for_scheduling_parent(&mut cache, false).await;
		assert_eq!(scheduling_info.should_reinit(), true);
	}

	/// Test the original bug scenario: relay block propagation exceeds `slot_offset`,
	/// causing the collator to see a stale relay parent at a slot boundary.
	///
	/// `wait_for_scheduling_parent` must block until a fresh relay block arrives
	/// (via the notification stream), then return that block's hash.
	#[tokio::test]
	async fn v2_wait_for_scheduling_parent_waits_when_stale() {
		let relay_slot_duration = Duration::from_secs(6);
		let slot_offset = Duration::from_secs(1);

		let (mut client, mut cache, headers) = build_mock_chain(relay_slot_duration, false);

		let (tx, rx) = futures::channel::mpsc::unbounded::<RelayHeader>();
		client.set_best_hash(Some(headers[0].hash()));
		client.set_best_notifications(Box::pin(rx));

		let mut scheduling_info = SchedulingInfo::new(relay_slot_duration, slot_offset);
		scheduling_info.ensure_initialized(&client, &mut cache).await;

		let mut handle = tokio::spawn(async move {
			scheduling_info.wait_for_scheduling_parent(&mut cache, false).await
		});

		// The function should not return before receiving a notification — the best block (slot 0)
		// is stale.
		assert!(
			tokio::time::timeout(Duration::from_millis(300), &mut handle).await.is_err(),
			"Should be waiting for fresh relay block, not returning immediately"
		);

		// Simulate: relay block from finished slot arrives.
		tx.unbounded_send(headers[1].clone()).unwrap();
		assert!(
			tokio::time::timeout(Duration::from_millis(300), &mut handle).await.is_err(),
			"Should be waiting for fresh relay block, not returning immediately"
		);

		// Simulate: relay block from fresh slot arrives.
		tx.unbounded_send(headers[3].clone()).unwrap();
		let result = tokio::time::timeout(Duration::from_millis(300), handle)
			.await
			.expect("Task should complete within timeout")
			.expect("Task should not panic");
		assert_eq!(result, Some((headers[3].clone(), false)));
	}

	/// When the best relay block is already current, `wait_for_scheduling_parent`
	/// should return immediately without waiting for any notification.
	#[tokio::test]
	async fn v2_wait_for_scheduling_parent_returns_immediately_when_fresh() {
		let relay_slot_duration = Duration::from_secs(6);
		let slot_offset = Duration::from_secs(1);

		let (mut client, mut cache, headers) = build_mock_chain(relay_slot_duration, false);

		// Create a notification stream that will never produce (no sender).
		let (_tx, rx) = futures::channel::mpsc::unbounded::<RelayHeader>();
		client.set_best_hash(Some(headers[4].hash()));
		client.set_best_notifications(Box::pin(rx));

		let mut scheduling_info = SchedulingInfo::new(relay_slot_duration, slot_offset);
		scheduling_info.ensure_initialized(&client, &mut cache).await;
		let result = tokio::time::timeout(
			Duration::from_millis(300),
			scheduling_info.wait_for_scheduling_parent(&mut cache, false),
		)
		.await
		.expect("Should return immediately, not timeout");

		assert_eq!(result, Some((headers[4].clone(), false)));
	}

	#[tokio::test]
	async fn v3_wait_for_scheduling_parent_returns_finished_slot() {
		let relay_slot_duration = Duration::from_secs(6);
		let slot_offset = Duration::from_secs(1);

		let (mut client, mut cache, headers) = build_mock_chain(relay_slot_duration, true);

		let (tx, rx) = futures::channel::mpsc::unbounded::<RelayHeader>();
		client.set_best_hash(None);
		client.set_best_notifications(Box::pin(rx));

		let mut scheduling_info = SchedulingInfo::new(relay_slot_duration, slot_offset);
		scheduling_info.ensure_initialized(&client, &mut cache).await;

		let mut handle = tokio::spawn(async move {
			scheduling_info.wait_for_scheduling_parent(&mut cache, true).await
		});

		// The function should not return before receiving a notification.
		assert!(
			tokio::time::timeout(Duration::from_millis(300), &mut handle).await.is_err(),
			"Should be waiting for fresh relay block, not returning immediately"
		);

		// Simulate: relay block from finished slot arrives.
		tx.unbounded_send(headers[2].clone()).unwrap();
		let result = tokio::time::timeout(Duration::from_millis(300), handle)
			.await
			.expect("Task should complete within timeout")
			.expect("Task should not panic");
		assert_eq!(result, Some((headers[2].clone(), true)));
	}

	#[tokio::test]
	async fn v3_wait_for_scheduling_parent_walks_back_when_fresh_slot() {
		let relay_slot_duration = Duration::from_secs(6);
		let slot_offset = Duration::from_secs(1);

		let (mut client, mut cache, headers) = build_mock_chain(relay_slot_duration, true);

		let (tx, rx) = futures::channel::mpsc::unbounded::<RelayHeader>();
		client.set_best_hash(None);
		client.set_best_notifications(Box::pin(rx));

		let mut scheduling_info = SchedulingInfo::new(relay_slot_duration, slot_offset);
		scheduling_info.ensure_initialized(&client, &mut cache).await;

		let mut handle = tokio::spawn(async move {
			scheduling_info.wait_for_scheduling_parent(&mut cache, true).await
		});

		// The function should not return before receiving a notification.
		assert!(
			tokio::time::timeout(Duration::from_millis(300), &mut handle).await.is_err(),
			"Should be waiting for fresh relay block, not returning immediately"
		);

		// Simulate: relay block from fresh slot arrives.
		tx.unbounded_send(headers[4].clone()).unwrap();
		let result = tokio::time::timeout(Duration::from_millis(300), handle)
			.await
			.expect("Task should complete within timeout")
			.expect("Task should not panic");
		assert_eq!(result, Some((headers[2].clone(), true)));
	}

	#[tokio::test]
	async fn v3_wait_for_scheduling_parent_checks_session() {
		let relay_slot_duration = Duration::from_secs(6);
		let slot_offset = Duration::from_secs(1);

		let (mut client, mut cache, mut headers) = build_mock_chain(relay_slot_duration, true);

		let (tx, rx) = futures::channel::mpsc::unbounded::<RelayHeader>();
		client.set_best_hash(None);
		client.set_best_notifications(Box::pin(rx));

		let mut scheduling_info = SchedulingInfo::new(relay_slot_duration, slot_offset);
		scheduling_info.ensure_initialized(&client, &mut cache).await;

		// Simulate: receiving relay block with header 3 (fresh slot).
		tx.unbounded_send(headers[3].clone()).unwrap();
		let result = tokio::time::timeout(Duration::from_millis(300), async {
			scheduling_info.wait_for_scheduling_parent(&mut cache, true).await
		})
		.await
		.expect("Task should complete within timeout");
		assert_eq!(result, Some((headers[2].clone(), true)));

		// add session change digest at header 3
		let mut node_features = NodeFeatures::from_vec(vec![0; 5]);
		node_features.set(FeatureIndex::CandidateReceiptV3 as usize, true);
		headers[3].digest.push(babe_epoch_change_digest_item());
		cache.set_test_data(headers[3].clone(), vec![], node_features.clone());
		headers[4].parent_hash = headers[3].hash();
		cache.set_test_data(headers[4].clone(), vec![], node_features);

		// Simulate: receiving the modified header 3 block.
		tx.unbounded_send(headers[3].clone()).unwrap();
		let result = tokio::time::timeout(Duration::from_millis(300), async {
			scheduling_info.wait_for_scheduling_parent(&mut cache, true).await
		})
		.await
		.expect("Task should complete within timeout");
		assert_eq!(result, None);
		assert_eq!(scheduling_info.maybe_best_relay_header.as_ref(), Some(&headers[3]));

		// Simulate: an even fresher block.
		tx.unbounded_send(headers[4].clone()).unwrap();
		let result = tokio::time::timeout(Duration::from_millis(300), async {
			scheduling_info.wait_for_scheduling_parent(&mut cache, true).await
		})
		.await
		.expect("Task should complete within timeout");
		assert_eq!(result, None);
		assert_eq!(scheduling_info.maybe_best_relay_header.as_ref(), Some(&headers[4]));
	}
}
