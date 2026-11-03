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

use crate::validator_side_experimental::{common::Score, peer_manager::ReputationUpdate};
use async_trait::async_trait;
use polkadot_node_network_protocol::PeerId;
use polkadot_primitives::{BlockNumber, Id as ParaId};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Trait describing the interface of the reputation database.
#[async_trait]
pub trait Backend {
	/// Return the latest finalized block for which the backend processed bumps.
	async fn processed_finalized_block_number(&self) -> Option<BlockNumber>;
	/// Get the peer's stored reputation for this paraid, if any.
	async fn query(&self, peer_id: &PeerId, para_id: &ParaId) -> Option<Score>;
	/// Slash the peer's reputation for this paraid, with the given value.
	async fn slash(&mut self, peer_id: &PeerId, para_id: &ParaId, value: Score);
	/// Prune all data for paraids that are no longer in this registered set.
	async fn prune_paras(&mut self, registered_paras: BTreeSet<ParaId>);
	/// Process the reputation bumps, returning all the reputation changes that were done in
	/// consequence. This is needed because a reputation bump for a para also means a reputation
	/// decay for the other collators of that para (if the `decay_value` param is present) and
	/// because if the number of stored reputations go over the `stored_limit_per_para`, we'll 100%
	/// slash the least recently bumped peers. `leaf_number` needs to be at least equal to the
	/// `processed_finalized_block_number`. `now` is the current wall-clock duration since the
	/// UNIX epoch used to stamp the bumped entries; passed in so tests can supply a
	/// deterministic clock.
	async fn process_bumps(
		&mut self,
		leaf_number: BlockNumber,
		bumps: BTreeMap<ParaId, HashMap<PeerId, Score>>,
		decay_value: Option<Score>,
		now: std::time::Duration,
	) -> Vec<ReputationUpdate>;
	/// Get the maximum scores for the given paraids.
	async fn max_scores_for_paras(&self, paras: BTreeSet<ParaId>) -> HashMap<ParaId, Score>;
}
