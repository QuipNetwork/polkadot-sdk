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

//! The Collator Protocol allows collators and validators talk to each other.
//! This subsystem implements both sides of the collator protocol.
//!
//! # Public API discipline
//!
//! The deterministic test-sim framework
//! (`polkadot-collator-protocol-test-sim`) is a **separate crate** and uses only this crate's
//! public API. This is intentional. **Do not** add `pub` items to expose internals just to
//! make a scenario compile.
//!
//! When a scenario needs a private type the answer is one of:
//!
//! - Add a variant to the contract enums in `test-sim/src/contract/{effect,query}.rs`. The scenario
//!   then asserts/queries the new observable rather than reaching into state.
//! - Add a public builder method on the test-sim builders (`World`, `Peer`, `Candidate`).
//! - Refactor the scenario to drive the property via a stimulus that makes it observable.
//!
//! Why: tests that depend on internals calcify implementation choices, mask production bugs
//! when private state diverges from observable behaviour, and break on refactors that did not
//! actually change behaviour. The whole point of the framework is that tests survive internal
//! refactors. Convenience exports erode that asset; the doc rule + reviewer attention on new
//! `pub` items at the lib.rs boundary is the enforcement mechanism. PR reviewers: any new
//! `pub` item in this file gets a justification or a "no".

#![deny(missing_docs)]
#![deny(unused_crate_dependencies)]
#![deny(clippy::disallowed_methods)]
// Tests still call `Instant::now`, `Delay::new`, etc directly. The deterministic-clock plumbing
// is enforced for production code; legacy tests retain their original behavior.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
#![recursion_limit = "256"]

// Acknowledge dev-deps used only by integration tests under `tests/`. The
// `unused_crate_dependencies` lint checks each Cargo target against the whole dependency set,
// so the lib target flags deps that only the `tests/` target uses.
// See https://github.com/rust-lang/rust/issues/95513.
#[cfg(test)]
use {
	polkadot_collator_protocol_test_sim_macros as _, polkadot_node_core_backing as _,
	polkadot_node_core_prospective_parachains as _, polkadot_overseer as _,
	polkadot_subsystem_test_sim as _, sp_consensus_slots as _,
};

use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	sync::Arc,
	time::{Duration, Instant},
};

use futures::{
	channel::oneshot,
	stream::{FusedStream, StreamExt},
	FutureExt, TryFutureExt,
};

use polkadot_node_subsystem::CollatorProtocolSenderTrait;
use polkadot_node_subsystem_util::{
	database::Database,
	reputation::ReputationAggregator,
	request_claim_queue,
	runtime::{self, fetch_scheduling_lookahead, recv_runtime},
};
use sp_consensus_babe::digests::CompatibleDigestItem;
use sp_core::H256;
use sp_keystore::KeystorePtr;

use polkadot_node_network_protocol::{
	request_response::{v2 as protocol_v2, v3 as protocol_v3, IncomingRequestReceiver},
	PeerId, UnifiedReputationChange as Rep,
};
use polkadot_node_subsystem::{
	errors::SubsystemError, messages::ChainApiMessage, overseer, DummySubsystem, SpawnedSubsystem,
};
use polkadot_primitives::{
	CollatorPair, CoreIndex, Hash, Id as ParaId, SessionIndex, RELAY_CHAIN_SLOT_DURATION_MILLIS,
};
use sp_consensus_slots::SlotDuration;
pub use validator_side_experimental::ReputationConfig;

use polkadot_node_clock::Clock;

mod collator_side;
mod validator_side;
mod validator_side_experimental;

// TODO: move into validator_side_experimental once `validator_side` is retired
mod validator_side_metrics;

const LOG_TARGET: &'static str = "parachain::collator-protocol";
const LOG_TARGET_STATS: &'static str = "parachain::collator-protocol::stats";

/// A collator eviction policy - how fast to evict collators which are inactive.
#[derive(Debug, Clone, Copy)]
pub struct CollatorEvictionPolicy {
	/// How fast to evict collators who are inactive.
	pub inactive_collator: Duration,
	/// How fast to evict peers which don't declare their para.
	pub undeclared: Duration,
}

impl Default for CollatorEvictionPolicy {
	fn default() -> Self {
		CollatorEvictionPolicy {
			inactive_collator: Duration::from_secs(24),
			undeclared: Duration::from_secs(1),
		}
	}
}

