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

//! Prospective-parachains-flavoured `World`. Composes [`WorldBase`] for shared
//! scaffolding and adds prospective-specific fluent verbs (introduce / back / queries).
//!
//! Tests boot a `World` via [`World::start`], create leaves via the
//! `HasBase`-provided `world.new_block().with_head_data(...).activate()` builder, then
//! drive prospective-flavoured queries directly through this struct's inherent
//! methods.

use super::ProspectiveParachains;
use futures::channel::oneshot;
use polkadot_node_subsystem::messages::{
	Ancestors, BackableCandidateRef, HypotheticalCandidate, HypotheticalMembership,
	HypotheticalMembershipRequest, IntroduceSecondedCandidateRequest, ParentHeadData,
	ProspectiveParachainsMessage, ProspectiveValidationDataRequest,
};
use polkadot_primitives::{
	CandidateHash, CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, Hash,
	HeadData, Id as ParaId, MutateDescriptorV2, PersistedValidationData, SessionIndex,
};
use polkadot_primitives_test_helpers::make_candidate;
use polkadot_subsystem_test_sim::{
	chain::CoreSchedule,
	world_base::{HasBase, LeafRef, WorldBase, WorldConfig},
};

// Re-export `HasBase` so tests' `use ...world::WorldExt` brings trait methods
// (`new_block`, `signal_active_leaves`, `deactivate_leaf`, `validation_code_hash`,
// `session_index`, `min_relay_parent_number_override`) into scope.
pub use polkadot_subsystem_test_sim::world_base::HasBase as WorldExt;
use std::sync::Arc;

/// Convenience alias preserving the in-crate prospective tests' `TestLeaf` naming.
pub type TestLeaf = LeafRef;

/// Suite-wide default [`WorldConfig`] for prospective scenarios — schedules
/// `chain_a` on core 0 and `chain_b` on core 1 for every block. Tests that need a
/// different shape construct their own [`WorldConfig`] inline.
pub fn default_world_config() -> WorldConfig {
	let chain_a = ParaId::from(1);
	let chain_b = ParaId::from(2);
	WorldConfig {
		schedule: vec![
			(CoreIndex(0), CoreSchedule::always(chain_a)),
			(CoreIndex(1), CoreSchedule::always(chain_b)),
		],
		..WorldConfig::default()
	}
}

/// Prospective-parachains-flavoured `World`. Composes [`WorldBase`] for shared
/// scaffolding; adds prospective-specific verbs as inherent methods.
pub struct World {
	/// Shared scaffolding: `Sim`, chain model, leaf bookkeeping. Plus default-impl
	/// methods (`activate_leaf`, etc.) reachable directly via `world.activate_leaf(...)`
	/// through the [`HasBase`] trait.
	pub base: WorldBase<ProspectiveParachains>,
}

impl HasBase for World {
	type Sut = ProspectiveParachains;
	fn base(&self) -> &WorldBase<Self::Sut> {
		&self.base
	}
	fn base_mut(&mut self) -> &mut WorldBase<Self::Sut> {
		&mut self.base
	}
}

impl World {
	/// Start a new world from a [`WorldConfig`]. No leaves active until
	/// [`HasBase::new_block`] (`...activate()`) runs. Mid-test config / chain changes
	/// go through `world.base.chain.lock()` (e.g. `add_session`, `set_claim_queue_at`);
	/// the [`WorldConfig`] copy on [`WorldBase::config`] stays frozen as the
	/// activation defaults.
	pub fn start(config: WorldConfig) -> Self {
		Self { base: WorldBase::<ProspectiveParachains>::start(config) }
	}

	// =====================================================================================
	// Prospective-flavoured fluent verbs. These don't fit on `WorldBase` because they
	// drive `ProspectiveParachainsMessage` and decode prospective-shaped replies.
	// =====================================================================================

	/// Drive `IntroduceSecondedCandidate` and return the subsystem's accept/reject reply.
	pub fn introduce_seconded_candidate(
		&mut self,
		candidate: CommittedCandidateReceipt,
		pvd: PersistedValidationData,
	) -> bool {
		let req = IntroduceSecondedCandidateRequest {
			candidate_para: candidate.descriptor.para_id(),
			candidate_receipt: candidate,
			persisted_validation_data: pvd,
		};
		let (tx, rx) = oneshot::channel();
		self.base
			.sim
			.send(ProspectiveParachainsMessage::IntroduceSecondedCandidate(req, tx));
		rx.now_or_never_ok()
			.expect("subsystem replied to IntroduceSecondedCandidate before parking")
	}

