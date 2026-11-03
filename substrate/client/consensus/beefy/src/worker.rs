// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
	communication::{
		gossip::{proofs_topic, votes_topic, GossipFilterCfg, GossipMessage},
		request_response::outgoing_requests_engine::ResponseInfo,
	},
	error::Error,
	find_authorities_change,
	fisherman::Fisherman,
	justification::BeefyVersionedFinalityProof,
	keystore::BeefyKeystore,
	metric_inc, metric_set,
	metrics::VoterMetrics,
	round::{Rounds, RoundsV4, VoteImportResult},
	BeefyComms, BeefyVoterLinks, UnpinnedFinalityNotification, LOG_TARGET,
};
use sp_application_crypto::RuntimeAppPublic;
use sp_blockchain::{Error as ClientError, Result as ClientResult};

use codec::{Codec, Decode, DecodeAll, Encode};
use futures::{stream::Fuse, FutureExt, StreamExt};
use log::{debug, error, info, trace, warn};
use sc_client_api::{Backend, HeaderBackend};
use sc_utils::notification::NotificationReceiver;
use sp_api::ProvideRuntimeApi;
use sp_arithmetic::traits::{AtLeast32Bit, Saturating};
use sp_consensus::SyncOracle;
use sp_consensus_beefy::{
	AuthorityIdBound, BeefyApi, Commitment, DoubleVotingProof, PayloadProvider, ValidatorSet,
	VersionedFinalityProof, VoteMessage, BEEFY_ENGINE_ID,
};
use sp_runtime::{
	generic::BlockId,
	traits::{Block, Header, NumberFor, Zero},
	SaturatedConversion,
};
use std::{
	collections::{BTreeMap, VecDeque},
	fmt::Debug,
	marker::PhantomData,
	sync::Arc,
};

/// Bound for the number of pending justifications - use 2400 - the max number
/// of justifications possible in a single session.
const MAX_BUFFERED_JUSTIFICATIONS: usize = 2400;

pub(crate) enum RoundAction {
	Drop,
	Process,
	Enqueue,
}

/// `VoterOracle` as persisted by aux-db schema v4.
///
/// Kept only to migrate v4 state to the current schema.
#[derive(Debug, Decode, Encode, PartialEq)]
pub(crate) struct VoterOracleV4<B: Block, AuthorityId: AuthorityIdBound> {
	pub(crate) sessions: VecDeque<RoundsV4<B, AuthorityId>>,
	pub(crate) min_block_delta: u32,
	pub(crate) best_grandpa_block_header: <B as Block>::Header,
	pub(crate) best_beefy_block: NumberFor<B>,
	pub(crate) _phantom: PhantomData<fn() -> AuthorityId>,
}

/// Responsible for the voting strategy.
/// It chooses which incoming votes to accept and which votes to generate.
/// Keeps track of voting seen for current and future rounds.
///
/// Note: this is part of `PersistedState` so any changes here should also bump
/// aux-db schema version.
#[derive(Debug, Decode, Encode, PartialEq)]
pub(crate) struct VoterOracle<B: Block, AuthorityId: AuthorityIdBound> {
	/// Queue of known sessions. Keeps track of voting rounds (block numbers) within each session.
	///
	/// There are three voter states corresponding to three queue states:
	/// 1. voter uninitialized: queue empty,
	/// 2. up-to-date - all mandatory blocks leading up to current GRANDPA finalized: queue has ONE
	///    element, the 'current session' where `mandatory_done == true`,
	/// 3. lagging behind GRANDPA: queue has [1, N] elements, where all `mandatory_done == false`.
	///    In this state, every time a session gets its mandatory block BEEFY finalized, it's
	///    popped off the queue, eventually getting to state `2. up-to-date`.
	sessions: VecDeque<Rounds<B, AuthorityId>>,
	/// Min delta in block numbers between two blocks, BEEFY should vote on.
	min_block_delta: u32,
	/// Best block we received a GRANDPA finality for.
	best_grandpa_block_header: <B as Block>::Header,
	/// Best block a BEEFY voting round has been concluded for.
	best_beefy_block: NumberFor<B>,
	_phantom: PhantomData<fn() -> AuthorityId>,
}

impl<B: Block, AuthorityId> VoterOracle<B, AuthorityId>
where
	AuthorityId: AuthorityIdBound,
{
	/// Verify provided `sessions` satisfies requirements, then build `VoterOracle`.
	pub fn checked_new(
		sessions: VecDeque<Rounds<B, AuthorityId>>,
		min_block_delta: u32,
		grandpa_header: <B as Block>::Header,
		best_beefy: NumberFor<B>,
	) -> Option<Self> {
		let mut prev_start = Zero::zero();
		let mut prev_validator_id = None;
		// verifies the
		let mut validate = || -> bool {
			let best_grandpa = *grandpa_header.number();
			if sessions.is_empty() || best_beefy > best_grandpa {
				return false;
			}
			for (idx, session) in sessions.iter().enumerate() {
				let start = session.session_start();
				if session.validators().is_empty() {
					return false;
				}
				if start > best_grandpa || start <= prev_start {
					return false;
				}
				#[cfg(not(test))]
				if let Some(prev_id) = prev_validator_id {
					if session.validator_set_id() <= prev_id {
						return false;
					}
				}
				if idx != 0 && session.mandatory_done() {
					return false;
				}
				prev_start = session.session_start();
				prev_validator_id = Some(session.validator_set_id());
			}
			true
		};
		if validate() {
			Some(VoterOracle {
				sessions,
				// Always target at least one block better than current best beefy.
				min_block_delta: min_block_delta.max(1),
				best_grandpa_block_header: grandpa_header,
				best_beefy_block: best_beefy,
				_phantom: PhantomData,
			})
		} else {
			error!(
				target: LOG_TARGET,
				"🥩 Invalid sessions queue: {:?}; best-beefy {:?} best-grandpa-header {:?}.",
				sessions,
				best_beefy,
				grandpa_header
			);
			None
		}
	}

	// Return reference to rounds pertaining to first session in the queue.
	// Voting will always happen at the head of the queue.
	fn active_rounds(&self) -> Result<&Rounds<B, AuthorityId>, Error> {
		self.sessions.front().ok_or(Error::UninitSession)
	}

	// Return mutable reference to rounds pertaining to first session in the queue.
	// Voting will always happen at the head of the queue.
	fn active_rounds_mut(&mut self) -> Result<&mut Rounds<B, AuthorityId>, Error> {
		self.sessions.front_mut().ok_or(Error::UninitSession)
	}

	fn current_validator_set(&self) -> Result<&ValidatorSet<AuthorityId>, Error> {
		self.active_rounds().map(|r| r.validator_set())
	}

	// Prune the sessions queue to keep the Oracle in one of the expected three states.
	//
	// To be called on each BEEFY finality and on each new rounds/session addition.
	fn try_prune(&mut self) {
		if self.sessions.len() > 1 {
			// when there's multiple sessions, only keep the `!mandatory_done()` ones.
			self.sessions.retain(|s| !s.mandatory_done())
		}
	}

	/// Check if an observed session can be added to the Oracle.
	pub fn can_add_session(&self, session_start: NumberFor<B>) -> bool {
		let latest_known_session_start =
			self.sessions.back().map(|session| session.session_start());
		Some(session_start) > latest_known_session_start
	}

	/// Add new observed session to the Oracle.
	pub fn add_session(&mut self, rounds: Rounds<B, AuthorityId>) {
		self.sessions.push_back(rounds);
		// Once we add a new session we can drop/prune previous session if it's been finalized.
		self.try_prune();
	}

	/// Finalize a particular block.
	pub fn finalize(&mut self, block: NumberFor<B>) -> Result<(), Error> {
		// Conclude voting round for this block.
		self.active_rounds_mut()?.conclude(block);
		// Prune any now "finalized" sessions from queue.
		self.try_prune();
		Ok(())
	}

	/// Return current pending mandatory block, if any, plus its active validator set.
	pub fn mandatory_pending(&self) -> Option<(NumberFor<B>, ValidatorSet<AuthorityId>)> {
		self.sessions.front().and_then(|round| {
			if round.mandatory_done() {
				None
			} else {
				Some((round.session_start(), round.validator_set().clone()))
			}
		})
	}

	/// Return `(A, B)` tuple representing inclusive [A, B] interval of votes to accept.
	pub fn accepted_interval(&self) -> Result<(NumberFor<B>, NumberFor<B>), Error> {
		let rounds = self.sessions.front().ok_or(Error::UninitSession)?;

		if rounds.mandatory_done() {
			// There's only one session active and its mandatory is done.
			// Accept any vote for a GRANDPA finalized block in a better round.
			Ok((
				rounds.session_start().max(self.best_beefy_block),
				(*self.best_grandpa_block_header.number()),
			))
		} else {
			// Current session has mandatory not done.
			// Only accept votes for the mandatory block in the front of queue.
			Ok((rounds.session_start(), rounds.session_start()))
		}
	}

	/// Utility function to quickly decide what to do for each round.
	pub fn triage_round(&self, round: NumberFor<B>) -> Result<RoundAction, Error> {
		let (start, end) = self.accepted_interval()?;
		if start <= round && round <= end {
			Ok(RoundAction::Process)
		} else if round > end {
			Ok(RoundAction::Enqueue)
		} else {
			Ok(RoundAction::Drop)
		}
	}

	/// Return `Some(number)` if we should be voting on block `number`,
	/// return `None` if there is no block we should vote on.
	pub fn voting_target(&self) -> Option<NumberFor<B>> {
		let rounds = self.sessions.front().or_else(|| {
			debug!(target: LOG_TARGET, "🥩 No voting round started");
			None
		})?;
		let best_grandpa = *self.best_grandpa_block_header.number();
		let best_beefy = self.best_beefy_block;

		// `target` is guaranteed > `best_beefy` since `min_block_delta` is at least `1`.
		let target =
			vote_target(best_grandpa, best_beefy, rounds.session_start(), self.min_block_delta);
		trace!(
			target: LOG_TARGET,
			"🥩 best beefy: #{:?}, best finalized: #{:?}, current_vote_target: {:?}",
			best_beefy,
			best_grandpa,
			target
		);
		target
	}
}

