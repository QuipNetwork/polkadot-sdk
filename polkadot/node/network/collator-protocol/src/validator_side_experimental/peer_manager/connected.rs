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

use crate::{
	validator_side_experimental::{
		common::{PeerInfo, PeerState, Score},
		peer_manager::{
			DeclarationOutcome, ReputationUpdate, ReputationUpdateKind, TryAcceptOutcome,
		},
	},
	validator_side_metrics::ScoreBand,
};
use polkadot_node_network_protocol::{peer_set::CollationVersion, PeerId};
use polkadot_primitives::Id as ParaId;
use std::{
	cmp::Ordering,
	collections::{BTreeMap, BTreeSet, HashMap},
	future::Future,
	num::NonZeroU16,
};

/// Keeps track of connected peers, together with relevant info such as their procotol versions,
/// declared paraids and reputations.
#[derive(Clone)]
pub struct ConnectedPeers {
	per_para: BTreeMap<ParaId, PerPara>,
	peer_info: HashMap<PeerId, PeerInfo>,
}

impl ConnectedPeers {
	/// Create a new ConnectedPeers object.
	pub fn new(
		scheduled_paras: BTreeSet<ParaId>,
		overall_limit: NonZeroU16,
		per_para_limit: NonZeroU16,
	) -> Self {
		debug_assert!(per_para_limit <= overall_limit);

		let mut per_para = BTreeMap::new();
		if scheduled_paras.len() > 0 {
			// Since `overall_limit` and `per_para_limit` are `u16`, `limit` must also fit in `u16`.
			let limit = std::cmp::min(
				overall_limit.get() as usize / scheduled_paras.len(),
				per_para_limit.get() as usize,
			) as u16;
			if let Some(non_zero_limit) = NonZeroU16::new(limit) {
				for para_id in scheduled_paras {
					per_para.insert(para_id, PerPara::new(non_zero_limit));
				}
			}
		}

		Self { per_para, peer_info: Default::default() }
	}

	/// Update the reputation of a peer for a specific paraid. This needs to also be persisted to
	/// the reputation database, but changes are duplicated to this in-memory store of connected
	/// peers.
	pub fn update_reputation(&mut self, update: ReputationUpdate) {
		let Some(per_para) = self.per_para.get_mut(&update.para_id) else { return };

		per_para.update_reputation(update);
	}

	/// Try accepting a new peer. The connection must have already been established, but here we
	/// decide whether to keep it or not. We don't know for which paraid the peer will collate.
	pub async fn try_accept<
		RepQueryFn: Fn(PeerId, ParaId) -> QueryFut,
		QueryFut: Future<Output = Score>,
	>(
		&mut self,
		reputation_query_fn: RepQueryFn,
		peer_id: PeerId,
		peer_info: PeerInfo,
	) -> TryAcceptOutcome {
		if self.contains(&peer_id) {
			return TryAcceptOutcome::Added;
		}

		let mut outcome = TryAcceptOutcome::Rejected;

		match peer_info.state {
			PeerState::Collating(para_id) => {
				if let Some(per_para) = self.per_para.get_mut(&para_id) {
					let past_reputation = reputation_query_fn(peer_id, para_id).await;
					let res = per_para.try_accept(peer_id, past_reputation);
					outcome = outcome.combine(res);
				}
			},
			PeerState::Connected => {
				for (para_id, per_para) in self.per_para.iter_mut() {
					let past_reputation = reputation_query_fn(peer_id, *para_id).await;
					let res = per_para.try_accept(peer_id, past_reputation);
					outcome = outcome.combine(res);
				}
			},
		}

		match outcome {
			TryAcceptOutcome::Replaced(mut replaced) => {
				self.peer_info.insert(peer_id, peer_info);

				// Even if this peer took the place of some other peers, these replaced peers may
				// still have connection slots for other paras. Only remove them if they don't.
				replaced.retain(|replaced_peer| {
					let disconnect =
						!self.per_para.values().any(|per_para| per_para.contains(&replaced_peer));

					if disconnect {
						self.peer_info.remove(replaced_peer);
					}

					disconnect
				});

				TryAcceptOutcome::Replaced(replaced)
			},
			TryAcceptOutcome::Added => {
				self.peer_info.insert(peer_id, peer_info);
				TryAcceptOutcome::Added
			},
			TryAcceptOutcome::Rejected => TryAcceptOutcome::Rejected,
		}
	}

	/// Remove the peer. Should be called when we see a peer being disconnected.
	pub fn remove(&mut self, peer: &PeerId) {
		for per_para in self.per_para.values_mut() {
			per_para.remove(peer);
		}

		self.peer_info.remove(peer);
	}

