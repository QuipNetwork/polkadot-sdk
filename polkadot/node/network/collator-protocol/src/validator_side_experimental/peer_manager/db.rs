// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use crate::validator_side_experimental::{
	common::Score,
	peer_manager::{backend::Backend, ReputationUpdate, ReputationUpdateKind},
};
use async_trait::async_trait;
use polkadot_node_network_protocol::PeerId;
use polkadot_primitives::{BlockNumber, Id as ParaId};
use sp_runtime::{traits::Bounded, FixedPointNumber, FixedU128};
use std::collections::{btree_map, hash_map, BTreeMap, BTreeSet, HashMap};

/// This is an in-memory temporary implementation for the DB, to be used only for prototyping and
/// testing purposes.
pub struct Db {
	db: BTreeMap<ParaId, HashMap<PeerId, ScoreEntry>>,
	last_finalized: Option<BlockNumber>,
	stored_limit_per_para: u16,
}

impl Db {
	/// Create a new instance of the in-memory DB.
	///
	/// `stored_limit_per_para` is the maximum number of reputations that can be stored per para.
	pub async fn new(stored_limit_per_para: u16) -> Self {
		Self { db: BTreeMap::new(), last_finalized: None, stored_limit_per_para }
	}
}

pub(crate) type Timestamp = u128;

#[derive(Clone, Copy, Debug, codec::Encode, codec::Decode)]
pub(crate) struct ScoreEntry {
	pub(crate) score: Score,
	pub(crate) last_bumped: Timestamp,
}

#[async_trait]
impl Backend for Db {
	async fn processed_finalized_block_number(&self) -> Option<BlockNumber> {
		self.last_finalized
	}

	async fn query(&self, peer_id: &PeerId, para_id: &ParaId) -> Option<Score> {
		self.db.get(para_id).and_then(|per_para| per_para.get(peer_id).map(|e| e.score))
	}

	async fn slash(&mut self, peer_id: &PeerId, para_id: &ParaId, value: Score) {
		if let btree_map::Entry::Occupied(mut per_para_entry) = self.db.entry(*para_id) {
			if let hash_map::Entry::Occupied(mut e) = per_para_entry.get_mut().entry(*peer_id) {
				let score = e.get_mut().score;
				// Remove the entry if it goes to zero.
				if score <= value {
					e.remove();
				} else {
					e.get_mut().score.saturating_sub(value.into());
				}
			}

			// If the per_para length went to 0, remove it completely
			if per_para_entry.get().is_empty() {
				per_para_entry.remove();
			}
		}
	}

	async fn prune_paras(&mut self, registered_paras: BTreeSet<ParaId>) {
		self.db.retain(|para, _| registered_paras.contains(&para));
	}

	async fn process_bumps(
		&mut self,
		leaf_number: BlockNumber,
		bumps: BTreeMap<ParaId, HashMap<PeerId, Score>>,
		decay_value: Option<Score>,
		now: std::time::Duration,
	) -> Vec<ReputationUpdate> {
		if self.last_finalized.unwrap_or(0) >= leaf_number {
			return vec![];
		}

		self.last_finalized = Some(leaf_number);
		self.bump_reputations(bumps, decay_value, now.as_millis())
	}

	async fn max_scores_for_paras(&self, paras: BTreeSet<ParaId>) -> HashMap<ParaId, Score> {
		let mut max_scores = HashMap::with_capacity(paras.len());
		for para in paras {
			if let Some(per_para) = self.db.get(&para) {
				let max_score = per_para.values().map(|e| e.score).max().unwrap_or(Score::new(0));
				max_scores.insert(para, max_score);
			}
		}
		max_scores
	}
}