impl<B, AuthorityId> TryFrom<VoterOracleV4<B, AuthorityId>> for VoterOracle<B, AuthorityId>
where
	B: Block,
	AuthorityId: AuthorityIdBound,
{
	type Error = ClientError;

	fn try_from(old: VoterOracleV4<B, AuthorityId>) -> Result<Self, Self::Error> {
		let sessions = old
			.sessions
			.into_iter()
			.map(TryInto::try_into)
			.collect::<ClientResult<VecDeque<_>>>()?;

		VoterOracle::checked_new(
			sessions,
			old.min_block_delta,
			old.best_grandpa_block_header,
			old.best_beefy_block,
		)
		.ok_or_else(|| {
			ClientError::Backend("BEEFY DB is corrupted: invalid migrated voter oracle".into())
		})
	}
}

/// `PersistedState` as persisted by aux-db schema v4.
///
/// Kept only to migrate v4 state to the current schema.
#[derive(Debug, Decode, Encode, PartialEq)]
pub(crate) struct PersistedStateV4<B: Block, AuthorityId: AuthorityIdBound> {
	pub(crate) best_voted: NumberFor<B>,
	pub(crate) voting_oracle: VoterOracleV4<B, AuthorityId>,
	pub(crate) pallet_genesis: NumberFor<B>,
}

/// BEEFY voter state persisted in aux DB.
///
/// Note: Any changes here should also bump aux-db schema version.
#[derive(Debug, Decode, Encode, PartialEq)]
pub(crate) struct PersistedState<B: Block, AuthorityId: AuthorityIdBound> {
	/// Best block we voted on.
	best_voted: NumberFor<B>,
	/// Chooses which incoming votes to accept and which votes to generate.
	/// Keeps track of voting seen for current and future rounds.
	voting_oracle: VoterOracle<B, AuthorityId>,
	/// Pallet-beefy genesis block - block number when BEEFY consensus started for this chain.
	pallet_genesis: NumberFor<B>,
}

impl<B, AuthorityId> TryFrom<PersistedStateV4<B, AuthorityId>> for PersistedState<B, AuthorityId>
where
	B: Block,
	AuthorityId: AuthorityIdBound,
{
	type Error = ClientError;

	fn try_from(old: PersistedStateV4<B, AuthorityId>) -> Result<Self, Self::Error> {
		let voting_oracle: VoterOracle<B, AuthorityId> = old.voting_oracle.try_into()?;

		Ok(PersistedState {
			best_voted: old.best_voted,
			voting_oracle,
			pallet_genesis: old.pallet_genesis,
		})
	}
}

impl<B: Block, AuthorityId: AuthorityIdBound> PersistedState<B, AuthorityId> {
	pub fn checked_new(
		grandpa_header: <B as Block>::Header,
		best_beefy: NumberFor<B>,
		sessions: VecDeque<Rounds<B, AuthorityId>>,
		min_block_delta: u32,
		pallet_genesis: NumberFor<B>,
	) -> Option<Self> {
		VoterOracle::checked_new(sessions, min_block_delta, grandpa_header, best_beefy).map(
			|voting_oracle| PersistedState {
				best_voted: Zero::zero(),
				voting_oracle,
				pallet_genesis,
			},
		)
	}

	pub fn pallet_genesis(&self) -> NumberFor<B> {
		self.pallet_genesis
	}

	pub(crate) fn set_min_block_delta(&mut self, min_block_delta: u32) {
		self.voting_oracle.min_block_delta = min_block_delta.max(1);
	}

	pub fn best_beefy(&self) -> NumberFor<B> {
		self.voting_oracle.best_beefy_block
	}

	pub(crate) fn set_best_beefy(&mut self, best_beefy: NumberFor<B>) {
		self.voting_oracle.best_beefy_block = best_beefy;
	}

	pub(crate) fn set_best_grandpa(&mut self, best_grandpa: <B as Block>::Header) {
		self.voting_oracle.best_grandpa_block_header = best_grandpa;
	}

	pub fn voting_oracle(&self) -> &VoterOracle<B, AuthorityId> {
		&self.voting_oracle
	}

	pub(crate) fn gossip_filter_config(
		&self,
	) -> Result<GossipFilterCfg<'_, B, AuthorityId>, Error> {
		let (start, end) = self.voting_oracle.accepted_interval()?;
		let validator_set = self.voting_oracle.current_validator_set()?;
		Ok(GossipFilterCfg { start, end, validator_set })
	}

	/// Handle session changes by starting new voting round for mandatory blocks.
	pub fn init_session_at(
		&mut self,
		new_session_start: NumberFor<B>,
		validator_set: ValidatorSet<AuthorityId>,
		key_store: &BeefyKeystore<AuthorityId>,
		metrics: &Option<VoterMetrics>,
		is_authority: bool,
	) {
		debug!(target: LOG_TARGET, "🥩 New active validator set: {:?}", validator_set);

		// BEEFY should finalize a mandatory block during each session.
		if let Ok(active_session) = self.voting_oracle.active_rounds() {
			if !active_session.mandatory_done() {
				debug!(
					target: LOG_TARGET,
					"🥩 New session {} while active session {} is still lagging.",
					validator_set.id(),
					active_session.validator_set_id(),
				);
				metric_inc!(metrics, beefy_lagging_sessions);
			}
		}

		// verify we have some BEEFY key available in keystore when role is authority.
		if is_authority && key_store.public_keys().map_or(false, |k| k.is_empty()) {
			error!(
				target: LOG_TARGET,
				"🥩 for session starting at block {:?} no BEEFY authority key found in store, \
				you must generate valid session keys \
				(https://docs.polkadot.com/infrastructure/running-a-validator/onboarding-and-offboarding/key-management/#set-session-keys)",
				new_session_start,
			);
			metric_inc!(metrics, beefy_no_authority_found_in_store);
		}

		let id = validator_set.id();
		self.voting_oracle.add_session(Rounds::new(new_session_start, validator_set));
		metric_set!(metrics, beefy_validator_set_id, id);
		info!(
			target: LOG_TARGET,
			"🥩 New Rounds for validator set id: {:?} with session_start {:?}",
			id,
			new_session_start
		);
	}
}