/// What side of the collator protocol is being engaged
pub enum ProtocolSide {
	/// Validators operate on the relay chain.
	Validator {
		/// The keystore holding validator keys.
		keystore: KeystorePtr,
		/// An eviction policy for inactive peers or validators.
		eviction_policy: CollatorEvictionPolicy,
		/// Prometheus metrics for validators.
		metrics: validator_side::Metrics,
		/// List of invulnerable collators which is handled with a priority.
		invulnerables: HashSet<PeerId>,
		/// Override for `HOLD_OFF_DURATION` constant .
		collator_protocol_hold_off: Option<Duration>,
		/// Clock used for all time reads. Production passes [`polkadot_node_clock::SystemClock`];
		/// tests inject a mock.
		clock: Arc<dyn Clock>,
	},
	/// Experimental variant of the validator side. Do not use in production.
	ValidatorExperimental {
		/// The keystore holding validator keys.
		keystore: KeystorePtr,
		/// Prometheus metrics for validators.
		metrics: validator_side_experimental::Metrics,
		/// Database used for reputation house keeping.
		db: Arc<dyn Database>,
		/// Reputation configuration (column number).
		reputation_config: validator_side_experimental::ReputationConfig,
		/// Clock used for all time reads. Production passes [`polkadot_node_clock::SystemClock`];
		/// tests inject a mock.
		clock: Arc<dyn Clock>,
	},
	/// Collators operate on a parachain.
	Collator {
		/// Local peer id.
		peer_id: PeerId,
		/// Parachain collator pair.
		collator_pair: CollatorPair,
		/// Receiver for v2 collation fetching requests.
		request_receiver_v2: IncomingRequestReceiver<protocol_v2::CollationFetchingRequest>,
		/// Receiver for v3 collation fetching requests.
		request_receiver_v3: IncomingRequestReceiver<protocol_v3::CollationFetchingRequest>,
		/// Metrics.
		metrics: collator_side::Metrics,
		/// Clock used for all time reads. Production passes [`polkadot_node_clock::SystemClock`];
		/// tests inject a mock.
		clock: Arc<dyn Clock>,
	},
	/// No protocol side, just disable it.
	None,
}

/// The collator protocol subsystem.
pub struct CollatorProtocolSubsystem {
	protocol_side: ProtocolSide,
}

#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
impl CollatorProtocolSubsystem {
	/// Start the collator protocol.
	pub fn new(protocol_side: ProtocolSide) -> Self {
		Self { protocol_side }
	}
}

#[overseer::subsystem(CollatorProtocol, error=SubsystemError, prefix=self::overseer)]
impl<Context> CollatorProtocolSubsystem {
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = match self.protocol_side {
			ProtocolSide::Validator {
				keystore,
				eviction_policy,
				metrics,
				invulnerables,
				collator_protocol_hold_off,
				clock,
			} => {
				gum::trace!(
					target: LOG_TARGET,
					?invulnerables,
					?collator_protocol_hold_off,
					"AH collator protocol params",
				);
				validator_side::run(
					ctx,
					keystore,
					eviction_policy,
					metrics,
					invulnerables,
					collator_protocol_hold_off,
					clock,
				)
				.map_err(|e| SubsystemError::with_origin("collator-protocol", e))
				.boxed()
			},
			ProtocolSide::ValidatorExperimental {
				keystore,
				metrics,
				db,
				reputation_config,
				clock,
			} => validator_side_experimental::run(
				ctx,
				keystore,
				metrics,
				db,
				reputation_config,
				clock,
			)
			.map_err(|e| SubsystemError::with_origin("collator-protocol", e))
			.boxed(),
			ProtocolSide::Collator {
				peer_id,
				collator_pair,
				request_receiver_v2,
				request_receiver_v3,
				metrics,
				clock,
			} => collator_side::run(
				ctx,
				peer_id,
				collator_pair,
				request_receiver_v2,
				request_receiver_v3,
				metrics,
				clock,
			)
			.map_err(|e| SubsystemError::with_origin("collator-protocol", e))
			.boxed(),
			ProtocolSide::None => return DummySubsystem.start(ctx),
		};

		SpawnedSubsystem { name: "collator-protocol-subsystem", future }
	}
}

/// Modify the reputation of a peer based on its behavior.
async fn modify_reputation(
	reputation: &mut ReputationAggregator,
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	peer: PeerId,
	rep: Rep,
) {
	gum::trace!(
		target: LOG_TARGET,
		rep = ?rep,
		peer_id = %peer,
		"reputation change for peer",
	);

	reputation.modify(sender, peer, rep).await;
}

/// Wait until tick and return the timestamp for the following one.
async fn wait_until_next_tick(clock: &dyn Clock, last_poll: Instant, period: Duration) -> Instant {
	let now = clock.now();
	let next_poll = last_poll + period;

	if next_poll > now {
		clock.delay(next_poll - now).await
	}

	clock.now()
}

/// Returns an infinite stream that yields with an interval of `period`.
fn tick_stream(clock: Arc<dyn Clock>, period: Duration) -> impl FusedStream<Item = ()> {
	futures::stream::unfold(clock.now(), move |next_check| {
		let clock = clock.clone();
		async move { Some(((), wait_until_next_tick(&*clock, next_check, period).await)) }
	})
	.fuse()
}