impl Db {
	fn bump_reputations(
		&mut self,
		bumps: BTreeMap<ParaId, HashMap<PeerId, Score>>,
		maybe_decay_value: Option<Score>,
		now: u128,
	) -> Vec<ReputationUpdate> {
		let mut reported_updates = vec![];

		for (para, bumps_per_para) in bumps {
			reported_updates.reserve(bumps_per_para.len());

			for (peer_id, bump) in bumps_per_para.iter() {
				if u16::from(*bump) == 0 {
					continue;
				}

				self.db
					.entry(para)
					.or_default()
					.entry(*peer_id)
					.and_modify(|e| {
						e.score.saturating_add(u16::from(*bump));
						e.last_bumped = now;
					})
					.or_insert(ScoreEntry { score: *bump, last_bumped: now });

				reported_updates.push(ReputationUpdate {
					peer_id: *peer_id,
					para_id: para,
					value: *bump,
					kind: ReputationUpdateKind::Bump,
				});
			}

			if let btree_map::Entry::Occupied(mut per_para_entry) = self.db.entry(para) {
				if let Some(decay_value) = maybe_decay_value {
					let peers_to_slash = per_para_entry
						.get()
						.keys()
						.filter(|peer_id| !bumps_per_para.contains_key(peer_id))
						.copied()
						.collect::<Vec<PeerId>>();

					for peer_id in peers_to_slash {
						if let hash_map::Entry::Occupied(mut e) =
							per_para_entry.get_mut().entry(peer_id)
						{
							// Remove the entry if it goes to zero.
							if e.get_mut().score <= decay_value {
								let score = e.remove().score;
								reported_updates.push(ReputationUpdate {
									peer_id,
									para_id: para,
									value: score,
									kind: ReputationUpdateKind::Slash,
								});
							} else {
								e.get_mut().score.saturating_sub(decay_value.into());
								reported_updates.push(ReputationUpdate {
									peer_id,
									para_id: para,
									value: decay_value,
									kind: ReputationUpdateKind::Slash,
								});
							}
						}
					}
				}

				let per_para_limit = self.stored_limit_per_para as usize;
				if per_para_entry.get().is_empty() {
					// If the per_para length went to 0, remove it completely
					per_para_entry.remove();
				} else if per_para_entry.get().len() > per_para_limit {
					// We have exceeded the maximum capacity, in which case we need to prune
					let diff = per_para_entry.get().len() - per_para_limit;
					Self::prune_for_para(
						&para,
						&mut per_para_entry,
						diff,
						now,
						&mut reported_updates,
					);
				}
			}
		}

		reported_updates
	}

	// Evicts the entries with minimum `score / (age in milliseconds)` ratio.
	fn prune_for_para(
		para_id: &ParaId,
		per_para: &mut btree_map::OccupiedEntry<ParaId, HashMap<PeerId, ScoreEntry>>,
		diff: usize,
		now: Timestamp,
		reported_updates: &mut Vec<ReputationUpdate>,
	) {
		for _ in 0..diff {
			let (peer_id_to_remove, score) = per_para
				.get()
				.iter()
				.min_by_key(|(_peer, entry)| {
					let age = now.saturating_sub(entry.last_bumped);
					let score = u16::from(entry.score);
					let ratio = FixedU128::checked_from_rational(u128::from(score), age)
						.unwrap_or(FixedU128::max_value());
					// In case of equal ratios, we evict the entry with the lower absolute score.
					// Note: Multiple peers can have the exact same (ratio, score) if they were
					// updated in the same batch (sharing the same `last_bumped` timestamp) and
					// have identical scores. In such cases, the eviction choice is arbitrary.
					(ratio, score)
				})
				.map(|(peer, entry)| (*peer, entry.score))
				.expect("We know there are enough reps over the limit");

			per_para.get_mut().remove(&peer_id_to_remove);

			reported_updates.push(ReputationUpdate {
				peer_id: peer_id_to_remove,
				para_id: *para_id,
				value: score,
				kind: ReputationUpdateKind::Slash,
			});
		}
	}

	/// Get the last finalized block number (for persistence).
	pub(crate) fn get_last_finalized(&self) -> Option<BlockNumber> {
		self.last_finalized
	}

	/// Set the last finalized block number (for loading from disk).
	pub(crate) fn set_last_finalized(&mut self, last_finalized: Option<BlockNumber>) {
		self.last_finalized = last_finalized;
	}

	/// Get reputations for a specific para (for persistence).
	pub(crate) fn get_para_reputations(&self, para_id: &ParaId) -> HashMap<PeerId, ScoreEntry> {
		self.db.get(para_id).cloned().unwrap_or_default()
	}

	/// Set reputations for a specific para (for loading from disk).
	pub(crate) fn set_para_reputations(
		&mut self,
		para_id: ParaId,
		reputations: HashMap<PeerId, ScoreEntry>,
	) {
		self.db.insert(para_id, reputations);
	}

	/// Get all reputations (for persistence).
	pub(crate) fn all_reputations(
		&self,
	) -> impl Iterator<Item = (&ParaId, &HashMap<PeerId, ScoreEntry>)> {
		self.db.iter()
	}