/// A BEEFY worker/voter that follows the BEEFY protocol
pub(crate) struct BeefyWorker<B: Block, BE, P, RuntimeApi, S, N, AuthorityId: AuthorityIdBound> {
	// utilities
	pub backend: Arc<BE>,
	pub runtime: Arc<RuntimeApi>,
	pub key_store: Arc<BeefyKeystore<AuthorityId>>,
	pub payload_provider: P,
	pub sync: Arc<S>,
	pub fisherman: Arc<Fisherman<B, BE, RuntimeApi, AuthorityId>>,

	// communication (created once, but returned and reused if worker is restarted/reinitialized)
	pub comms: BeefyComms<B, N, AuthorityId>,

	// channels
	/// Links between the block importer, the background voter and the RPC layer.
	pub links: BeefyVoterLinks<B, AuthorityId>,

	// voter state
	/// Buffer holding justifications for future processing.
	pub pending_justifications: BTreeMap<NumberFor<B>, BeefyVersionedFinalityProof<B, AuthorityId>>,
	/// Persisted voter state.
	pub persisted_state: PersistedState<B, AuthorityId>,
	/// BEEFY voter metrics
	pub metrics: Option<VoterMetrics>,
	/// Node runs under "Authority" role.
	pub is_authority: bool,
}