	/// Drive `CandidateBacked`. Fire-and-forget — no reply.
	pub fn back_candidate(&mut self, para: ParaId, candidate_hash: CandidateHash) {
		self.base
			.sim
			.send(ProspectiveParachainsMessage::CandidateBacked(para, candidate_hash));
	}

	/// Drive `GetBackableCandidates` and return the reply.
	pub fn get_backable_candidates(
		&mut self,
		leaf: Hash,
		para_id: ParaId,
		count: u32,
		ancestors: Ancestors,
	) -> Vec<BackableCandidateRef> {
		let (tx, rx) = oneshot::channel();
		self.base.sim.send(ProspectiveParachainsMessage::GetBackableCandidates {
			leaf,
			para_id,
			count,
			ancestors,
			sender: tx,
		});
		rx.now_or_never_ok()
			.expect("subsystem replied to GetBackableCandidates before parking")
	}

	/// Drive `GetHypotheticalMembership` and return the reply for the single submitted
	/// candidate.
	pub fn get_hypothetical_membership(
		&mut self,
		candidate_hash: CandidateHash,
		receipt: CommittedCandidateReceipt,
		pvd: PersistedValidationData,
	) -> Vec<(HypotheticalCandidate, HypotheticalMembership)> {
		let hypothetical = HypotheticalCandidate::Complete {
			candidate_hash,
			receipt: Arc::new(receipt),
			persisted_validation_data: pvd,
		};
		let request = HypotheticalMembershipRequest {
			candidates: vec![hypothetical],
			fragment_chain_relay_parent: None,
		};
		let (tx, rx) = oneshot::channel();
		self.base
			.sim
			.send(ProspectiveParachainsMessage::GetHypotheticalMembership(request, tx));
		rx.now_or_never_ok().expect("subsystem replied to GetHypotheticalMembership")
	}

	/// Drive `GetProspectiveValidationData` and return the reply.
	pub fn get_pvd(
		&mut self,
		para_id: ParaId,
		candidate_relay_parent: Hash,
		parent_head_data: HeadData,
		session_index: SessionIndex,
	) -> Option<PersistedValidationData> {
		let request = ProspectiveValidationDataRequest {
			para_id,
			candidate_relay_parent,
			session_index,
			parent_head_data: ParentHeadData::OnlyHash(parent_head_data.hash()),
		};
		let (tx, rx) = oneshot::channel();
		self.base
			.sim
			.send(ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx));
		rx.now_or_never_ok().expect("subsystem replied to GetProspectiveValidationData")
	}

	/// Build a child candidate of `parent` on `leaf`, introduce it as seconded, then back
	/// it. Mirrors the in-crate test helper of the same name. `index` is used for both
	/// the candidate's head data (`vec![index]`) and its `para_head` low-u64 hash —
	/// ensures distinct candidate hashes per index.
	pub fn make_and_back_candidate(
		&mut self,
		leaf: &TestLeaf,
		parent: &CommittedCandidateReceipt,
		index: u8,
	) -> (CommittedCandidateReceipt, CandidateHash) {
		let para = ParaId::from(1);
		let (mut candidate, pvd) = make_candidate(
			leaf.hash,
			leaf.number,
			para,
			parent.commitments.head_data.clone(),
			HeadData(vec![index]),
			self.validation_code_hash(),
		);
		candidate.descriptor.set_para_head(Hash::from_low_u64_le(index as u64));
		let candidate_hash = candidate.hash();
		assert!(self.introduce_seconded_candidate(candidate.clone(), pvd));
		self.back_candidate(para, candidate_hash);
		(candidate, candidate_hash)
	}
}

/// Helper to extract a oneshot::Receiver value the harness has settled into ready state.
trait OneshotNowOrNever<T> {
	fn now_or_never_ok(self) -> Option<T>;
}

impl<T> OneshotNowOrNever<T> for oneshot::Receiver<T> {
	fn now_or_never_ok(self) -> Option<T> {
		use futures::FutureExt;
		match self.now_or_never() {
			Some(Ok(v)) => Some(v),
			_ => None,
		}
	}
}