	#[cfg(test)]
	pub(crate) fn len(&self) -> usize {
		self.db.len()
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	#[tokio::test]
	// Test different types of reputation updates and their effects.
	async fn test_reputation_updates() {
		let mut db = Db::new(10).await;
		assert_eq!(db.processed_finalized_block_number().await, None);
		assert_eq!(db.len(), 0);

		// Test empty update with no decay.
		assert!(db
			.process_bumps(10, Default::default(), None, std::time::Duration::ZERO)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(10));
		assert_eq!(db.len(), 0);

		// Test a query on a non-existent entry.
		assert_eq!(db.query(&PeerId::random(), &ParaId::from(1000)).await, None);

		// Test empty update with decay.
		assert!(db
			.process_bumps(11, Default::default(), Some(Score::new(1)), std::time::Duration::ZERO)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(11));
		assert_eq!(db.len(), 0);

		// Test empty update with a leaf number smaller than the latest one.
		assert!(db
			.process_bumps(5, Default::default(), Some(Score::new(1)), std::time::Duration::ZERO)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(11));
		assert_eq!(db.len(), 0);

		// Test an update with zeroed score.
		assert!(db
			.process_bumps(
				12,
				[(ParaId::from(100), [(PeerId::random(), Score::new(0))].into_iter().collect())]
					.into_iter()
					.collect(),
				Some(Score::new(1)),
				std::time::Duration::ZERO
			)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(12));
		assert_eq!(db.len(), 0);

		// Reuse the same 12 block height, it should not be taken into consideration.
		let first_peer_id = PeerId::random();
		let first_para_id = ParaId::from(100);
		assert!(db
			.process_bumps(
				12,
				[(first_para_id, [(first_peer_id, Score::new(10))].into_iter().collect())]
					.into_iter()
					.collect(),
				Some(Score::new(1)),
				std::time::Duration::ZERO
			)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(12));
		assert_eq!(db.len(), 0);
		assert_eq!(db.query(&first_peer_id, &first_para_id).await, None);

		// Test a non-zero update on an empty DB.
		assert_eq!(
			db.process_bumps(
				13,
				[(first_para_id, [(first_peer_id, Score::new(10))].into_iter().collect())]
					.into_iter()
					.collect(),
				Some(Score::new(1)),
				std::time::Duration::ZERO
			)
			.await,
			vec![ReputationUpdate {
				peer_id: first_peer_id,
				para_id: first_para_id,
				kind: ReputationUpdateKind::Bump,
				value: Score::new(10)
			}]
		);
		assert_eq!(db.processed_finalized_block_number().await, Some(13));
		assert_eq!(db.len(), 1);
		assert_eq!(db.query(&first_peer_id, &first_para_id).await.unwrap(), Score::new(10));
		// Query a non-existent peer_id for this para.
		assert_eq!(db.query(&PeerId::random(), &first_para_id).await, None);
		// Query this peer's rep for a different para.
		assert_eq!(db.query(&first_peer_id, &ParaId::from(200)).await, None);

		// Test a subsequent update with a lower block height. Will be ignored.
		assert!(db
			.process_bumps(
				10,
				[(first_para_id, [(first_peer_id, Score::new(10))].into_iter().collect())]
					.into_iter()
					.collect(),
				Some(Score::new(1)),
				std::time::Duration::ZERO
			)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(13));
		assert_eq!(db.len(), 1);
		assert_eq!(db.query(&first_peer_id, &first_para_id).await.unwrap(), Score::new(10));

		let second_para_id = ParaId::from(200);
		let second_peer_id = PeerId::random();
		// Test a subsequent update with no decay.
		assert_eq!(
			db.process_bumps(
				14,
				[
					(first_para_id, [(second_peer_id, Score::new(10))].into_iter().collect()),
					(second_para_id, [(first_peer_id, Score::new(5))].into_iter().collect())
				]
				.into_iter()
				.collect(),
				None,
				std::time::Duration::ZERO
			)
			.await,
			vec![
				ReputationUpdate {
					peer_id: second_peer_id,
					para_id: first_para_id,
					kind: ReputationUpdateKind::Bump,
					value: Score::new(10)
				},
				ReputationUpdate {
					peer_id: first_peer_id,
					para_id: second_para_id,
					kind: ReputationUpdateKind::Bump,
					value: Score::new(5)
				}
			]
		);
		assert_eq!(db.len(), 2);
		assert_eq!(db.processed_finalized_block_number().await, Some(14));
		assert_eq!(db.query(&first_peer_id, &first_para_id).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&second_peer_id, &first_para_id).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&first_peer_id, &second_para_id).await.unwrap(), Score::new(5));