	/// Process a peer's declaration of intention to collate for this paraid.
	pub fn declared(&mut self, peer_id: PeerId, para_id: ParaId) -> DeclarationOutcome {
		let mut outcome = DeclarationOutcome::Rejected;

		let Some(peer_info) = self.peer_info.get_mut(&peer_id) else { return outcome };

		match &peer_info.state {
			PeerState::Connected => {
				for (para, per_para) in self.per_para.iter_mut() {
					if para == &para_id && per_para.contains(&peer_id) {
						outcome = DeclarationOutcome::Accepted;
					} else {
						// We remove the reserved slots from all other paras.
						per_para.remove(&peer_id);
					}
				}
			},
			PeerState::Collating(old_para_id) if old_para_id == &para_id => {
				// Redundant, already collating for this para.
				outcome = DeclarationOutcome::Accepted;
			},
			PeerState::Collating(old_para_id) => {
				if let Some(old_per_para) = self.per_para.get_mut(&old_para_id) {
					old_per_para.remove(&peer_id);
				}
				if self.per_para.contains_key(&para_id) {
					outcome = DeclarationOutcome::Switched(*old_para_id);
				}
			},
		}

		if matches!(outcome, DeclarationOutcome::Accepted) {
			peer_info.state = PeerState::Collating(para_id);
		} else {
			self.peer_info.remove(&peer_id);
		}

		outcome
	}

	/// Get a reference to the peer's info, if connected.
	pub fn peer_info(&self, peer_id: &PeerId) -> Option<&PeerInfo> {
		self.peer_info.get(&peer_id)
	}

	/// The number of *declared* collators per para, grouped by their reputation score band.
	///
	/// Undeclared (`PeerState::Connected`) peers reserve a slot in every assigned para until they
	/// declare, so skip them here to avoid multi-counting one peer across paras.
	pub fn score_distribution(&self) -> BTreeMap<ParaId, BTreeMap<ScoreBand, u64>> {
		let mut distribution: BTreeMap<ParaId, BTreeMap<ScoreBand, u64>> = BTreeMap::new();

		for (para_id, per_para) in &self.per_para {
			for (peer_id, score) in &per_para.per_peer_score {
				if matches!(
					self.peer_info.get(peer_id).map(|info| &info.state),
					Some(PeerState::Collating(declared)) if declared == para_id
				) {
					*distribution
						.entry(*para_id)
						.or_default()
						.entry(ScoreBand::classify(u16::from(*score)))
						.or_default() += 1;
				}
			}
		}

		distribution
	}

	pub fn peer_score(&self, peer_id: &PeerId, para_id: &ParaId) -> Option<Score> {
		self.per_para.get(para_id).and_then(|per_para| per_para.get_score(peer_id))
	}

	/// Consume self and return the relevant information for building the next instance.
	pub fn consume(self) -> (HashMap<PeerId, PeerInfo>, BTreeMap<ParaId, PerPara>) {
		(self.peer_info, self.per_para)
	}

	/// Return an iterator over the scheduled paraids.
	pub fn scheduled_paras<'a>(&'a self) -> impl Iterator<Item = &'a ParaId> + 'a {
		self.per_para.keys()
	}

	/// Returns the number of connected peers.
	pub fn len(&self) -> usize {
		self.peer_info.len()
	}

	fn contains(&self, peer_id: &PeerId) -> bool {
		self.peer_info.contains_key(peer_id)
	}

	/// Get the negotiated collation version.
	/// Returns None if the peer is not connected.
	pub fn get_version(&self, peer_id: &PeerId) -> Option<CollationVersion> {
		self.peer_info.get(peer_id).map(|peer_info| peer_info.version)
	}
}

/// Per-para connected peers store. Acts as a handy in-memory cache of connected peer scores for a
/// specific paraid.
#[derive(Clone)]
pub struct PerPara {
	// Don't accept more than this number of connected peers for this para.
	limit: NonZeroU16,
	// A min-heap would be more efficient for getting the min (constant) but modifying the score
	// would be linear, so use a BST which achieves logarithmic performance for all ops.
	sorted_scores: BTreeSet<PeerScoreEntry>,
	// This is needed so that we can quickly access the ordered entry. Also has the nice benefit of
	// being an in-memory copy of the reputation DB for the connected peers.
	per_peer_score: HashMap<PeerId, Score>,
}

impl PerPara {
	/// Get the peer's score, if any.
	pub fn get_score(&self, peer_id: &PeerId) -> Option<Score> {
		self.per_peer_score.get(peer_id).copied()
	}

	fn new(limit: NonZeroU16) -> Self {
		Self { limit, sorted_scores: BTreeSet::default(), per_peer_score: HashMap::default() }
	}