/// Scheduling info tracked per active leaf, used for V3 scheduling parent validation.
/// Stores the leaf's BABE slot and parent hash so the validator can determine whether
/// the scheduling parent corresponds to the last finished relay chain slot.
struct LeafSchedulingInfo {
	/// The parent hash of the leaf block.
	parent_hash: Hash,
	/// The BABE slot of the leaf block.
	slot: sp_consensus_slots::Slot,
}

pub(crate) async fn extract_leaf_scheduling_info<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
	leaf: H256,
) -> Option<LeafSchedulingInfo> {
	// Fetch leaf header to extract BABE slot for V3 scheduling parent validation.
	// Without this info, V3 advertisements referencing this leaf will be rejected.
	let (tx, rx) = oneshot::channel();
	sender.send_message(ChainApiMessage::BlockHeader(leaf, tx)).await;
	let header = rx.await.ok().and_then(|r| r.ok().flatten());
	header.and_then(|header| {
		let slot = header.digest.logs().iter().find_map(|log| log.as_babe_pre_digest())?.slot();
		Some(LeafSchedulingInfo { parent_hash: header.parent_hash, slot })
	})
}

pub(crate) fn is_scheduling_parent_valid(
	clock: &dyn Clock,
	scheduling_parent: &Hash,
	leaf_scheduling_info: &HashMap<Hash, LeafSchedulingInfo>,
) -> bool {
	let slot_duration = SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS);
	let current_slot = sp_consensus_slots::Slot::from_timestamp(
		sp_timestamp::Timestamp::new(clock.duration_since_epoch().as_millis() as u64),
		slot_duration,
	);
	if let Some(info) = leaf_scheduling_info.get(scheduling_parent) {
		// scheduling_parent is a leaf. This is allowed only when the leaf's slot is
		// the previous slot.
		*current_slot == *info.slot + 1
	} else {
		// scheduling_parent is not a leaf. This is allowed only if the sp is the parent of
		// any leaf whose slot is still in progress.
		leaf_scheduling_info
			.iter()
			.any(|(_, info)| *current_slot == *info.slot && *scheduling_parent == info.parent_hash)
	}
}

/// The per-core claim queues for one active leaf, together with the runtime scheduling lookahead
/// that bounds them.
///
/// Experimental (`validator_side_experimental`) goes through the [`Self::slots`] / [`Self::window`]
/// helpers, which own the lookahead arithmetic. Legacy (`validator_side`) has its own
/// bitfield-based allocation and reads the fields directly — a properly factored legacy would go
/// through the helpers too, but legacy is on its way out so we don't invest in that.
pub(crate) struct LeafClaimQueues {
	claim_queues: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	scheduling_lookahead: usize,
}

impl LeafClaimQueues {
	/// Fetch the per-core claim queues and the scheduling lookahead for `leaf` from the runtime.
	pub(crate) async fn fetch<Sender: CollatorProtocolSenderTrait>(
		leaf: Hash,
		session_index: SessionIndex,
		sender: &mut Sender,
	) -> std::result::Result<Self, runtime::Error> {
		let scheduling_lookahead =
			fetch_scheduling_lookahead(leaf, session_index, sender).await? as usize;
		let claim_queues = recv_runtime(request_claim_queue(leaf, sender).await).await?;
		Ok(Self { claim_queues, scheduling_lookahead })
	}

	/// The core's claim queue padded to the scheduling lookahead: slot `i` is `Some(para)` where
	/// scheduled, `None` for positions within the lookahead the runtime left unscheduled. Length
	/// is always the lookahead. `None` if the core has no claim queue at this leaf.
	pub(crate) fn slots(&self, core: CoreIndex) -> Option<Vec<Option<ParaId>>> {
		let cq = self.claim_queues.get(&core)?;
		Some(
			cq.iter()
				.copied()
				.map(Some)
				.chain(std::iter::repeat(None))
				.take(self.scheduling_lookahead)
				.collect(),
		)
	}

	/// Claim-queue positions on `core` reachable from a scheduling parent at `depth` from this
	/// leaf (leaf = depth 0). A depth-`d` SP can host advertisements landing at leaf-CQ positions
	/// `i` with `i + d < lookahead`, so the window is `cq[0 .. lookahead - d]` capped to the
	/// scheduled entries.
	pub(crate) fn window(&self, core: CoreIndex, depth: usize) -> Vec<ParaId> {
		let Some(cq) = self.claim_queues.get(&core) else { return Vec::new() };
		let valid_len = self.scheduling_lookahead.saturating_sub(depth).min(cq.len());
		cq.iter().take(valid_len).copied().collect()
	}
}