		// Empty update with decay has no effect.
		assert!(db
			.process_bumps(15, Default::default(), Some(Score::new(1)), std::time::Duration::ZERO)
			.await
			.is_empty());
		assert_eq!(db.processed_finalized_block_number().await, Some(15));
		assert_eq!(db.len(), 2);
		assert_eq!(db.query(&first_peer_id, &first_para_id).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&second_peer_id, &first_para_id).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&first_peer_id, &second_para_id).await.unwrap(), Score::new(5));

		// Test a subsequent update with decay.
		assert_eq!(
			db.process_bumps(
				16,
				[
					(first_para_id, [(first_peer_id, Score::new(10))].into_iter().collect()),
					(second_para_id, [(second_peer_id, Score::new(10))].into_iter().collect()),
				]
				.into_iter()
				.collect(),
				Some(Score::new(1)),
				std::time::Duration::ZERO
			)
			.await,
			vec![
				ReputationUpdate {
					peer_id: first_peer_id,
					para_id: first_para_id,
					kind: ReputationUpdateKind::Bump,
					value: Score::new(10)
				},
				ReputationUpdate {
					peer_id: second_peer_id,
					para_id: first_para_id,
					kind: ReputationUpdateKind::Slash,
					value: Score::new(1)
				},
				ReputationUpdate {
					peer_id: second_peer_id,
					para_id: second_para_id,
					kind: ReputationUpdateKind::Bump,
					value: Score::new(10)
				},
				ReputationUpdate {
					peer_id: first_peer_id,
					para_id: second_para_id,
					kind: ReputationUpdateKind::Slash,
					value: Score::new(1)
				},
			]
		);
		assert_eq!(db.processed_finalized_block_number().await, Some(16));
		assert_eq!(db.len(), 2);
		assert_eq!(db.query(&first_peer_id, &first_para_id).await.unwrap(), Score::new(20));
		assert_eq!(db.query(&second_peer_id, &first_para_id).await.unwrap(), Score::new(9));
		assert_eq!(db.query(&first_peer_id, &second_para_id).await.unwrap(), Score::new(4));
		assert_eq!(db.query(&second_peer_id, &second_para_id).await.unwrap(), Score::new(10));

		// Test a decay that makes the reputation go to 0 (The peer's entry will be removed)
		assert_eq!(
			db.process_bumps(
				17,
				[(second_para_id, [(second_peer_id, Score::new(10))].into_iter().collect()),]
					.into_iter()
					.collect(),
				Some(Score::new(5)),
				std::time::Duration::ZERO
			)
			.await,
			vec![
				ReputationUpdate {
					peer_id: second_peer_id,
					para_id: second_para_id,
					kind: ReputationUpdateKind::Bump,
					value: Score::new(10)
				},
				ReputationUpdate {
					peer_id: first_peer_id,
					para_id: second_para_id,
					kind: ReputationUpdateKind::Slash,
					value: Score::new(4)
				}
			]
		);
		assert_eq!(db.processed_finalized_block_number().await, Some(17));
		assert_eq!(db.len(), 2);
		assert_eq!(db.query(&first_peer_id, &first_para_id).await.unwrap(), Score::new(20));
		assert_eq!(db.query(&second_peer_id, &first_para_id).await.unwrap(), Score::new(9));
		assert_eq!(db.query(&first_peer_id, &second_para_id).await, None);
		assert_eq!(db.query(&second_peer_id, &second_para_id).await.unwrap(), Score::new(20));

		// Test an update which ends up pruning least recently used entries. The per-para limit is
		// 10.
		let mut db = Db::new(10).await;
		let peer_ids = (0..10).map(|_| PeerId::random()).collect::<Vec<_>>();

		// Add an equal reputation for all peers.
		assert_eq!(
			db.process_bumps(
				1,
				[(
					first_para_id,
					peer_ids.iter().map(|peer_id| (*peer_id, Score::new(10))).collect()
				)]
				.into_iter()
				.collect(),
				None,
				std::time::Duration::ZERO
			)
			.await
			.len(),
			10
		);
		assert_eq!(db.len(), 1);

		for peer_id in peer_ids.iter() {
			assert_eq!(db.query(peer_id, &first_para_id).await.unwrap(), Score::new(10));
		}

		// Now sleep for one second and then bump the reputations of all peers except for the one
		// with 4th index. We need to sleep so that the update time of the 4th peer is older than
		// the rest.
		tokio::time::sleep(Duration::from_millis(100)).await;
		assert_eq!(
			db.process_bumps(
				2,
				[(
					first_para_id,
					peer_ids
						.iter()
						.enumerate()
						.filter_map(|(i, peer_id)| (i != 4).then_some((*peer_id, Score::new(10))))
						.collect()
				)]
				.into_iter()
				.collect(),
				Some(Score::new(5)),
				std::time::Duration::ZERO
			)
			.await
			.len(),
			10
		);

		for (i, peer_id) in peer_ids.iter().enumerate() {
			if i == 4 {
				assert_eq!(db.query(peer_id, &first_para_id).await.unwrap(), Score::new(5));
			} else {
				assert_eq!(db.query(peer_id, &first_para_id).await.unwrap(), Score::new(20));
			}
		}

		// Now add a 11th peer. It should evict the 4th peer.
		let new_peer = PeerId::random();
		tokio::time::sleep(Duration::from_millis(100)).await;
		assert_eq!(
			db.process_bumps(
				3,
				[(first_para_id, [(new_peer, Score::new(10))].into_iter().collect())]
					.into_iter()
					.collect(),
				Some(Score::new(5)),
				std::time::Duration::ZERO
			)
			.await
			.len(),
			11
		);
		for (i, peer_id) in peer_ids.iter().enumerate() {
			if i == 4 {
				assert_eq!(db.query(peer_id, &first_para_id).await, None);
			} else {
				assert_eq!(db.query(peer_id, &first_para_id).await.unwrap(), Score::new(15));
			}
		}
		assert_eq!(db.query(&new_peer, &first_para_id).await.unwrap(), Score::new(10));

		// Now try adding yet another peer. The decay would naturally evict the new peer so no need
		// to evict the least recently bumped.
		let yet_another_peer = PeerId::random();
		assert_eq!(
			db.process_bumps(
				4,
				[(first_para_id, [(yet_another_peer, Score::new(10))].into_iter().collect())]
					.into_iter()
					.collect(),
				Some(Score::new(10)),
				std::time::Duration::ZERO
			)
			.await
			.len(),
			11
		);
		for (i, peer_id) in peer_ids.iter().enumerate() {
			if i == 4 {
				assert_eq!(db.query(peer_id, &first_para_id).await, None);
			} else {
				assert_eq!(db.query(peer_id, &first_para_id).await.unwrap(), Score::new(5));
			}
		}
		assert_eq!(db.query(&new_peer, &first_para_id).await, None);
		assert_eq!(db.query(&yet_another_peer, &first_para_id).await, Some(Score::new(10)));
	}

	#[tokio::test]
	// Test reputation slashes.
	async fn test_slash() {
		let mut db = Db::new(10).await;

		// Test slash on empty DB
		let peer_id = PeerId::random();
		db.slash(&peer_id, &ParaId::from(100), Score::new(50)).await;
		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await, None);

		// Test slash on non-existent para
		let another_peer_id = PeerId::random();
		assert_eq!(
			db.process_bumps(
				1,
				[
					(ParaId::from(100), [(peer_id, Score::new(10))].into_iter().collect()),
					(ParaId::from(200), [(another_peer_id, Score::new(12))].into_iter().collect()),
					(ParaId::from(300), [(peer_id, Score::new(15))].into_iter().collect())
				]
				.into_iter()
				.collect(),
				Some(Score::new(10)),
				std::time::Duration::ZERO
			)
			.await
			.len(),
			3
		);
		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&another_peer_id, &ParaId::from(200)).await.unwrap(), Score::new(12));
		assert_eq!(db.query(&peer_id, &ParaId::from(300)).await.unwrap(), Score::new(15));

		db.slash(&peer_id, &ParaId::from(200), Score::new(4)).await;
		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&another_peer_id, &ParaId::from(200)).await.unwrap(), Score::new(12));
		assert_eq!(db.query(&peer_id, &ParaId::from(300)).await.unwrap(), Score::new(15));

		// Test regular slash
		db.slash(&peer_id, &ParaId::from(100), Score::new(4)).await;
		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await.unwrap(), Score::new(6));

		// Test slash which removes the entry altogether
		db.slash(&peer_id, &ParaId::from(100), Score::new(8)).await;
		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await, None);
		assert_eq!(db.len(), 2);
	}

	#[tokio::test]
	// Test para pruning.
	async fn test_prune_paras() {
		let mut db = Db::new(10).await;

		db.prune_paras(BTreeSet::new()).await;
		assert_eq!(db.len(), 0);

		db.prune_paras([ParaId::from(100), ParaId::from(200)].into_iter().collect())
			.await;
		assert_eq!(db.len(), 0);

		let peer_id = PeerId::random();
		let another_peer_id = PeerId::random();

		assert_eq!(
			db.process_bumps(
				1,
				[
					(ParaId::from(100), [(peer_id, Score::new(10))].into_iter().collect()),
					(ParaId::from(200), [(another_peer_id, Score::new(12))].into_iter().collect()),
					(ParaId::from(300), [(peer_id, Score::new(15))].into_iter().collect())
				]
				.into_iter()
				.collect(),
				Some(Score::new(10)),
				std::time::Duration::ZERO
			)
			.await
			.len(),
			3
		);
		assert_eq!(db.len(), 3);

		// Registered paras include the existing ones. Does nothing
		db.prune_paras(
			[ParaId::from(100), ParaId::from(200), ParaId::from(300), ParaId::from(400)]
				.into_iter()
				.collect(),
		)
		.await;
		assert_eq!(db.len(), 3);

		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await.unwrap(), Score::new(10));
		assert_eq!(db.query(&another_peer_id, &ParaId::from(200)).await.unwrap(), Score::new(12));
		assert_eq!(db.query(&peer_id, &ParaId::from(300)).await.unwrap(), Score::new(15));

		// Prunes multiple paras.
		db.prune_paras([ParaId::from(300)].into_iter().collect()).await;
		assert_eq!(db.len(), 1);
		assert_eq!(db.query(&peer_id, &ParaId::from(100)).await, None);
		assert_eq!(db.query(&another_peer_id, &ParaId::from(200)).await, None);
		assert_eq!(db.query(&peer_id, &ParaId::from(300)).await.unwrap(), Score::new(15));

		// Prunes all paras.
		db.prune_paras(BTreeSet::new()).await;
		assert_eq!(db.len(), 0);
		assert_eq!(db.query(&peer_id, &ParaId::from(300)).await, None);
	}

	mod peer_pruning {
		use super::*;

		#[tokio::test]
		async fn max_score_oldest_first() {
			use crate::validator_side_experimental::common::MAX_SCORE;
			use std::time::{SystemTime, UNIX_EPOCH};

			let mut db = Db::new(2).await;
			let para_id = ParaId::from(100);
			let peer_old = PeerId::random();
			let peer_mid = PeerId::random();
			let peer_new = PeerId::random();

			let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();

			// Inject two existing peers with MAX_SCORE two days apart; peer_old has a lower
			// score/age ratio (older with equal score) and will be evicted.
			let mut reputations = HashMap::new();
			reputations.insert(
				peer_old,
				ScoreEntry {
					score: Score::new(MAX_SCORE),
					last_bumped: now.saturating_sub(172_800_000),
				},
			);
			reputations.insert(
				peer_mid,
				ScoreEntry {
					score: Score::new(MAX_SCORE),
					last_bumped: now.saturating_sub(86_400_000),
				},
			);
			db.set_para_reputations(para_id, reputations);

			// Adding peer_new pushes the para over the limit of 2, triggering one eviction.
			db.process_bumps(
				1,
				[(para_id, [(peer_new, Score::new(MAX_SCORE))].into_iter().collect())]
					.into_iter()
					.collect(),
				None,
				std::time::Duration::from_millis(now as u64),
			)
			.await;

			// The oldest peer (smallest last_bumped) should have been evicted.
			assert_eq!(db.query(&peer_old, &para_id).await, None, "oldest peer should be pruned");
			assert!(db.query(&peer_mid, &para_id).await.is_some(), "middle peer should remain");
			assert!(db.query(&peer_new, &para_id).await.is_some(), "newest peer should remain");
		}

		#[tokio::test]
		async fn lower_score_over_older_timestamp() {
			use crate::validator_side_experimental::common::MAX_SCORE;
			use std::time::{SystemTime, UNIX_EPOCH};

			let mut db = Db::new(2).await;
			let para_id = ParaId::from(200);
			let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
			let mut reputations = HashMap::new();

			// score=32767, age=100s -> ratio≈0.328
			let peer_high_score = PeerId::random();
			reputations.insert(
				peer_high_score,
				ScoreEntry {
					score: Score::new(MAX_SCORE / 2),
					last_bumped: now.saturating_sub(100_000),
				},
			);

			// score=100, age=1s -> ratio=0.1
			let peer_low_score = PeerId::random();
			reputations.insert(
				peer_low_score,
				ScoreEntry { score: Score::new(100), last_bumped: now.saturating_sub(1_000) },
			);
			db.set_para_reputations(para_id, reputations);

			// score=1, age=0 -> ratio=max_value
			let peer_trigger = PeerId::random();
			db.process_bumps(
				1,
				[(para_id, [(peer_trigger, Score::new(1))].into_iter().collect())]
					.into_iter()
					.collect(),
				None,
				std::time::Duration::from_millis(now as u64),
			)
			.await;

			assert!(db.query(&peer_high_score, &para_id).await.is_some(), "ratio≈0.328, survives");
			assert_eq!(db.query(&peer_low_score, &para_id).await, None, "ratio=0.1, evicted");
		}

		// A high-score peer remains in the DB even as low-score peers cycle through it.
		// When a fresh low-score peer is added and the limit is exceeded, the oldest
		// low-score peer (lowest score/age ratio) is evicted rather than the high-score one.
		#[tokio::test]
		async fn high_score_peer_protected_from_low_score_churn() {
			use std::time::{SystemTime, UNIX_EPOCH};

			let mut db = Db::new(4).await;
			let para_id = ParaId::from(100);
			let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
			let mut reputations = HashMap::new();

			let peer_high = PeerId::random(); // score=2000, age=5s -> ratio=0.4
			reputations.insert(
				peer_high,
				ScoreEntry { score: Score::new(2000), last_bumped: now.saturating_sub(5_000) },
			);

			// score=1, age=30s -> ratio=1/30_000
			let peer_old = PeerId::random();
			reputations.insert(
				peer_old,
				ScoreEntry { score: Score::new(1), last_bumped: now.saturating_sub(30_000) },
			);

			// score=1, age=20s -> ratio=1/20_000
			let peer_mid = PeerId::random();
			reputations.insert(
				peer_mid,
				ScoreEntry { score: Score::new(1), last_bumped: now.saturating_sub(20_000) },
			);

			// score=1, age=10s -> ratio=1/10_000
			let peer_recent = PeerId::random();
			reputations.insert(
				peer_recent,
				ScoreEntry { score: Score::new(1), last_bumped: now.saturating_sub(10_000) },
			);
			db.set_para_reputations(para_id, reputations);

			// peer_new pushes over the limit of 4, triggering one eviction
			// score=1, age=0 -> ratio=max_value
			let peer_new = PeerId::random();
			db.process_bumps(
				1,
				[(para_id, [(peer_new, Score::new(1))].into_iter().collect())]
					.into_iter()
					.collect(),
				None,
				std::time::Duration::from_millis(now as u64),
			)
			.await;

			assert_eq!(db.query(&peer_old, &para_id).await, None, "ratio=1/30_000, evicted");
			assert!(db.query(&peer_high, &para_id).await.is_some(), "ratio=0.4, survives");
			assert!(db.query(&peer_mid, &para_id).await.is_some());
			assert!(db.query(&peer_recent, &para_id).await.is_some());
			assert!(db.query(&peer_new, &para_id).await.is_some());
		}

		#[tokio::test]
		async fn multiple_evictions_correct_order() {
			use std::time::{SystemTime, UNIX_EPOCH};

			let mut db = Db::new(2).await; // 5 entries → 3 evictions needed
			let para_id = ParaId::from(100);
			let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
			let mut reputations = HashMap::new();

			// score=1, age=100s -> ratio=1/100_000
			let peer_a = PeerId::random();
			reputations.insert(
				peer_a,
				ScoreEntry { score: Score::new(1), last_bumped: now.saturating_sub(100_000) },
			);

			// score=1, age=50s -> ratio=1/50_000
			let peer_b = PeerId::random();
			reputations.insert(
				peer_b,
				ScoreEntry { score: Score::new(1), last_bumped: now.saturating_sub(50_000) },
			);

			// score=1, age=20s -> ratio=1/20_000
			let peer_c = PeerId::random();
			reputations.insert(
				peer_c,
				ScoreEntry { score: Score::new(1), last_bumped: now.saturating_sub(20_000) },
			);

			// score=2000, age=1000s -> ratio=0.002
			let peer_d = PeerId::random();
			reputations.insert(
				peer_d,
				ScoreEntry { score: Score::new(2000), last_bumped: now.saturating_sub(1_000_000) },
			);
			db.set_para_reputations(para_id, reputations);

			// score=1, age=0 -> ratio=max_value
			let peer_e = PeerId::random();
			db.process_bumps(
				// peer_e triggers 3 evictions (5 entries → limit 2)
				1,
				[(para_id, [(peer_e, Score::new(1))].into_iter().collect())]
					.into_iter()
					.collect(),
				None,
				std::time::Duration::from_millis(now as u64),
			)
			.await;

			assert_eq!(db.query(&peer_a, &para_id).await, None, "ratio=1/100_000, evicted");
			assert_eq!(db.query(&peer_b, &para_id).await, None, "ratio=1/50_000, evicted");
			assert_eq!(db.query(&peer_c, &para_id).await, None, "ratio=1/20_000, evicted");
			assert!(db.query(&peer_d, &para_id).await.is_some(), "ratio=0.002, survives");
			assert!(db.query(&peer_e, &para_id).await.is_some(), "ratio=max_value, survives");
		}

		#[tokio::test]
		async fn zero_score_works() {
			use std::time::{SystemTime, UNIX_EPOCH};

			let mut db = Db::new(2).await;
			let para_id = ParaId::from(100);
			let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
			let mut reputations = HashMap::new();

			// score=0, age=1s → checked_from_rational(0, age) = Some(0) → ratio=0
			let peer_zero = PeerId::random();
			reputations.insert(
				peer_zero,
				ScoreEntry { score: Score::new(0), last_bumped: now.saturating_sub(1_000) },
			);

			// score=100, age=10s → ratio=0.01
			let peer_normal = PeerId::random();
			reputations.insert(
				peer_normal,
				ScoreEntry { score: Score::new(100), last_bumped: now.saturating_sub(10_000) },
			);
			db.set_para_reputations(para_id, reputations);

			let peer_trigger = PeerId::random();
			db.process_bumps(
				1,
				[(para_id, [(peer_trigger, Score::new(1))].into_iter().collect())]
					.into_iter()
					.collect(),
				None,
				std::time::Duration::from_millis(now as u64),
			)
			.await;

			assert_eq!(db.query(&peer_zero, &para_id).await, None, "ratio=0, evicted");
			assert!(db.query(&peer_normal, &para_id).await.is_some(), "ratio=0.01, survives");
			assert!(db.query(&peer_trigger, &para_id).await.is_some(), "ratio=max_value, survives");
		}

		#[tokio::test]
		async fn equal_ratio_tiebreaker_evicts_lower_score() {
			use std::time::{SystemTime, UNIX_EPOCH};

			// Both entries have the same score/age ratio (2/200 = 3/300 = 0.01). The tiebreaker
			// `score` breaks the tie deterministically: lower score → evicted first.
			let mut db = Db::new(2).await;
			let para_id = ParaId::from(100);
			let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
			let mut reputations = HashMap::new();

			// score=2, age=200ms -> ratio=0.01, tiebreaker=2
			let peer_a = PeerId::random();
			reputations.insert(
				peer_a,
				ScoreEntry { score: Score::new(2), last_bumped: now.saturating_sub(200) },
			);

			// score=3, age=300ms -> ratio=0.01, tiebreaker=3
			let peer_b = PeerId::random();
			reputations.insert(
				peer_b,
				ScoreEntry { score: Score::new(3), last_bumped: now.saturating_sub(300) },
			);
			db.set_para_reputations(para_id, reputations);

			let peer_trigger = PeerId::random();
			db.process_bumps(
				1,
				[(para_id, [(peer_trigger, Score::new(1))].into_iter().collect())]
					.into_iter()
					.collect(),
				None,
				std::time::Duration::from_millis(now as u64),
			)
			.await;

			assert_eq!(db.query(&peer_a, &para_id).await, None, "lower score wins tie, evicted");
			assert!(db.query(&peer_b, &para_id).await.is_some(), "peer_b survives");
			assert!(db.query(&peer_trigger, &para_id).await.is_some(), "peer_trigger survives");
		}
	}
}