	fn try_accept(&mut self, peer_id: PeerId, score: Score) -> TryAcceptOutcome {
		// If we've got enough room, add it. Otherwise, see if it has a higher reputation than any
		// other connected peer.

		if self.sorted_scores.len() < (u16::from(self.limit) as usize) {
			self.sorted_scores.insert(PeerScoreEntry { peer_id, score });
			self.per_peer_score.insert(peer_id, score);

			TryAcceptOutcome::Added
		} else {
			let Some(min_score) = self.sorted_scores.first() else {
				// The limit must be 0, which is not possible given that limit is a NonZeroU16.
				return TryAcceptOutcome::Rejected;
			};

			if min_score.score >= score {
				TryAcceptOutcome::Rejected
			} else {
				let Some(replaced) = self.sorted_scores.pop_first() else {
					// Cannot really happen since we already know there's some entry with a lower
					// score than ours.
					return TryAcceptOutcome::Rejected;
				};
				self.per_peer_score.remove(&replaced.peer_id);

				self.sorted_scores.insert(PeerScoreEntry { peer_id, score });
				self.per_peer_score.insert(peer_id, score);
				TryAcceptOutcome::Replaced([replaced.peer_id].into_iter().collect())
			}
		}
	}

	fn update_reputation(&mut self, update: ReputationUpdate) {
		let Some(score) = self.per_peer_score.get_mut(&update.peer_id) else {
			// If the peer is not connected we don't care to update anything besides the DB.
			return;
		};

		self.sorted_scores
			.remove(&PeerScoreEntry { peer_id: update.peer_id, score: *score });

		match update.kind {
			ReputationUpdateKind::Bump => score.saturating_add(update.value.into()),
			ReputationUpdateKind::Slash => score.saturating_sub(update.value.into()),
		};

		self.sorted_scores
			.insert(PeerScoreEntry { peer_id: update.peer_id, score: *score });
	}

	fn remove(&mut self, peer_id: &PeerId) {
		let Some(score) = self.per_peer_score.remove(&peer_id) else { return };

		self.sorted_scores.remove(&PeerScoreEntry { peer_id: *peer_id, score });
	}

	fn contains(&self, peer_id: &PeerId) -> bool {
		self.per_peer_score.contains_key(peer_id)
	}
}

#[derive(PartialEq, Eq, Clone)]
struct PeerScoreEntry {
	peer_id: PeerId,
	score: Score,
}

impl Ord for PeerScoreEntry {
	fn cmp(&self, other: &Self) -> Ordering {
		match self.score.cmp(&other.score) {
			Ordering::Equal => self.peer_id.cmp(&other.peer_id),
			order => order,
		}
	}
}

impl PartialOrd for PeerScoreEntry {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use super::*;

	use polkadot_node_network_protocol::peer_set::CollationVersion;

	fn default_connected_state() -> PeerInfo {
		PeerInfo { version: CollationVersion::V2, state: PeerState::Connected }
	}

	// Test the ConnectedPeers constructor
	#[test]
	fn test_connected_peers_constructor() {
		// Test an empty instance.
		let connected = ConnectedPeers::new(
			BTreeSet::new(),
			NonZeroU16::new(1000).unwrap(),
			NonZeroU16::new(50).unwrap(),
		);
		assert!(connected.per_para.is_empty());
		assert!(connected.peer_info.is_empty());

		// Test that the constructor sets the per-para limit as the minimum between the
		// per_para_limit and the overall_limit divided by the number of scheduled paras.
		let connected = ConnectedPeers::new(
			(0..5).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(3).unwrap(),
		);
		assert_eq!(connected.per_para.len(), 5);
		assert!(connected.peer_info.is_empty());
		for (para_id, per_para) in connected.per_para {
			let para_id = u32::from(para_id);
			assert!(para_id < 5);
			assert_eq!(per_para.limit.get(), 3);
		}

		let connected = ConnectedPeers::new(
			(0..5).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(15).unwrap(),
		);
		assert_eq!(connected.per_para.len(), 5);
		assert!(connected.peer_info.is_empty());
		for (para_id, per_para) in connected.per_para {
			let para_id = u32::from(para_id);
			assert!(para_id < 5);
			assert_eq!(per_para.limit.get(), 10);
		}
	}