impl<B, BE, P, R, S, N, AuthorityId> BeefyWorker<B, BE, P, R, S, N, AuthorityId>
where
	B: Block + Codec,
	BE: Backend<B>,
	P: PayloadProvider<B>,
	S: SyncOracle,
	R: ProvideRuntimeApi<B>,
	R::Api: BeefyApi<B, AuthorityId>,
	AuthorityId: AuthorityIdBound,
{
	fn best_grandpa_block(&self) -> NumberFor<B> {
		*self.persisted_state.voting_oracle.best_grandpa_block_header.number()
	}

	fn voting_oracle(&self) -> &VoterOracle<B, AuthorityId> {
		&self.persisted_state.voting_oracle
	}

	#[cfg(test)]
	fn active_rounds(&mut self) -> Result<&Rounds<B, AuthorityId>, Error> {
		self.persisted_state.voting_oracle.active_rounds()
	}

	/// Handle session changes by starting new voting round for mandatory blocks.
	fn init_session_at(
		&mut self,
		validator_set: ValidatorSet<AuthorityId>,
		new_session_start: NumberFor<B>,
	) {
		self.persisted_state.init_session_at(
			new_session_start,
			validator_set,
			&self.key_store,
			&self.metrics,
			self.is_authority,
		);
	}

	fn handle_finality_notification(
		&mut self,
		notification: &UnpinnedFinalityNotification<B>,
	) -> Result<(), Error> {
		let header = &notification.header;
		debug!(
			target: LOG_TARGET,
			"🥩 Finality notification: header(number {:?}, hash {:?}) tree_route {:?}",
			header.number(),
			notification.hash,
			notification.tree_route,
		);

		match self.runtime.runtime_api().beefy_genesis(notification.hash) {
			Ok(Some(genesis)) if genesis != self.persisted_state.pallet_genesis => {
				debug!(target: LOG_TARGET, "🥩 ConsensusReset detected. Expected genesis: {}, found genesis: {}", self.persisted_state.pallet_genesis, genesis);
				return Err(Error::ConsensusReset);
			},
			Ok(_) => {},
			Err(api_error) => {
				// This can happen in case the block was already pruned.
				// Mostly after warp sync when finality notifications are piled up.
				debug!(target: LOG_TARGET, "🥩 Unable to check beefy genesis: {}", api_error);
			},
		}

		let mut new_session_added = false;
		if *header.number() > self.best_grandpa_block() {
			// update best GRANDPA finalized block we have seen
			self.persisted_state.set_best_grandpa(header.clone());

			// Check all (newly) finalized blocks for new session(s).
			let backend = self.backend.clone();
			for header in notification
				.tree_route
				.iter()
				.map(|hash| {
					backend
						.blockchain()
						.expect_header(*hash)
						.expect("just finalized block should be available; qed.")
				})
				.chain(std::iter::once(header.clone()))
			{
				if let Some(new_validator_set) = find_authorities_change::<B, AuthorityId>(&header)
				{
					self.init_session_at(new_validator_set, *header.number());
					new_session_added = true;
				}
			}

			if new_session_added {
				crate::aux_schema::write_voter_state(&*self.backend, &self.persisted_state)
					.map_err(|e| Error::Backend(e.to_string()))?;
			}

			// Update gossip validator votes filter.
			if let Err(e) = self
				.persisted_state
				.gossip_filter_config()
				.map(|filter| self.comms.gossip_validator.update_filter(filter))
			{
				error!(target: LOG_TARGET, "🥩 Voter error: {:?}", e);
			}
		}

		Ok(())
	}

	/// Based on [VoterOracle] this vote is either processed here or discarded.
	fn triage_incoming_vote(
		&mut self,
		vote: VoteMessage<NumberFor<B>, AuthorityId, <AuthorityId as RuntimeAppPublic>::Signature>,
	) -> Result<(), Error>
	where
		<AuthorityId as RuntimeAppPublic>::Signature: Encode + Decode,
	{
		let block_num = vote.commitment.block_number;
		match self.voting_oracle().triage_round(block_num)? {
			RoundAction::Process => {
				if let Some(finality_proof) = self.handle_vote(vote)? {
					let gossip_proof =
						GossipMessage::<B, AuthorityId>::FinalityProof(finality_proof);
					let encoded_proof = gossip_proof.encode();
					self.comms.gossip_engine.gossip_message(
						proofs_topic::<B>(),
						encoded_proof,
						true,
					);
				}
			},
			RoundAction::Drop => metric_inc!(self.metrics, beefy_stale_votes),
			RoundAction::Enqueue => error!(target: LOG_TARGET, "🥩 unexpected vote: {:?}.", vote),
		};
		Ok(())
	}

	/// Based on [VoterOracle] this justification is either processed here or enqueued for later.
	///
	/// Expects `justification` to be valid.
	fn triage_incoming_justif(
		&mut self,
		justification: BeefyVersionedFinalityProof<B, AuthorityId>,
	) -> Result<(), Error> {
		let signed_commitment = match justification {
			VersionedFinalityProof::V1(ref sc) => sc,
		};
		let block_num = signed_commitment.commitment.block_number;
		match self.voting_oracle().triage_round(block_num)? {
			RoundAction::Process => {
				debug!(target: LOG_TARGET, "🥩 Process justification for round: {:?}.", block_num);
				metric_inc!(self.metrics, beefy_imported_justifications);
				self.finalize(justification)?
			},
			RoundAction::Enqueue => {
				debug!(target: LOG_TARGET, "🥩 Buffer justification for round: {:?}.", block_num);
				if self.pending_justifications.len() < MAX_BUFFERED_JUSTIFICATIONS {
					self.pending_justifications.entry(block_num).or_insert(justification);
					metric_inc!(self.metrics, beefy_buffered_justifications);
				} else {
					metric_inc!(self.metrics, beefy_buffered_justifications_dropped);
					warn!(
						target: LOG_TARGET,
						"🥩 Buffer justification dropped for round: {:?}.", block_num
					);
				}
			},
			RoundAction::Drop => metric_inc!(self.metrics, beefy_stale_justifications),
		};
		Ok(())
	}

	fn handle_vote(
		&mut self,
		vote: VoteMessage<NumberFor<B>, AuthorityId, <AuthorityId as RuntimeAppPublic>::Signature>,
	) -> Result<Option<BeefyVersionedFinalityProof<B, AuthorityId>>, Error> {
		let rounds = self.persisted_state.voting_oracle.active_rounds_mut()?;

		let block_number = vote.commitment.block_number;
		match rounds.add_vote(vote) {
			VoteImportResult::RoundConcluded(signed_commitment) => {
				let finality_proof = VersionedFinalityProof::V1(signed_commitment);
				debug!(
					target: LOG_TARGET,
					"🥩 Round #{} concluded, finality_proof: {:?}.", block_number, finality_proof
				);
				// We created the `finality_proof` and know to be valid.
				// New state is persisted after finalization.
				self.finalize(finality_proof.clone())?;
				metric_inc!(self.metrics, beefy_good_votes_processed);
				return Ok(Some(finality_proof));
			},
			VoteImportResult::Ok => {
				// Persist state after handling mandatory block vote.
				if self
					.voting_oracle()
					.mandatory_pending()
					.map(|(mandatory_num, _)| mandatory_num == block_number)
					.unwrap_or(false)
				{
					crate::aux_schema::write_voter_state(&*self.backend, &self.persisted_state)
						.map_err(|e| Error::Backend(e.to_string()))?;
				}
				metric_inc!(self.metrics, beefy_good_votes_processed);
			},
			VoteImportResult::DoubleVoting(proof) => {
				metric_inc!(self.metrics, beefy_equivocation_votes);
				self.report_double_voting(proof)?;
			},
			VoteImportResult::Invalid => metric_inc!(self.metrics, beefy_invalid_votes),
			VoteImportResult::Stale => metric_inc!(self.metrics, beefy_stale_votes),
		};
		Ok(None)
	}

	/// Provide BEEFY finality for block based on `finality_proof`:
	/// 1. Prune now-irrelevant past sessions from the oracle,
	/// 2. Set BEEFY best block,
	/// 3. Persist voter state,
	/// 4. Send best block hash and `finality_proof` to RPC worker.
	///
	/// Expects `finality proof` to be valid and for a block > current-best-beefy.
	fn finalize(
		&mut self,
		finality_proof: BeefyVersionedFinalityProof<B, AuthorityId>,
	) -> Result<(), Error> {
		let block_num = match finality_proof {
			VersionedFinalityProof::V1(ref sc) => sc.commitment.block_number,
		};

		if block_num <= self.persisted_state.voting_oracle.best_beefy_block {
			// we've already finalized this round before, short-circuit.
			return Ok(());
		}

		// Finalize inner round and update voting_oracle state.
		self.persisted_state.voting_oracle.finalize(block_num)?;

		// Set new best BEEFY block number.
		self.persisted_state.set_best_beefy(block_num);
		crate::aux_schema::write_voter_state(&*self.backend, &self.persisted_state)
			.map_err(|e| Error::Backend(e.to_string()))?;

		metric_set!(self.metrics, beefy_best_block, block_num);

		self.comms.on_demand_justifications.cancel_requests_older_than(block_num);

		if let Err(e) = self
			.backend
			.blockchain()
			.expect_block_hash_from_id(&BlockId::Number(block_num))
			.and_then(|hash| {
				self.links
					.to_rpc_best_block_sender
					.notify(|| Ok::<_, ()>(hash))
					.expect("forwards closure result; the closure always returns Ok; qed.");

				self.backend
					.append_justification(hash, (BEEFY_ENGINE_ID, finality_proof.encode()))
			}) {
			debug!(
				target: LOG_TARGET,
				"🥩 Error {:?} on appending justification: {:?}", e, finality_proof
			);
		}

		self.links
			.to_rpc_justif_sender
			.notify(|| Ok::<_, ()>(finality_proof))
			.expect("forwards closure result; the closure always returns Ok; qed.");

		// Update gossip validator votes filter.
		self.persisted_state
			.gossip_filter_config()
			.map(|filter| self.comms.gossip_validator.update_filter(filter))?;
		Ok(())
	}

	/// Handle previously buffered justifications, that now land in the voting interval.
	fn try_pending_justifications(&mut self) -> Result<(), Error> {
		// Interval of blocks for which we can process justifications and votes right now.
		let (start, end) = self.voting_oracle().accepted_interval()?;
		// Process pending justifications.
		if !self.pending_justifications.is_empty() {
			// These are still pending.
			let still_pending =
				self.pending_justifications.split_off(&end.saturating_add(1u32.into()));
			// These can be processed.
			let justifs_to_process = self.pending_justifications.split_off(&start);
			// The rest can be dropped.
			self.pending_justifications = still_pending;

			for (num, justification) in justifs_to_process.into_iter() {
				debug!(target: LOG_TARGET, "🥩 Handle buffered justification for: {:?}.", num);
				metric_inc!(self.metrics, beefy_imported_justifications);
				if let Err(err) = self.finalize(justification) {
					error!(target: LOG_TARGET, "🥩 Error finalizing block: {}", err);
				}
			}
			metric_set!(
				self.metrics,
				beefy_buffered_justifications,
				self.pending_justifications.len()
			);
		}
		Ok(())
	}

	/// Decide if should vote, then vote.. or don't..
	fn try_to_vote(&mut self) -> Result<(), Error> {
		// Vote if there's now a new vote target.
		if let Some(target) = self.voting_oracle().voting_target() {
			metric_set!(self.metrics, beefy_should_vote_on, target);
			if target > self.persisted_state.best_voted {
				self.do_vote(target)?;
			}
		}
		Ok(())
	}

	/// Create and gossip Signed Commitment for block number `target_number`.
	///
	/// Also handle this self vote by calling `self.handle_vote()` for it.
	fn do_vote(&mut self, target_number: NumberFor<B>) -> Result<(), Error> {
		debug!(target: LOG_TARGET, "🥩 Try voting on {}", target_number);

		// Most of the time we get here, `target` is actually `best_grandpa`,
		// avoid getting header from backend in that case.
		let target_header = if target_number == self.best_grandpa_block() {
			self.persisted_state.voting_oracle.best_grandpa_block_header.clone()
		} else {
			let hash = self
				.backend
				.blockchain()
				.expect_block_hash_from_id(&BlockId::Number(target_number))
				.map_err(|err| {
					let err_msg = format!(
						"Couldn't get hash for block #{:?} (error: {:?}), skipping vote..",
						target_number, err
					);
					Error::Backend(err_msg)
				})?;

			self.backend.blockchain().expect_header(hash).map_err(|err| {
				let err_msg = format!(
					"Couldn't get header for block #{:?} ({:?}) (error: {:?}), skipping vote..",
					target_number, hash, err
				);
				Error::Backend(err_msg)
			})?
		};
		let target_hash = target_header.hash();

		let payload = if let Some(hash) = self.payload_provider.payload(&target_header) {
			hash
		} else {
			warn!(target: LOG_TARGET, "🥩 No MMR root digest found for: {:?}", target_hash);
			return Ok(());
		};

		let rounds = self.persisted_state.voting_oracle.active_rounds_mut()?;
		let (validators, validator_set_id) = (rounds.validators(), rounds.validator_set_id());

		let authority_id = if let Some(id) = self.key_store.authority_id(validators) {
			debug!(target: LOG_TARGET, "🥩 Local authority id: {:?}", id);
			id
		} else {
			debug!(
				target: LOG_TARGET,
				"🥩 Missing validator id - can't vote for: {:?}", target_hash
			);
			return Ok(());
		};

		let commitment = Commitment { payload, block_number: target_number, validator_set_id };
		let encoded_commitment = commitment.encode();

		let signature = match self.key_store.sign(&authority_id, &encoded_commitment) {
			Ok(sig) => sig,
			Err(err) => {
				warn!(target: LOG_TARGET, "🥩 Error signing commitment: {:?}", err);
				return Ok(());
			},
		};

		trace!(
			target: LOG_TARGET,
			"🥩 Produced signature using {:?}, is_valid: {:?}",
			authority_id,
			BeefyKeystore::verify(&authority_id, &signature, &encoded_commitment)
		);

		let vote = VoteMessage { commitment, id: authority_id, signature };
		if let Some(finality_proof) = self.handle_vote(vote.clone()).map_err(|err| {
			error!(target: LOG_TARGET, "🥩 Error handling self vote: {}", err);
			err
		})? {
			let encoded_proof =
				GossipMessage::<B, AuthorityId>::FinalityProof(finality_proof).encode();
			self.comms
				.gossip_engine
				.gossip_message(proofs_topic::<B>(), encoded_proof, true);
		} else {
			metric_inc!(self.metrics, beefy_votes_sent);
			debug!(target: LOG_TARGET, "🥩 Sent vote message: {:?}", vote);
			let encoded_vote = GossipMessage::<B, AuthorityId>::Vote(vote).encode();
			self.comms.gossip_engine.gossip_message(votes_topic::<B>(), encoded_vote, false);
		}

		// Persist state after vote to avoid double voting in case of voter restarts.
		self.persisted_state.best_voted = target_number;
		metric_set!(self.metrics, beefy_best_voted, target_number);
		crate::aux_schema::write_voter_state(&*self.backend, &self.persisted_state)
			.map_err(|e| Error::Backend(e.to_string()))
	}

	fn process_new_state(&mut self) {
		// Handle pending justifications and/or votes for now GRANDPA finalized blocks.
		if let Err(err) = self.try_pending_justifications() {
			debug!(target: LOG_TARGET, "🥩 {}", err);
		}

		// Don't bother voting or requesting justifications during major sync.
		if !self.sync.is_major_syncing() {
			// There were external events, 'state' is changed, author a vote if needed/possible.
			if let Err(err) = self.try_to_vote() {
				debug!(target: LOG_TARGET, "🥩 {}", err);
			}
			// If the current target is a mandatory block,
			// make sure there's also an on-demand justification request out for it.
			if let Some((block, active)) = self.voting_oracle().mandatory_pending() {
				// This only starts new request if there isn't already an active one.
				self.comms.on_demand_justifications.request(block, active);
			}
		}
	}

	/// Main loop for BEEFY worker.
	///
	/// Run the main async loop which is driven by finality notifications and gossiped votes.
	/// Should never end, returns `Error` otherwise.
	pub(crate) async fn run(
		mut self,
		block_import_justif: &mut Fuse<
			NotificationReceiver<BeefyVersionedFinalityProof<B, AuthorityId>>,
		>,
		finality_notifications: &mut Fuse<crate::FinalityNotifications<B>>,
	) -> (Error, BeefyComms<B, N, AuthorityId>) {
		info!(
			target: LOG_TARGET,
			"🥩 run BEEFY worker, best grandpa: #{:?}.",
			self.best_grandpa_block()
		);

		let mut votes = Box::pin(
			self.comms
				.gossip_engine
				.messages_for(votes_topic::<B>())
				.filter_map(|notification| async move {
					let vote =
						GossipMessage::<B, AuthorityId>::decode_all(&mut &notification.message[..])
							.ok()
							.and_then(|message| message.unwrap_vote());
					trace!(target: LOG_TARGET, "🥩 Got vote message: {:?}", vote);
					vote
				})
				.fuse(),
		);
		let mut gossip_proofs = Box::pin(
			self.comms
				.gossip_engine
				.messages_for(proofs_topic::<B>())
				.filter_map(|notification| async move {
					let proof =
						GossipMessage::<B, AuthorityId>::decode_all(&mut &notification.message[..])
							.ok()
							.and_then(|message| message.unwrap_finality_proof());
					trace!(target: LOG_TARGET, "🥩 Got gossip proof message: {:?}", proof);
					proof
				})
				.fuse(),
		);

		self.process_new_state();
		let error = loop {
			// Mutable reference used to drive the gossip engine.
			let mut gossip_engine = &mut self.comms.gossip_engine;

			// Wait for, and handle external events.
			// The branches below only change 'state', actual voting happens afterwards,
			// based on the new resulting 'state'.
			futures::select_biased! {
				// Use `select_biased!` to prioritize order below.
				// Process finality notifications first since these drive the voter.
				notification = finality_notifications.next() => {
					if let Some(notif) = notification {
						if let Err(err) = self.handle_finality_notification(&notif) {
							break err;
						}
					} else {
						break Error::FinalityStreamTerminated;
					}
				},
				// Make sure to pump gossip engine.
				_ = gossip_engine => {
					break Error::GossipEngineTerminated;
				},
				// Process incoming justifications as these can make some in-flight votes obsolete.
				response_info = self.comms.on_demand_justifications.next().fuse() => {
					match response_info {
						ResponseInfo::ValidProof(justif, peer_report) => {
							if let Err(err) = self.triage_incoming_justif(justif) {
								debug!(target: LOG_TARGET, "🥩 {}", err);
							}
							self.comms.gossip_engine.report(peer_report.who, peer_report.cost_benefit);
						},
						ResponseInfo::PeerReport(peer_report) => {
							self.comms.gossip_engine.report(peer_report.who, peer_report.cost_benefit);
						},
						ResponseInfo::Pending => {},
					}
				},
				justif = block_import_justif.next() => {
					if let Some(justif) = justif {
						// Block import justifications have already been verified to be valid
						// by `BeefyBlockImport`.
						if let Err(err) = self.triage_incoming_justif(justif) {
							debug!(target: LOG_TARGET, "🥩 {}", err);
						}
					} else {
						break Error::BlockImportStreamTerminated;
					}
				},
				justif = gossip_proofs.next() => {
					if let Some(justif) = justif {
						// Gossiped justifications have already been verified by `GossipValidator`.
						if let Err(err) = self.triage_incoming_justif(justif) {
							debug!(target: LOG_TARGET, "🥩 {}", err);
						}
					} else {
						break Error::FinalityProofGossipStreamTerminated;
					}
				},
				// Finally process incoming votes.
				vote = votes.next() => {
					if let Some(vote) = vote {
						// Votes have already been verified to be valid by the gossip validator.
						if let Err(err) = self.triage_incoming_vote(vote) {
							debug!(target: LOG_TARGET, "🥩 {}", err);
						}
					} else {
						break Error::VotesGossipStreamTerminated;
					}
				},
			}

			// Act on changed 'state'.
			self.process_new_state();
		};

		// return error _and_ `comms` that can be reused
		(error, self.comms)
	}

	/// Report the given equivocation to the BEEFY runtime module.
	fn report_double_voting(
		&self,
		proof: DoubleVotingProof<
			NumberFor<B>,
			AuthorityId,
			<AuthorityId as RuntimeAppPublic>::Signature,
		>,
	) -> Result<(), Error> {
		let rounds = self.persisted_state.voting_oracle.active_rounds()?;
		self.fisherman.report_double_voting(proof, rounds)
	}
}