	#[tokio::test]
	// Test peer connection acceptance criteria while the peer limit is not reached.
	async fn test_try_accept_below_limit() {
		let mut connected = ConnectedPeers::new(
			(0..5).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(15).unwrap(),
		);
		let first_peer = PeerId::random();

		// Try accepting a new peer which has no past reputation and has not declared.
		assert_eq!(
			connected
				.try_accept(
					|_, _| async { Score::default() },
					first_peer,
					default_connected_state()
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		for per_para in connected.per_para.values() {
			assert!(per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::default());
		}

		// Try adding an already accepted peer.
		assert_eq!(
			connected
				.try_accept(
					|_, _| async { Score::default() },
					first_peer,
					default_connected_state()
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		for per_para in connected.per_para.values() {
			assert!(per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::default());
		}

		// Try accepting a peer which has past reputation for an unscheduled para.
		let second_peer = PeerId::random();
		assert_eq!(
			connected
				.try_accept(
					|peer_id, para_id| async move {
						if peer_id == second_peer && para_id == ParaId::from(100) {
							Score::new(10)
						} else {
							Score::default()
						}
					},
					second_peer,
					default_connected_state()
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		assert_eq!(connected.peer_info(&second_peer).unwrap(), &default_connected_state());

		for per_para in connected.per_para.values() {
			assert!(per_para.contains(&second_peer));
			assert_eq!(per_para.get_score(&second_peer).unwrap(), Score::default());
		}

		// Try accepting a peer which has past reputation for a scheduled para but is not yet
		// declared.
		let third_peer = PeerId::random();
		let third_peer_para_id = ParaId::from(3);
		assert_eq!(
			connected
				.try_accept(
					|peer_id, para_id| async move {
						if peer_id == third_peer && para_id == third_peer_para_id {
							Score::new(10)
						} else {
							Score::default()
						}
					},
					third_peer,
					default_connected_state()
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		assert_eq!(connected.peer_info(&second_peer).unwrap(), &default_connected_state());
		assert_eq!(connected.peer_info(&third_peer).unwrap(), &default_connected_state());

		for (para_id, per_para) in connected.per_para.iter() {
			assert!(per_para.contains(&third_peer));

			if para_id == &third_peer_para_id {
				assert_eq!(per_para.get_score(&third_peer).unwrap(), Score::new(10));
			} else {
				assert_eq!(per_para.get_score(&third_peer).unwrap(), Score::default());
			}
		}

		// Try accepting a peer which is declared for an unscheduled para. It will be rejected,
		// regardless of its other reputations.
		let rejected_peer = PeerId::random();
		assert_eq!(
			connected
				.try_accept(
					|peer_id, _para_id| async move {
						if peer_id == rejected_peer {
							Score::new(10)
						} else {
							Score::default()
						}
					},
					rejected_peer,
					PeerInfo {
						version: CollationVersion::V2,
						state: PeerState::Collating(ParaId::from(100))
					}
				)
				.await,
			TryAcceptOutcome::Rejected
		);
		assert_eq!(connected.peer_info(&rejected_peer), None);
		for per_para in connected.per_para.values() {
			assert!(!per_para.contains(&rejected_peer));
			assert_eq!(per_para.get_score(&rejected_peer), None);
		}

		// Try accepting a peer which is declared for a scheduled para.
		let fourth_peer = PeerId::random();
		let fourth_peer_para_id = ParaId::from(4);

		assert_eq!(
			connected
				.try_accept(
					|peer_id, para_id| async move {
						if peer_id == fourth_peer && para_id == fourth_peer_para_id {
							Score::new(10)
						} else {
							Score::default()
						}
					},
					fourth_peer,
					PeerInfo {
						version: CollationVersion::V2,
						state: PeerState::Collating(fourth_peer_para_id)
					}
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		assert_eq!(connected.peer_info(&second_peer).unwrap(), &default_connected_state());
		assert_eq!(connected.peer_info(&third_peer).unwrap(), &default_connected_state());
		assert_eq!(
			connected.peer_info(&fourth_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(fourth_peer_para_id)
			}
		);

		for (para_id, per_para) in connected.per_para.iter() {
			if para_id == &fourth_peer_para_id {
				assert!(per_para.contains(&fourth_peer));
				assert_eq!(per_para.get_score(&fourth_peer).unwrap(), Score::new(10));
			} else {
				assert!(!per_para.contains(&fourth_peer));
				assert_eq!(per_para.get_score(&fourth_peer), None);
			}
		}
	}

	#[tokio::test]
	// Test peer connection acceptance criteria while the peer limit is reached.
	async fn test_try_accept_at_limit() {
		// We have 2 scheduled paras. The per para limit is 2.
		let mut connected = ConnectedPeers::new(
			(1..=2).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(2).unwrap(),
		);
		let first_peer = PeerId::random();
		let second_peer = PeerId::random();
		let third_peer = PeerId::random();
		let para_1 = ParaId::from(1);
		let para_2 = ParaId::from(2);

		let new_peer = PeerId::random();

		// Para 1 has: first_peer (not declared), reputation 10.
		// Para 1 has: second_peer (declared), reputation 20.

		// Para 2 has: first_peer (not declared), reputation 10.
		// Para 2 has: third_peer (declared), reputation 20.

		let rep_query_fn = |peer_id, para_id| async move {
			match (peer_id, para_id) {
				(peer_id, _para_id) if peer_id == first_peer => Score::new(10),
				(peer_id, para_id) if peer_id == second_peer && para_id == para_1 => Score::new(20),
				(peer_id, para_id) if peer_id == third_peer && para_id == para_2 => Score::new(20),
				(peer_id, para_id) if peer_id == new_peer && para_id == para_1 => Score::new(5),

				(_, _) => Score::default(),
			}
		};

		assert_eq!(
			connected.try_accept(rep_query_fn, first_peer, default_connected_state()).await,
			TryAcceptOutcome::Added
		);
		assert_eq!(
			connected
				.try_accept(
					rep_query_fn,
					second_peer,
					PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_1) }
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(
			connected
				.try_accept(
					rep_query_fn,
					third_peer,
					PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_2) }
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		assert_eq!(
			connected.peer_info(&second_peer).unwrap(),
			&PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_1) }
		);
		assert_eq!(
			connected.peer_info(&third_peer).unwrap(),
			&PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_2) }
		);

		// Let's assert the current state of the ConnectedPeers.

		assert_eq!(connected.per_para.len(), 2);
		let per_para_1 = connected.per_para.get(&para_1).unwrap();
		assert_eq!(per_para_1.per_peer_score.len(), 2);
		assert_eq!(per_para_1.sorted_scores.len(), 2);

		assert_eq!(connected.peer_score(&first_peer, &para_1).unwrap(), Score::new(10));
		assert_eq!(connected.peer_score(&second_peer, &para_1).unwrap(), Score::new(20));
		assert_eq!(connected.peer_score(&first_peer, &para_2).unwrap(), Score::new(10));
		assert_eq!(connected.peer_score(&third_peer, &para_2).unwrap(), Score::new(20));
		assert_eq!(connected.peer_score(&second_peer, &para_2), None);
		assert_eq!(connected.peer_score(&new_peer, &para_1), None);
		assert_eq!(connected.peer_score(&new_peer, &para_2), None);

		// Trying accepting a peer (declared or not) when all other peers have higher reputations ->
		// Rejection.
		assert_eq!(
			connected.try_accept(rep_query_fn, new_peer, default_connected_state()).await,
			TryAcceptOutcome::Rejected
		);
		assert_eq!(
			connected
				.try_accept(
					rep_query_fn,
					new_peer,
					PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_1) }
				)
				.await,
			TryAcceptOutcome::Rejected
		);
		assert_eq!(
			connected
				.try_accept(
					rep_query_fn,
					new_peer,
					PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_2) }
				)
				.await,
			TryAcceptOutcome::Rejected
		);
		assert_eq!(
			connected
				.try_accept(
					rep_query_fn,
					new_peer,
					PeerInfo {
						version: CollationVersion::V2,
						state: PeerState::Collating(ParaId::from(100))
					}
				)
				.await,
			TryAcceptOutcome::Rejected
		);

		// Trying to accept an undeclared peer when all other peers have lower reputations ->
		// Replace the ones with the lowest rep. Only kick out one if the ones with the lowest rep
		// are the same for all paras.
		{
			let mut connected = connected.clone();
			let rep_query_fn = |peer_id, _para_id| async move {
				if peer_id == new_peer {
					Score::new(30)
				} else {
					Score::default()
				}
			};
			assert_eq!(
				connected.try_accept(rep_query_fn, new_peer, default_connected_state()).await,
				TryAcceptOutcome::Replaced([first_peer].into_iter().collect())
			);
			assert_eq!(connected.peer_info(&new_peer).unwrap(), &default_connected_state());
			assert_eq!(connected.peer_info(&first_peer), None);

			assert_eq!(connected.peer_score(&first_peer, &para_1), None);
			assert_eq!(connected.peer_score(&second_peer, &para_1).unwrap(), Score::new(20));
			assert_eq!(connected.peer_score(&first_peer, &para_2), None);
			assert_eq!(connected.peer_score(&third_peer, &para_2).unwrap(), Score::new(20));
			assert_eq!(connected.peer_score(&third_peer, &para_1), None);
			assert_eq!(connected.peer_score(&second_peer, &para_2), None);
			assert_eq!(connected.peer_score(&new_peer, &para_1).unwrap(), Score::new(30));
			assert_eq!(connected.peer_score(&new_peer, &para_2).unwrap(), Score::new(30));
		}

		// Trying to accept an undeclared peer when all other peers have lower reputations ->
		// Replace the ones with the lowest rep.
		{
			let mut connected = ConnectedPeers::new(
				(1..=2).map(ParaId::from).collect(),
				NonZeroU16::new(50).unwrap(),
				NonZeroU16::new(2).unwrap(),
			);
			let fourth_peer = PeerId::random();

			let rep_query_fn = |peer_id, para_id| async move {
				match (peer_id, para_id) {
					(peer_id, _para_id) if peer_id == first_peer => Score::new(10),
					(peer_id, para_id) if peer_id == second_peer && para_id == para_1 => {
						Score::new(20)
					},
					(peer_id, para_id) if peer_id == third_peer && para_id == para_2 => {
						Score::new(20)
					},
					(peer_id, para_id) if peer_id == fourth_peer && para_id == para_2 => {
						Score::new(15)
					},
					(peer_id, _para_id) if peer_id == new_peer => Score::new(30),

					(_, _) => Score::default(),
				}
			};

			assert_eq!(
				connected.try_accept(rep_query_fn, first_peer, default_connected_state()).await,
				TryAcceptOutcome::Added
			);
			assert_eq!(
				connected
					.try_accept(
						rep_query_fn,
						second_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(para_1)
						}
					)
					.await,
				TryAcceptOutcome::Added
			);
			assert_eq!(
				connected
					.try_accept(
						rep_query_fn,
						third_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(para_2)
						}
					)
					.await,
				TryAcceptOutcome::Added
			);
			assert_eq!(
				connected
					.try_accept(
						rep_query_fn,
						fourth_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(para_2)
						}
					)
					.await,
				TryAcceptOutcome::Replaced(HashSet::new())
			);

			assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());

			assert_eq!(
				connected.try_accept(rep_query_fn, new_peer, default_connected_state()).await,
				TryAcceptOutcome::Replaced([first_peer, fourth_peer].into_iter().collect())
			);
			assert_eq!(connected.peer_info(&first_peer), None);
			assert_eq!(connected.peer_info(&fourth_peer), None);

			assert_eq!(connected.peer_info(&new_peer).unwrap(), &default_connected_state());

			assert_eq!(connected.peer_score(&first_peer, &para_1), None);
			assert_eq!(connected.peer_score(&second_peer, &para_1).unwrap(), Score::new(20));
			assert_eq!(connected.peer_score(&third_peer, &para_1), None);
			assert_eq!(connected.peer_score(&fourth_peer, &para_1), None);
			assert_eq!(connected.peer_score(&new_peer, &para_1).unwrap(), Score::new(30));

			assert_eq!(connected.peer_score(&first_peer, &para_2), None);
			assert_eq!(connected.peer_score(&second_peer, &para_2), None);
			assert_eq!(connected.peer_score(&third_peer, &para_2).unwrap(), Score::new(20));
			assert_eq!(connected.peer_score(&fourth_peer, &para_2), None);
			assert_eq!(connected.peer_score(&new_peer, &para_2).unwrap(), Score::new(30));
		}

		// Trying to accept a declared peer when all other peers have lower reputations ->
		// Replace the one with the lowest rep.
		// Because new_peer is already declared for para_1, it will only kick out first_peer's slot
		// on para_1.
		{
			let mut connected = connected.clone();
			let rep_query_fn = |peer_id, _para_id| async move {
				if peer_id == new_peer {
					Score::new(30)
				} else {
					Score::default()
				}
			};
			assert_eq!(
				connected
					.try_accept(
						rep_query_fn,
						new_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(para_1)
						}
					)
					.await,
				TryAcceptOutcome::Replaced(HashSet::new())
			);
			assert_eq!(
				connected.peer_info(&new_peer).unwrap(),
				&PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_1) }
			);
			assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
			assert_eq!(connected.peer_score(&first_peer, &para_1), None);
			assert_eq!(connected.peer_score(&second_peer, &para_1).unwrap(), Score::new(20));
			assert_eq!(connected.peer_score(&first_peer, &para_2).unwrap(), Score::new(10));
			assert_eq!(connected.peer_score(&third_peer, &para_2).unwrap(), Score::new(20));
			assert_eq!(connected.peer_score(&second_peer, &para_2), None);
			assert_eq!(connected.peer_score(&new_peer, &para_1).unwrap(), Score::new(30));
			assert_eq!(connected.peer_score(&new_peer, &para_2), None);
		}

		// Trying to accept a declared/undeclared peer when only one peer has lower reputation ->
		// Replace the one with the lowest rep.
		for peer_info in [
			default_connected_state(),
			PeerInfo { version: CollationVersion::V2, state: PeerState::Collating(para_1) },
		] {
			let mut connected = ConnectedPeers::new(
				(1..=2).map(ParaId::from).collect(),
				NonZeroU16::new(50).unwrap(),
				NonZeroU16::new(2).unwrap(),
			);

			let rep_query_fn = |peer_id, para_id| async move {
				match (peer_id, para_id) {
					(peer_id, _para_id) if peer_id == first_peer => Score::new(10),
					(peer_id, para_id) if peer_id == second_peer && para_id == para_1 => {
						Score::new(5)
					},
					(peer_id, para_id) if peer_id == third_peer && para_id == para_2 => {
						Score::new(5)
					},
					(peer_id, para_id) if peer_id == new_peer && para_id == para_1 => Score::new(8),

					(_, _) => Score::default(),
				}
			};
			assert_eq!(
				connected.try_accept(rep_query_fn, first_peer, default_connected_state()).await,
				TryAcceptOutcome::Added
			);
			assert_eq!(
				connected
					.try_accept(
						rep_query_fn,
						second_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(para_1)
						}
					)
					.await,
				TryAcceptOutcome::Added
			);
			assert_eq!(
				connected
					.try_accept(
						rep_query_fn,
						third_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(para_2)
						}
					)
					.await,
				TryAcceptOutcome::Added
			);

			assert_eq!(
				connected.try_accept(rep_query_fn, new_peer, peer_info.clone()).await,
				TryAcceptOutcome::Replaced([second_peer].into_iter().collect())
			);
			assert_eq!(connected.peer_info(&new_peer).unwrap(), &peer_info);

			assert_eq!(connected.peer_score(&first_peer, &para_1).unwrap(), Score::new(10));
			assert_eq!(connected.peer_score(&second_peer, &para_1), None);
			assert_eq!(connected.peer_score(&first_peer, &para_2).unwrap(), Score::new(10));
			assert_eq!(connected.peer_score(&third_peer, &para_2).unwrap(), Score::new(5));
			assert_eq!(connected.peer_score(&second_peer, &para_2), None);
			assert_eq!(connected.peer_score(&new_peer, &para_1).unwrap(), Score::new(8));
			assert_eq!(connected.peer_score(&new_peer, &para_2), None);
		}
	}

	#[tokio::test]
	// Test the handling of a Declare message in different scenarios.
	async fn test_declare() {
		let mut connected = ConnectedPeers::new(
			(0..5).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(15).unwrap(),
		);
		let first_peer = PeerId::random();

		assert_eq!(connected.peer_info(&first_peer), None);

		// Try handling a Declare statement from a non-existant peer. Should be a no-op
		assert_eq!(connected.declared(first_peer, ParaId::from(1)), DeclarationOutcome::Rejected);

		assert_eq!(connected.peer_info(&first_peer), None);

		assert_eq!(
			connected
				.try_accept(
					|_, _| async { Score::default() },
					first_peer,
					default_connected_state()
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		for per_para in connected.per_para.values() {
			assert!(per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::default());
		}

		// Declare coming for a para that is not scheduled.
		{
			let mut connected = connected.clone();
			assert_eq!(
				connected.declared(first_peer, ParaId::from(100)),
				DeclarationOutcome::Rejected
			);

			assert_eq!(connected.peer_info(&first_peer), None);

			for per_para in connected.per_para.values() {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}

		// Declare coming for a peer that is in undeclared state on multiple paras.
		assert_eq!(connected.declared(first_peer, ParaId::from(1)), DeclarationOutcome::Accepted);

		assert_eq!(
			connected.peer_info(&first_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(ParaId::from(1))
			}
		);

		for (para_id, per_para) in connected.per_para.iter() {
			if para_id == &ParaId::from(1) {
				assert!(per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::default());
			} else {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}

		// Test a redundant declare message, for the same para.
		assert_eq!(connected.declared(first_peer, ParaId::from(1)), DeclarationOutcome::Accepted);
		assert_eq!(
			connected.peer_info(&first_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(ParaId::from(1))
			}
		);

		// Peer already declared. New declare for a different, unscheduled para.
		{
			let mut connected = connected.clone();
			assert_eq!(
				connected.declared(first_peer, ParaId::from(100)),
				DeclarationOutcome::Rejected
			);
			assert_eq!(connected.peer_info(&first_peer), None);

			for per_para in connected.per_para.values() {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}

		// Peer already declared. New declare for a different para. The paraid switch is just like a
		// rejection, the peer manager then needs to retry accepting the connection on the new para.
		assert_eq!(
			connected.peer_info(&first_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(ParaId::from(1))
			}
		);
		assert_eq!(
			connected.declared(first_peer, ParaId::from(2)),
			DeclarationOutcome::Switched(ParaId::from(1))
		);
		assert_eq!(connected.peer_info(&first_peer), None);

		for per_para in connected.per_para.values() {
			assert!(!per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer), None);
		}
	}

	#[tokio::test]
	// Test the removal of disconnected peers.
	async fn test_remove() {
		let mut connected = ConnectedPeers::new(
			(0..5).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(15).unwrap(),
		);
		let first_peer = PeerId::random();

		assert_eq!(connected.peer_info(&first_peer), None);

		// Try removing a non-existant peer. Should be a no-op
		connected.remove(&first_peer);

		assert_eq!(connected.peer_info(&first_peer), None);

		for per_para in connected.per_para.values() {
			assert!(!per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer), None);
		}

		// Add a peer in undeclared state and remove it. It will be removed from all paras.
		{
			assert_eq!(
				connected
					.try_accept(
						|_, _| async { Score::default() },
						first_peer,
						default_connected_state()
					)
					.await,
				TryAcceptOutcome::Added
			);
			assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
			for per_para in connected.per_para.values() {
				assert!(per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::default());
			}

			connected.remove(&first_peer);

			assert_eq!(connected.peer_info(&first_peer), None);

			for per_para in connected.per_para.values() {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}

		// Add a peer in declared state and remove it. It will be from the declared para.
		{
			assert_eq!(
				connected
					.try_accept(
						|_, _| async { Score::default() },
						first_peer,
						PeerInfo {
							version: CollationVersion::V2,
							state: PeerState::Collating(ParaId::from(1))
						}
					)
					.await,
				TryAcceptOutcome::Added
			);
			assert_eq!(
				connected.peer_info(&first_peer).unwrap(),
				&PeerInfo {
					version: CollationVersion::V2,
					state: PeerState::Collating(ParaId::from(1))
				}
			);
			for (para_id, per_para) in connected.per_para.iter() {
				if para_id == &ParaId::from(1) {
					assert!(per_para.contains(&first_peer));
					assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::default());
				} else {
					assert!(!per_para.contains(&first_peer));
					assert_eq!(per_para.get_score(&first_peer), None);
				}
			}

			connected.remove(&first_peer);

			assert_eq!(connected.peer_info(&first_peer), None);

			for per_para in connected.per_para.values() {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}
	}

	#[tokio::test]
	// Test different scenarios for reputation updates.
	async fn test_update_reputation() {
		let mut connected = ConnectedPeers::new(
			(0..6).map(ParaId::from).collect(),
			NonZeroU16::new(50).unwrap(),
			NonZeroU16::new(15).unwrap(),
		);
		let first_peer = PeerId::random();

		assert_eq!(connected.peer_info(&first_peer), None);
		for per_para in connected.per_para.values() {
			assert!(!per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer), None);
		}

		// Update for a non-existant peer. No-op.
		connected.update_reputation(ReputationUpdate {
			peer_id: first_peer,
			para_id: ParaId::from(1),
			value: Score::new(100),
			kind: ReputationUpdateKind::Slash,
		});

		assert_eq!(connected.peer_info(&first_peer), None);
		for per_para in connected.per_para.values() {
			assert!(!per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer), None);
		}

		// Peer exists, but this para is not scheduled.
		assert_eq!(
			connected
				.try_accept(
					|peer_id, _| async move {
						if peer_id == first_peer {
							Score::new(10)
						} else {
							Score::default()
						}
					},
					first_peer,
					default_connected_state()
				)
				.await,
			TryAcceptOutcome::Added
		);
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		for per_para in connected.per_para.values() {
			assert!(per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::new(10));
		}

		connected.update_reputation(ReputationUpdate {
			peer_id: first_peer,
			para_id: ParaId::from(100),
			value: Score::new(100),
			kind: ReputationUpdateKind::Slash,
		});
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		for per_para in connected.per_para.values() {
			assert!(per_para.contains(&first_peer));
			assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::new(10));
		}

		// Test a slash for only one para, even though peer has reputation for all.
		connected.update_reputation(ReputationUpdate {
			peer_id: first_peer,
			para_id: ParaId::from(1),
			value: Score::new(100),
			kind: ReputationUpdateKind::Slash,
		});
		assert_eq!(connected.peer_info(&first_peer).unwrap(), &default_connected_state());
		for (para_id, per_para) in connected.per_para.iter() {
			assert!(per_para.contains(&first_peer));

			if para_id == &ParaId::from(1) {
				assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::new(0));
			} else {
				assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::new(10));
			}
		}

		// Test a bump after the peer declared for one para. First test a bump for the wrong para.
		// Then a bump for the declared para.
		assert_eq!(connected.declared(first_peer, ParaId::from(5)), DeclarationOutcome::Accepted);
		assert_eq!(
			connected.peer_info(&first_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(ParaId::from(5))
			}
		);

		connected.update_reputation(ReputationUpdate {
			peer_id: first_peer,
			para_id: ParaId::from(1),
			value: Score::new(100),
			kind: ReputationUpdateKind::Bump,
		});
		assert_eq!(
			connected.peer_info(&first_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(ParaId::from(5))
			}
		);

		for (para_id, per_para) in connected.per_para.iter() {
			if para_id == &ParaId::from(5) {
				assert!(per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::new(10));
			} else {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}

		connected.update_reputation(ReputationUpdate {
			peer_id: first_peer,
			para_id: ParaId::from(5),
			value: Score::new(50),
			kind: ReputationUpdateKind::Bump,
		});
		assert_eq!(
			connected.peer_info(&first_peer).unwrap(),
			&PeerInfo {
				version: CollationVersion::V2,
				state: PeerState::Collating(ParaId::from(5))
			}
		);

		for (para_id, per_para) in connected.per_para.iter() {
			if para_id == &ParaId::from(5) {
				assert!(per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer).unwrap(), Score::new(60));
			} else {
				assert!(!per_para.contains(&first_peer));
				assert_eq!(per_para.get_score(&first_peer), None);
			}
		}
	}
}