/// Calculate next block number to vote on.
///
/// Return `None` if there is no votable target yet.
fn vote_target<N>(best_grandpa: N, best_beefy: N, session_start: N, min_delta: u32) -> Option<N>
where
	N: AtLeast32Bit + Copy + Debug,
{
	// if the mandatory block (session_start) does not have a beefy justification yet,
	// we vote on it
	let target = if best_beefy < session_start {
		debug!(target: LOG_TARGET, "🥩 vote target - mandatory block: #{:?}", session_start);
		session_start
	} else {
		let diff = best_grandpa.saturating_sub(best_beefy) + 1u32.into();
		let diff = diff.saturated_into::<u32>() / 2;
		let target = best_beefy + min_delta.max(diff.next_power_of_two()).into();
		trace!(
			target: LOG_TARGET,
			"🥩 vote target - diff: {:?}, next_power_of_two: {:?}, target block: #{:?}",
			diff,
			diff.next_power_of_two(),
			target,
		);

		target
	};

	// Don't vote for targets until they've been finalized
	// (`target` can be > `best_grandpa` when `min_delta` is big enough).
	if target > best_grandpa {
		None
	} else {
		Some(target)
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::{
		communication::{
			gossip::{tests::TestNetwork, GossipValidator},
			notification::{BeefyBestBlockStream, BeefyVersionedFinalityProofStream},
			request_response::outgoing_requests_engine::OnDemandJustificationsEngine,
		},
		tests::{
			create_beefy_keystore, get_beefy_streams, make_beefy_ids, BeefyPeer, BeefyTestNet,
			TestApi,
		},
		BeefyRPCLinks, KnownPeers,
	};
	use futures::{future::poll_fn, task::Poll};
	use parking_lot::Mutex;
	use sc_client_api::{Backend as BackendT, HeaderBackend};
	use sc_network_gossip::GossipEngine;
	use sc_network_sync::SyncingService;
	use sc_network_test::TestNetFactory;
	use sp_blockchain::Backend as BlockchainBackendT;
	use sp_consensus_beefy::{
		ecdsa_crypto, known_payloads,
		known_payloads::MMR_ROOT_ID,
		mmr::MmrRootProvider,
		test_utils::{generate_double_voting_proof, Keyring},
		ConsensusLog, Payload, SignedCommitment,
	};
	use sp_runtime::traits::{Header as HeaderT, One};
	use substrate_test_runtime_client::{
		runtime::{Block, Digest, DigestItem, Header},
		Backend,
	};

	impl<B: super::Block, AuthorityId: AuthorityIdBound> PersistedState<B, AuthorityId> {
		pub fn active_round(&self) -> Result<&Rounds<B, AuthorityId>, Error> {
			self.voting_oracle.active_rounds()
		}

		pub fn best_grandpa_number(&self) -> NumberFor<B> {
			*self.voting_oracle.best_grandpa_block_header.number()
		}
	}

	impl<B: super::Block> VoterOracle<B, ecdsa_crypto::AuthorityId> {
		pub fn sessions(&self) -> &VecDeque<Rounds<B, ecdsa_crypto::AuthorityId>> {
			&self.sessions
		}
	}

	fn create_beefy_worker(
		peer: &mut BeefyPeer,
		key: &Keyring<ecdsa_crypto::AuthorityId>,
		min_block_delta: u32,
		genesis_validator_set: ValidatorSet<ecdsa_crypto::AuthorityId>,
	) -> BeefyWorker<
		Block,
		Backend,
		MmrRootProvider<Block, TestApi>,
		TestApi,
		Arc<SyncingService<Block>>,
		TestNetwork,
		ecdsa_crypto::AuthorityId,
	> {
		let keystore = create_beefy_keystore(key);

		let (to_rpc_justif_sender, from_voter_justif_stream) =
			BeefyVersionedFinalityProofStream::<Block, ecdsa_crypto::AuthorityId>::channel();
		let (to_rpc_best_block_sender, from_voter_best_beefy_stream) =
			BeefyBestBlockStream::<Block>::channel();
		let (_, from_block_import_justif_stream) =
			BeefyVersionedFinalityProofStream::<Block, ecdsa_crypto::AuthorityId>::channel();

		let beefy_rpc_links =
			BeefyRPCLinks { from_voter_justif_stream, from_voter_best_beefy_stream };
		*peer.data.beefy_rpc_links.lock() = Some(beefy_rpc_links);

		let links = BeefyVoterLinks {
			from_block_import_justif_stream,
			to_rpc_justif_sender,
			to_rpc_best_block_sender,
		};

		let backend = peer.client().as_backend();
		let beefy_genesis = 1;
		let api = Arc::new(TestApi::with_validator_set(&genesis_validator_set));
		let network = peer.network_service().clone();
		let sync = peer.sync_service().clone();
		let notification_service = peer
			.take_notification_service(&crate::tests::beefy_gossip_proto_name())
			.unwrap();
		let known_peers = Arc::new(Mutex::new(KnownPeers::new()));
		let gossip_validator =
			GossipValidator::new(known_peers.clone(), Arc::new(TestNetwork::new().0));
		let gossip_validator = Arc::new(gossip_validator);
		let gossip_engine = GossipEngine::new(
			network.clone(),
			sync.clone(),
			notification_service,
			"/beefy/1",
			gossip_validator.clone(),
			None,
		);
		let metrics = None;
		let on_demand_justifications = OnDemandJustificationsEngine::new(
			network.clone(),
			"/beefy/justifs/1".into(),
			known_peers,
			None,
		);
		// Push 1 block - will start first session.
		let hashes = peer.push_blocks(1, false);
		backend.finalize_block(hashes[0], None).unwrap();
		let first_header = backend
			.blockchain()
			.expect_header(backend.blockchain().info().best_hash)
			.unwrap();
		let persisted_state = PersistedState::checked_new(
			first_header,
			Zero::zero(),
			vec![Rounds::new(One::one(), genesis_validator_set)].into(),
			min_block_delta,
			beefy_genesis,
		)
		.unwrap();
		let payload_provider = MmrRootProvider::new(api.clone());
		let comms = BeefyComms { gossip_engine, gossip_validator, on_demand_justifications };
		let key_store: Arc<BeefyKeystore<ecdsa_crypto::AuthorityId>> =
			Arc::new(Some(keystore).into());
		BeefyWorker {
			backend: backend.clone(),
			runtime: api.clone(),
			key_store: key_store.clone(),
			metrics,
			payload_provider,
			sync: Arc::new(sync),
			fisherman: Arc::new(Fisherman::new(backend, api, key_store)),
			links,
			comms,
			pending_justifications: BTreeMap::new(),
			persisted_state,
			is_authority: true,
		}
	}

	#[test]
	fn vote_on_min_block_delta() {
		let t = vote_target(1u32, 1, 1, 4);
		assert_eq!(None, t);
		let t = vote_target(2u32, 1, 1, 4);
		assert_eq!(None, t);
		let t = vote_target(4u32, 2, 1, 4);
		assert_eq!(None, t);
		let t = vote_target(6u32, 2, 1, 4);
		assert_eq!(Some(6), t);

		let t = vote_target(9u32, 4, 1, 4);
		assert_eq!(Some(8), t);

		let t = vote_target(10u32, 10, 1, 8);
		assert_eq!(None, t);
		let t = vote_target(12u32, 10, 1, 8);
		assert_eq!(None, t);
		let t = vote_target(18u32, 10, 1, 8);
		assert_eq!(Some(18), t);
	}

	#[test]
	fn vote_on_power_of_two() {
		let t = vote_target(1008u32, 1000, 1, 4);
		assert_eq!(Some(1004), t);

		let t = vote_target(1016u32, 1000, 1, 4);
		assert_eq!(Some(1008), t);

		let t = vote_target(1032u32, 1000, 1, 4);
		assert_eq!(Some(1016), t);

		let t = vote_target(1064u32, 1000, 1, 4);
		assert_eq!(Some(1032), t);

		let t = vote_target(1128u32, 1000, 1, 4);
		assert_eq!(Some(1064), t);

		let t = vote_target(1256u32, 1000, 1, 4);
		assert_eq!(Some(1128), t);

		let t = vote_target(1512u32, 1000, 1, 4);
		assert_eq!(Some(1256), t);

		let t = vote_target(1024u32, 1, 1, 4);
		assert_eq!(Some(513), t);
	}

	#[test]
	fn vote_on_target_block() {
		let t = vote_target(1008u32, 1002, 1, 4);
		assert_eq!(Some(1006), t);
		let t = vote_target(1010u32, 1002, 1, 4);
		assert_eq!(Some(1006), t);

		let t = vote_target(1016u32, 1006, 1, 4);
		assert_eq!(Some(1014), t);
		let t = vote_target(1022u32, 1006, 1, 4);
		assert_eq!(Some(1014), t);

		let t = vote_target(1032u32, 1012, 1, 4);
		assert_eq!(Some(1028), t);
		let t = vote_target(1044u32, 1012, 1, 4);
		assert_eq!(Some(1028), t);

		let t = vote_target(1064u32, 1014, 1, 4);
		assert_eq!(Some(1046), t);
		let t = vote_target(1078u32, 1014, 1, 4);
		assert_eq!(Some(1046), t);

		let t = vote_target(1128u32, 1008, 1, 4);
		assert_eq!(Some(1072), t);
		let t = vote_target(1136u32, 1008, 1, 4);
		assert_eq!(Some(1072), t);
	}

	#[test]
	fn vote_on_mandatory_block() {
		let t = vote_target(1008u32, 1002, 1004, 4);
		assert_eq!(Some(1004), t);
		let t = vote_target(1016u32, 1006, 1007, 4);
		assert_eq!(Some(1007), t);
		let t = vote_target(1064u32, 1014, 1063, 4);
		assert_eq!(Some(1063), t);
		let t = vote_target(1320u32, 1012, 1234, 4);
		assert_eq!(Some(1234), t);

		let t = vote_target(1128u32, 1008, 1008, 4);
		assert_eq!(Some(1072), t);
	}

	#[test]
	fn should_vote_target() {
		let header = Header::new(
			1u32.into(),
			Default::default(),
			Default::default(),
			Default::default(),
			Digest::default(),
		);
		let mut oracle = VoterOracle::<Block, ecdsa_crypto::AuthorityId> {
			best_beefy_block: 0,
			best_grandpa_block_header: header,
			min_block_delta: 1,
			sessions: VecDeque::new(),
			_phantom: PhantomData,
		};
		let voting_target_with = |oracle: &mut VoterOracle<Block, ecdsa_crypto::AuthorityId>,
		                          best_beefy: NumberFor<Block>,
		                          best_grandpa: NumberFor<Block>|
		 -> Option<NumberFor<Block>> {
			oracle.best_beefy_block = best_beefy;
			oracle.best_grandpa_block_header.number = best_grandpa;
			oracle.voting_target()
		};

		// rounds not initialized -> should vote: `None`
		assert_eq!(voting_target_with(&mut oracle, 0, 1), None);

		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();

		oracle.add_session(Rounds::new(1, validator_set.clone()));

		// under min delta
		oracle.min_block_delta = 4;
		assert_eq!(voting_target_with(&mut oracle, 1, 1), None);
		assert_eq!(voting_target_with(&mut oracle, 2, 5), None);

		// vote on min delta
		assert_eq!(voting_target_with(&mut oracle, 4, 9), Some(8));
		oracle.min_block_delta = 8;
		assert_eq!(voting_target_with(&mut oracle, 10, 18), Some(18));

		// vote on power of two
		oracle.min_block_delta = 1;
		assert_eq!(voting_target_with(&mut oracle, 1000, 1008), Some(1004));
		assert_eq!(voting_target_with(&mut oracle, 1000, 1016), Some(1008));

		// nothing new to vote on
		assert_eq!(voting_target_with(&mut oracle, 1000, 1000), None);

		// vote on mandatory
		oracle.sessions.clear();
		oracle.add_session(Rounds::new(1000, validator_set.clone()));
		assert_eq!(voting_target_with(&mut oracle, 0, 1008), Some(1000));
		oracle.sessions.clear();
		oracle.add_session(Rounds::new(1001, validator_set.clone()));
		assert_eq!(voting_target_with(&mut oracle, 1000, 1008), Some(1001));
	}

	#[test]
	fn test_oracle_accepted_interval() {
		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();

		let header = Header::new(
			1u32.into(),
			Default::default(),
			Default::default(),
			Default::default(),
			Digest::default(),
		);
		let mut oracle = VoterOracle::<Block, ecdsa_crypto::AuthorityId> {
			best_beefy_block: 0,
			best_grandpa_block_header: header,
			min_block_delta: 1,
			sessions: VecDeque::new(),
			_phantom: PhantomData,
		};
		let accepted_interval_with =
			|oracle: &mut VoterOracle<Block, ecdsa_crypto::AuthorityId>,
			 best_grandpa: NumberFor<Block>|
			 -> Result<(NumberFor<Block>, NumberFor<Block>), Error> {
				oracle.best_grandpa_block_header.number = best_grandpa;
				oracle.accepted_interval()
			};

		// rounds not initialized -> should accept votes: `None`
		assert!(accepted_interval_with(&mut oracle, 1).is_err());

		let session_one = 1;
		oracle.add_session(Rounds::new(session_one, validator_set.clone()));
		// mandatory not done, only accept mandatory
		for i in 0..15 {
			assert_eq!(accepted_interval_with(&mut oracle, i), Ok((session_one, session_one)));
		}

		// add more sessions, nothing changes
		let session_two = 11;
		let session_three = 21;
		oracle.add_session(Rounds::new(session_two, validator_set.clone()));
		oracle.add_session(Rounds::new(session_three, validator_set.clone()));
		// mandatory not done, should accept mandatory for session_one
		for i in session_three..session_three + 15 {
			assert_eq!(accepted_interval_with(&mut oracle, i), Ok((session_one, session_one)));
		}

		// simulate finish mandatory for session one, prune oracle
		oracle.sessions.front_mut().unwrap().test_set_mandatory_done(true);
		oracle.try_prune();
		// session_one pruned, should accept mandatory for session_two
		for i in session_three..session_three + 15 {
			assert_eq!(accepted_interval_with(&mut oracle, i), Ok((session_two, session_two)));
		}

		// simulate finish mandatory for session two, prune oracle
		oracle.sessions.front_mut().unwrap().test_set_mandatory_done(true);
		oracle.try_prune();
		// session_two pruned, should accept mandatory for session_three
		for i in session_three..session_three + 15 {
			assert_eq!(accepted_interval_with(&mut oracle, i), Ok((session_three, session_three)));
		}

		// simulate finish mandatory for session three
		oracle.sessions.front_mut().unwrap().test_set_mandatory_done(true);
		// verify all other blocks in this session are now open to voting
		for i in session_three..session_three + 15 {
			assert_eq!(accepted_interval_with(&mut oracle, i), Ok((session_three, i)));
		}
		// pruning does nothing in this case
		oracle.try_prune();
		for i in session_three..session_three + 15 {
			assert_eq!(accepted_interval_with(&mut oracle, i), Ok((session_three, i)));
		}

		// adding new session automatically prunes "finalized" previous session
		let session_four = 31;
		oracle.add_session(Rounds::new(session_four, validator_set.clone()));
		assert_eq!(oracle.sessions.front().unwrap().session_start(), session_four);
		assert_eq!(
			accepted_interval_with(&mut oracle, session_four + 10),
			Ok((session_four, session_four))
		);
	}

	#[test]
	fn extract_authorities_change_digest() {
		let mut header = Header::new(
			1u32.into(),
			Default::default(),
			Default::default(),
			Default::default(),
			Digest::default(),
		);

		// verify empty digest shows nothing
		assert!(find_authorities_change::<Block, ecdsa_crypto::AuthorityId>(&header).is_none());

		let peers = &[Keyring::One, Keyring::Two];
		let id = 42;
		let validator_set = ValidatorSet::new(make_beefy_ids(peers), id).unwrap();
		header.digest_mut().push(DigestItem::Consensus(
			BEEFY_ENGINE_ID,
			ConsensusLog::<ecdsa_crypto::AuthorityId>::AuthoritiesChange(validator_set.clone())
				.encode(),
		));

		// verify validator set is correctly extracted from digest
		let extracted = find_authorities_change::<Block, ecdsa_crypto::AuthorityId>(&header);
		assert_eq!(extracted, Some(validator_set));
	}

	#[tokio::test]
	async fn should_finalize_correctly() {
		let keys = [Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(&keys), 0).unwrap();
		let mut net = BeefyTestNet::new(1);
		let backend = net.peer(0).client().as_backend();
		let mut worker = create_beefy_worker(net.peer(0), &keys[0], 1, validator_set.clone());
		// remove default session, will manually add custom one.
		worker.persisted_state.voting_oracle.sessions.clear();

		let keys = keys.iter().cloned().enumerate();
		let (mut best_block_streams, mut finality_proofs) =
			get_beefy_streams(&mut net, keys.clone());
		let mut best_block_stream = best_block_streams.drain(..).next().unwrap();
		let mut finality_proof = finality_proofs.drain(..).next().unwrap();

		let create_finality_proof = |block_num: NumberFor<Block>| {
			let commitment = Commitment {
				payload: Payload::from_single_entry(known_payloads::MMR_ROOT_ID, vec![]),
				block_number: block_num,
				validator_set_id: validator_set.id(),
			};
			VersionedFinalityProof::V1(SignedCommitment { commitment, signatures: vec![None] })
		};

		// no 'best beefy block' or finality proofs
		assert_eq!(worker.persisted_state.best_beefy(), 0);
		poll_fn(move |cx| {
			assert_eq!(best_block_stream.poll_next_unpin(cx), Poll::Pending);
			assert_eq!(finality_proof.poll_next_unpin(cx), Poll::Pending);
			Poll::Ready(())
		})
		.await;

		let client = net.peer(0).client().as_client();
		// unknown hash for block #1
		let (mut best_block_streams, mut finality_proofs) =
			get_beefy_streams(&mut net, keys.clone());
		let mut best_block_stream = best_block_streams.drain(..).next().unwrap();
		let mut finality_proof = finality_proofs.drain(..).next().unwrap();
		let justif = create_finality_proof(1);
		// create new session at block #1
		worker
			.persisted_state
			.voting_oracle
			.add_session(Rounds::new(1, validator_set.clone()));
		// try to finalize block #1
		worker.finalize(justif.clone()).unwrap();
		// verify block finalized
		assert_eq!(worker.persisted_state.best_beefy(), 1);
		poll_fn(move |cx| {
			// expect Some(hash-of-block-1)
			match best_block_stream.poll_next_unpin(cx) {
				Poll::Ready(Some(hash)) => {
					let block_num = client.number(hash).unwrap();
					assert_eq!(block_num, Some(1));
				},
				v => panic!("unexpected value: {:?}", v),
			}
			// commitment streamed
			match finality_proof.poll_next_unpin(cx) {
				// expect justification
				Poll::Ready(Some(received)) => assert_eq!(received, justif),
				v => panic!("unexpected value: {:?}", v),
			}
			Poll::Ready(())
		})
		.await;

		// generate 2 blocks, try again expect success
		let (mut best_block_streams, _) = get_beefy_streams(&mut net, keys);
		let mut best_block_stream = best_block_streams.drain(..).next().unwrap();
		let hashes = net.peer(0).push_blocks(1, false);
		// finalize 1 and 2 without justifications (hashes does not contain genesis)
		let hashof2 = hashes[0];
		backend.finalize_block(hashof2, None).unwrap();

		let justif = create_finality_proof(2);
		// create new session at block #2
		worker.persisted_state.voting_oracle.add_session(Rounds::new(2, validator_set));
		worker.finalize(justif).unwrap();
		// verify old session pruned
		assert_eq!(worker.voting_oracle().sessions.len(), 1);
		// new session starting at #2 is in front
		assert_eq!(worker.active_rounds().unwrap().session_start(), 2);
		// verify block finalized
		assert_eq!(worker.persisted_state.best_beefy(), 2);
		poll_fn(move |cx| {
			match best_block_stream.poll_next_unpin(cx) {
				// expect Some(hash-of-block-2)
				Poll::Ready(Some(hash)) => {
					let block_num = net.peer(0).client().as_client().number(hash).unwrap();
					assert_eq!(block_num, Some(2));
				},
				v => panic!("unexpected value: {:?}", v),
			}
			Poll::Ready(())
		})
		.await;

		// check BEEFY justifications are also appended to backend
		let justifs = backend.blockchain().justifications(hashof2).unwrap().unwrap();
		assert!(justifs.get(BEEFY_ENGINE_ID).is_some())
	}

	#[tokio::test]
	async fn should_init_session() {
		let keys = &[Keyring::Alice, Keyring::Bob];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();
		let mut net = BeefyTestNet::new(1);
		let mut worker = create_beefy_worker(net.peer(0), &keys[0], 1, validator_set.clone());

		let worker_rounds = worker.active_rounds().unwrap();
		assert_eq!(worker_rounds.session_start(), 1);
		assert_eq!(worker_rounds.validators(), validator_set.validators());
		assert_eq!(worker_rounds.validator_set_id(), validator_set.id());

		// new validator set
		let keys = &[Keyring::Bob];
		let new_validator_set = ValidatorSet::new(make_beefy_ids(keys), 1).unwrap();

		worker.init_session_at(new_validator_set.clone(), 11);
		// Since mandatory is not done for old rounds, we still get those.
		let rounds = worker.persisted_state.voting_oracle.active_rounds_mut().unwrap();
		assert_eq!(rounds.validator_set_id(), validator_set.id());
		// Let's finalize mandatory.
		rounds.test_set_mandatory_done(true);
		worker.persisted_state.voting_oracle.try_prune();
		// Now we should get the next round.
		let rounds = worker.active_rounds().unwrap();
		// Expect new values.
		assert_eq!(rounds.session_start(), 11);
		assert_eq!(rounds.validators(), new_validator_set.validators());
		assert_eq!(rounds.validator_set_id(), new_validator_set.id());
	}

	#[tokio::test]
	async fn should_not_report_bad_old_or_self_equivocations() {
		let block_num = 1;
		let set_id = 1;
		let keys = [Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(&keys), set_id).unwrap();
		// Alice votes on good MMR roots, equivocations are allowed/expected
		let mut api_alice = TestApi::with_validator_set(&validator_set);
		api_alice.allow_equivocations();
		let api_alice = Arc::new(api_alice);

		let mut net = BeefyTestNet::new(1);
		let mut worker = create_beefy_worker(net.peer(0), &keys[0], 1, validator_set.clone());
		worker.runtime = api_alice.clone();
		worker.fisherman = Arc::new(Fisherman::new(
			worker.backend.clone(),
			worker.runtime.clone(),
			worker.key_store.clone(),
		));

		// let there be a block with num = 1:
		let _ = net.peer(0).push_blocks(1, false);

		let payload1 = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
		let payload2 = Payload::from_single_entry(MMR_ROOT_ID, vec![128]);

		// generate an equivocation proof, with Bob as perpetrator
		let good_proof = generate_double_voting_proof(
			(block_num, payload1.clone(), set_id, &Keyring::Bob),
			(block_num, payload2.clone(), set_id, &Keyring::Bob),
		);
		{
			// expect voter (Alice) to successfully report it
			assert_eq!(worker.report_double_voting(good_proof.clone()), Ok(()));
			// verify Alice reports Bob equivocation to runtime
			let reported = api_alice.reported_equivocations.as_ref().unwrap().lock();
			assert_eq!(reported.len(), 1);
			assert_eq!(*reported.get(0).unwrap(), good_proof);
		}
		api_alice.reported_equivocations.as_ref().unwrap().lock().clear();

		// now let's try with a bad proof
		let mut bad_proof = good_proof.clone();
		bad_proof.first.id = Keyring::Charlie.public();
		// bad proofs are simply ignored
		assert_eq!(worker.report_double_voting(bad_proof), Ok(()));
		// verify nothing reported to runtime
		assert!(api_alice.reported_equivocations.as_ref().unwrap().lock().is_empty());

		// now let's try with old set it
		let mut old_proof = good_proof.clone();
		old_proof.first.commitment.validator_set_id = 0;
		old_proof.second.commitment.validator_set_id = 0;
		// old proofs are simply ignored
		assert_eq!(worker.report_double_voting(old_proof), Ok(()));
		// verify nothing reported to runtime
		assert!(api_alice.reported_equivocations.as_ref().unwrap().lock().is_empty());

		// now let's try reporting a self-equivocation
		let self_proof = generate_double_voting_proof(
			(block_num, payload1.clone(), set_id, &Keyring::Alice),
			(block_num, payload2.clone(), set_id, &Keyring::Alice),
		);
		// equivocations done by 'self' are simply ignored (not reported)
		assert_eq!(worker.report_double_voting(self_proof), Ok(()));
		// verify nothing reported to runtime
		assert!(api_alice.reported_equivocations.as_ref().unwrap().lock().is_empty());
	}
}
