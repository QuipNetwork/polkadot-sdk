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

//! The collation generation subsystem is the interface between polkadot and the collators.
//!
//! # Overview
//!
//! On every `ActiveLeavesUpdate`:
//!
//! # Two Modes of Operation
//!
//! The subsystem supports two distinct interfaces for receiving collations:
//!
//! ## 1. `CollatorFn` callback (legacy/simple interface)
//!
//! Configured via [`CollationGenerationMessage::Initialize`] with a [`CollatorFn`] callback.
//! The subsystem invokes this callback on each new relay chain head to request collations.
//!
//! - **Trigger**: `ActiveLeavesUpdate` signal with new relay parent
//! - **Flow**: Subsystem calls `CollatorFn(relay_parent, validation_data)` → receives `Collation`
//! - **Limitations**: Does not support V3 candidate descriptors because the interface has no way to
//!   specify a `scheduling_parent`. The `scheduling_parent` is always set to `None`, resulting in
//!   V2 descriptors where `relay_parent == scheduling_parent`.
//! - **Used by**: Test collators (adder, undying)
//!
//! ## 2. `SubmitSegment` message (full-featured interface)
//!
//! Collations are submitted directly via [`CollationGenerationMessage::SubmitSegment`]. A segment
//! is one or more collations that share a scheduling parent and target core; the collator builds
//! them and decides when to submit.
//!
//! - **Trigger**: Explicit `SubmitSegment` message from the collator
//! - **Flow**: Collator builds collations externally → sends `SubmitSegmentParams` → subsystem
//!   constructs one [`SegmentEntry`] per collation and distributes them in order as a [`Segment`].
//!   The collator protocol assembles the candidate receipts from the entry fields and the
//!   segment-level commons.
//! - **Descriptor version**: Selected explicitly via `candidates_descriptor_version` and reflected
//!   in the [`Segment`] arm. `V3` enables low-latency collation where the scheduling context (which
//!   relay block determined core assignment) differs from the relay parent (the block the parablock
//!   actually builds on).
//! - **Used by**: Production collators (cumulus slot-based, lookahead)
//!
//! # Candidate Descriptor Versions
//!
//! The descriptor version is selected via `candidates_descriptor_version` and is reflected in
//! the [`Segment`] arm:
//!
//! - **V2**: [`Segment::V2`], exactly one collation. The scheduling context implicitly equals the
//!   relay parent. The `CollatorFn` path only produces these.
//! - **V3**: [`Segment::V3`], one or more collations sharing a scheduling parent. V3 candidates
//!   require UMP signals to be present. Requires the `CandidateReceiptV3` node feature to be
//!   enabled.
//!
//! UMP-signal checks run on the commitments and descriptor fields directly
//! ([`parse_ump_signals_for_commitments`]);
//!
//! # Protocol Details
//!
//! On `ActiveLeavesUpdate` (only relevant for `CollatorFn` mode):
//!
//! 1. If no collation config or no `CollatorFn`, ignore.
//! 2. For each activated head:
//!    - Fetch claim queue to determine core assignments
//!    - Fetch validation data and code hash
//!    - Invoke `CollatorFn` for each assigned core
//!    - Construct a [`SegmentEntry`] and distribute it as a V2 segment via
//!      [`CollatorProtocolMessage::DistributeSegment`]
//!
//! On `SubmitSegment`:
//!
//! 1. Validate the subsystem is initialized
//! 2. Fetch claim queue and session info for the scheduling parent
//! 3. Construct one [`SegmentEntry`] per collation
//! 4. Distribute the segment via [`CollatorProtocolMessage::DistributeSegment`]
//!
//! [`CollatorFn`]: polkadot_node_primitives::CollatorFn
//! [`SubmitSegmentParams`]: polkadot_node_primitives::SubmitSegmentParams
//! [`CommittedCandidateReceiptV2`]: polkadot_primitives::CommittedCandidateReceiptV2

#![deny(missing_docs)]

use codec::Encode;
use error::{Error, Result};
use futures::{future::FutureExt, select};
use polkadot_node_primitives::{
	AvailableData, CollationGenerationConfig, PoV, SegmentCollation, SubmitSegmentParams,
	MAX_SEGMENT_LEN,
};
use polkadot_node_subsystem::{
	messages::{
		CollationGenerationMessage, CollatorProtocolMessage, RuntimeApiMessage, Segment,
		SegmentEntry,
	},
	overseer, ActiveLeavesUpdate, FromOrchestra, OverseerSignal, SpawnedSubsystem,
	SubsystemContext, SubsystemError, SubsystemResult, SubsystemSender,
};
use polkadot_node_subsystem_util::{
	request_claim_queue, request_persisted_validation_data, request_session_index_for_child,
	request_validation_code_hash, request_validators, runtime::ClaimQueueSnapshot,
};
use polkadot_primitives::{
	transpose_claim_queue, v9::parse_ump_signals_for_commitments, CandidateCommitments,
	CandidateDescriptorVersion, CoreIndex, Hash, Id as ParaId, OccupiedCoreAssumption,
	PersistedValidationData, SessionIndex, TransposedClaimQueue,
};
use schnellru::{ByLength, LruMap};
use sp_core::{bounded::BoundedVec, ConstU32};
use std::{collections::HashSet, sync::Arc};

mod error;

#[cfg(test)]
mod tests;

mod metrics;
use self::metrics::Metrics;

const LOG_TARGET: &'static str = "parachain::collation-generation";

/// Collation Generation Subsystem
pub struct CollationGenerationSubsystem {
	config: Option<Arc<CollationGenerationConfig>>,
	session_info_cache: SessionInfoCache,
	metrics: Metrics,
}

#[overseer::contextbounds(CollationGeneration, prefix = self::overseer)]
impl CollationGenerationSubsystem {
	/// Create a new instance of the `CollationGenerationSubsystem`.
	pub fn new(metrics: Metrics) -> Self {
		Self { config: None, metrics, session_info_cache: SessionInfoCache::new() }
	}

	/// Run this subsystem
	///
	/// Conceptually, this is very simple: it just loops forever.
	///
	/// - On incoming overseer messages, it starts or stops jobs as appropriate.
	/// - On other incoming messages, if they can be converted into `Job::ToJob` and include a hash,
	///   then they're forwarded to the appropriate individual job.
	/// - On outgoing messages from the jobs, it forwards them to the overseer.
	///
	/// If `err_tx` is not `None`, errors are forwarded onto that channel as they occur.
	/// Otherwise, most are logged and then discarded.
	async fn run<Context>(mut self, mut ctx: Context) {
		loop {
			select! {
				incoming = ctx.recv().fuse() => {
					if self.handle_incoming::<Context>(incoming, &mut ctx).await {
						break;
					}
				},
			}
		}
	}

	// handle an incoming message. return true if we should break afterwards.
	// note: this doesn't strictly need to be a separate function; it's more an administrative
	// function so that we don't clutter the run loop. It could in principle be inlined directly
	// into there. it should hopefully therefore be ok that it's an async function mutably borrowing
	// self.
	async fn handle_incoming<Context>(
		&mut self,
		incoming: SubsystemResult<FromOrchestra<<Context as SubsystemContext>::Message>>,
		ctx: &mut Context,
	) -> bool {
		match incoming {
			Ok(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated,
				..
			}))) => {
				if let Err(err) = self.handle_new_activation(activated.map(|v| v.hash), ctx).await {
					gum::warn!(target: LOG_TARGET, err = ?err, "failed to handle new activation");
				}

				false
			},
			Ok(FromOrchestra::Signal(OverseerSignal::Conclude)) => true,
			Ok(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(config),
			}) => {
				if self.config.is_some() {
					gum::error!(target: LOG_TARGET, "double initialization");
				} else {
					self.config = Some(Arc::new(config));
				}
				false
			},
			Ok(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Reinitialize(config),
			}) => {
				self.config = Some(Arc::new(config));
				false
			},
			Ok(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(params),
			}) => {
				if let Err(err) = self.handle_submit_segment(params, ctx).await {
					gum::error!(target: LOG_TARGET, ?err, "Failed to submit segment");
				}
				false
			},
			Ok(FromOrchestra::Signal(OverseerSignal::BlockFinalized(..))) => false,
			Err(err) => {
				gum::error!(
					target: LOG_TARGET,
					err = ?err,
					"error receiving message from subsystem context: {:?}",
					err
				);
				true
			},
		}
	}

	async fn handle_submit_segment<Context>(
		&mut self,
		params: SubmitSegmentParams,
		ctx: &mut Context,
	) -> Result<()> {
		let Some(config) = &self.config else {
			return Err(Error::SubmittedBeforeInit);
		};

		let _timer = self.metrics.time_submit_collation();
		if params.collations.is_empty() {
			return Err(Error::InvalidSegmentSize(params.collations.len()));
		}
		if params.candidates_descriptor_version == CandidateDescriptorVersion::V2 &&
			params.collations.len() > 1
		{
			return Err(Error::V2InvalidSegmentLength);
		}

		let scheduling_parent = params.scheduling_parent;
		let claim_queue = request_claim_queue(scheduling_parent, ctx.sender()).await.await??;

		let scheduling_session =
			request_session_index_for_child(scheduling_parent, ctx.sender()).await.await??;

		let session_info = self
			.session_info_cache
			.get(scheduling_parent, scheduling_session, ctx.sender())
			.await?;

		let transposed_queue = &transpose_claim_queue(claim_queue);
		let mut segment_entries = vec![];
		for submit_param in params.collations {
			let collation = PreparedCollation {
				base: submit_param,
				para_id: config.para_id,
				n_validators: session_info.n_validators,
				core_index: params.core_index,
			};
			let entry = construct_segment_entry(
				collation,
				&mut self.metrics,
				transposed_queue,
				params.candidates_descriptor_version,
			)?;
			segment_entries.push(entry);
		}
		let sender = ctx.sender();
		let len = segment_entries.len();
		let segment = match params.candidates_descriptor_version {
			CandidateDescriptorVersion::V2 => {
				// V2 was validated above to contain exactly one collation.
				let entry = segment_entries.pop().ok_or(Error::InvalidSegmentSize(0))?;
				Segment::V2(entry)
			},
			CandidateDescriptorVersion::V3 => Segment::V3 {
				scheduling_parent,
				scheduling_session,
				candidates: BoundedVec::<SegmentEntry, ConstU32<MAX_SEGMENT_LEN>>::try_from(
					segment_entries,
				)
				.map_err(|_| Error::InvalidSegmentSize(len))?,
			},
			CandidateDescriptorVersion::V1 | CandidateDescriptorVersion::Unknown(_) => {
				return Err(Error::UnsupportedDescriptorVersion)
			},
		};
		sender
			.send_message(CollatorProtocolMessage::DistributeSegment {
				core_index: params.core_index,
				para_id: config.para_id,
				segment,
			})
			.await;
		Ok(())
	}

	async fn handle_new_activation<Context>(
		&mut self,
		maybe_activated: Option<Hash>,
		ctx: &mut Context,
	) -> Result<()> {
		let Some(config) = &self.config else {
			return Ok(());
		};

		let Some(activated) = maybe_activated else { return Ok(()) };

		// If there is no collation function provided, bail out early.
		// Important: Lookahead collator and slot based collator do not use `CollatorFn`.
		if config.collator.is_none() {
			return Ok(());
		}

		let para_id = config.para_id;

		let _timer = self.metrics.time_new_activation();

		let session_index =
			request_session_index_for_child(activated, ctx.sender()).await.await??;

		let session_info =
			self.session_info_cache.get(activated, session_index, ctx.sender()).await?;
		let n_validators = session_info.n_validators;

		let claim_queue =
			ClaimQueueSnapshot::from(request_claim_queue(activated, ctx.sender()).await.await??);

		let assigned_cores = claim_queue
			.iter_all_claims()
			.filter_map(|(core_idx, para_ids)| {
				para_ids.iter().any(|&para_id| para_id == config.para_id).then_some(*core_idx)
			})
			.collect::<Vec<_>>();

		// Nothing to do if no core is assigned to us at any depth.
		if assigned_cores.is_empty() {
			return Ok(());
		}

		// We are being very optimistic here, but one of the cores could be pending availability
		// for some more blocks, or even time out. We assume all cores are being freed.

		let mut validation_data = match request_persisted_validation_data(
			activated,
			para_id,
			// Just use included assumption always. If there are no pending candidates it's a
			// no-op.
			OccupiedCoreAssumption::Included,
			ctx.sender(),
		)
		.await
		.await??
		{
			Some(v) => v,
			None => {
				gum::debug!(
					target: LOG_TARGET,
					relay_parent = ?activated,
					our_para = %para_id,
					"validation data is not available",
				);
				return Ok(());
			},
		};

		let validation_code_hash = match request_validation_code_hash(
			activated,
			para_id,
			// Just use included assumption always. If there are no pending candidates it's a
			// no-op.
			OccupiedCoreAssumption::Included,
			ctx.sender(),
		)
		.await
		.await??
		{
			Some(v) => v,
			None => {
				gum::debug!(
					target: LOG_TARGET,
					relay_parent = ?activated,
					our_para = %para_id,
					"validation code hash is not found.",
				);
				return Ok(());
			},
		};

		let task_config = config.clone();
		let metrics = self.metrics.clone();
		let mut task_sender = ctx.sender().clone();

		ctx.spawn(
			"chained-collation-builder",
			Box::pin(async move {
				let transposed_claim_queue = transpose_claim_queue(claim_queue.0.clone());

				// Track used core indexes not to submit collations on the same core.
				let mut used_cores = HashSet::new();

				for i in 0..assigned_cores.len() {
					// Get the collation.
					let collator_fn = match task_config.collator.as_ref() {
						Some(x) => x,
						None => return,
					};

					let collation = match collator_fn(activated, &validation_data).await {
						Some(collation_result) => collation_result.collation,
						None => {
							gum::debug!(
								target: LOG_TARGET,
								?para_id,
								"collator returned no collation on collate",
							);
							return;
						},
					};

					// Use the core_selector method from CandidateCommitments to extract
					// CoreSelector and ClaimQueueOffset.
					let mut commitments = CandidateCommitments::default();
					commitments.upward_messages = collation.upward_messages.clone();

					let ump_signals = match commitments.ump_signals() {
						Ok(signals) => signals,
						Err(err) => {
							gum::debug!(
								target: LOG_TARGET,
								?para_id,
								"error processing UMP signals: {}",
								err
							);
							return;
						},
					};

					let (cs_index, cq_offset) = ump_signals
						.core_selector()
						.map(|(cs_index, cq_offset)| (cs_index.0 as usize, cq_offset.0 as usize))
						.unwrap_or((i, 0));

					// Identify the cores to build collations on using the given claim queue offset.
					let cores_to_build_on = claim_queue
						.iter_claims_at_depth(cq_offset)
						.filter_map(|(core_idx, para_id)| {
							(para_id == task_config.para_id).then_some(core_idx)
						})
						.collect::<Vec<_>>();

					if cores_to_build_on.is_empty() {
						gum::debug!(
							target: LOG_TARGET,
							?para_id,
							"no core is assigned to para at depth {}",
							cq_offset,
						);
						return;
					}

					let descriptor_core_index =
						cores_to_build_on[cs_index % cores_to_build_on.len()];

					// Ensure the core index has not been used before.
					if used_cores.contains(&descriptor_core_index.0) {
						gum::warn!(
							target: LOG_TARGET,
							?para_id,
							"parachain repeatedly selected the same core index: {}",
							descriptor_core_index.0,
						);
						return;
					}

					used_cores.insert(descriptor_core_index.0);
					gum::trace!(
						target: LOG_TARGET,
						?para_id,
						"selected core index: {}",
						descriptor_core_index.0,
					);

					// Distribute the collation.
					let parent_head = collation.head_data.clone();
					// Note: CollatorFn-based collators don't support V3 scheduling,
					// so this path always produces V2 segments.
					if let Err(err) = construct_and_distribute_v2_receipt(
						PreparedCollation {
							base: SegmentCollation {
								collation,
								relay_parent: activated,
								validation_data: validation_data.clone(),
								validation_code_hash,
								session_index,
							},
							para_id,
							n_validators,
							core_index: descriptor_core_index,
						},
						&mut task_sender,
						&metrics,
						&transposed_claim_queue,
					)
					.await
					{
						gum::error!(
							target: LOG_TARGET,
							"Failed to construct and distribute collation: {}",
							err
						);
						return;
					}

					// Chain the collations. All else stays the same as we build the chained
					// collation on same relay parent.
					validation_data.parent_head = parent_head;
				}
			}),
		)?;

		Ok(())
	}
}

#[overseer::subsystem(CollationGeneration, error=SubsystemError, prefix=self::overseer)]
impl<Context> CollationGenerationSubsystem {
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = async move {
			self.run(ctx).await;
			Ok(())
		}
		.boxed();

		SpawnedSubsystem { name: "collation-generation-subsystem", future }
	}
}

#[derive(Clone)]
struct PerSessionInfo {
	n_validators: usize,
}

struct SessionInfoCache(LruMap<SessionIndex, PerSessionInfo>);

impl SessionInfoCache {
	fn new() -> Self {
		Self(LruMap::new(ByLength::new(2)))
	}

	async fn get<Sender: SubsystemSender<RuntimeApiMessage>>(
		&mut self,
		relay_parent: Hash,
		session_index: SessionIndex,
		sender: &mut Sender,
	) -> Result<PerSessionInfo> {
		if let Some(info) = self.0.get(&session_index) {
			return Ok(info.clone());
		}

		let n_validators =
			request_validators(relay_parent, &mut sender.clone()).await.await??.len();

		let info = PerSessionInfo { n_validators };
		self.0.insert(session_index, info);
		Ok(self.0.get(&session_index).expect("Just inserted").clone())
	}
}

struct PreparedCollation {
	base: SegmentCollation,
	para_id: ParaId,
	n_validators: usize,
	core_index: CoreIndex,
}

/// Construct a [`SegmentEntry`] from a prepared collation: compress the PoV, compute the
/// erasure root and run the UMP-signal checks on the commitments and descriptor fields.
/// The final `CandidateReceipt` is assembled by the receiver from these fields.
fn construct_segment_entry(
	collation: PreparedCollation,
	metrics: &Metrics,
	transposed_claim_queue: &TransposedClaimQueue,
	candidates_descriptor_version: CandidateDescriptorVersion,
) -> Result<SegmentEntry> {
	let PreparedCollation {
		base:
			SegmentCollation {
				collation,
				relay_parent,
				validation_data,
				validation_code_hash,
				session_index,
			},
		para_id,
		n_validators,
		core_index,
	} = collation;

	let persisted_validation_data_hash = validation_data.hash();
	let parent_head_data = validation_data.parent_head.clone();

	// Apply compression to the block data.
	let pov = {
		let pov = collation.proof_of_validity.into_compressed();
		let encoded_size = pov.encoded_size();

		// As long as `POV_BOMB_LIMIT` is at least `max_pov_size`, this ensures
		// that honest collators never produce a PoV which is uncompressed.
		//
		// As such, honest collators never produce an uncompressed PoV which starts with
		// a compression magic number, which would lead validators to reject the collation.
		if encoded_size > validation_data.max_pov_size as usize {
			return Err(Error::POVSizeExceeded(
				encoded_size,
				validation_data.max_pov_size as usize,
			));
		}

		pov
	};

	let erasure_root = erasure_root(n_validators, validation_data, pov.clone())?;

	let commitments = CandidateCommitments {
		upward_messages: collation.upward_messages,
		horizontal_messages: collation.horizontal_messages,
		new_validation_code: collation.new_validation_code,
		head_data: collation.head_data,
		processed_downward_messages: collation.processed_downward_messages,
		hrmp_watermark: collation.hrmp_watermark,
	};

	parse_ump_signals_for_commitments(
		&commitments,
		candidates_descriptor_version,
		transposed_claim_queue,
		para_id,
		core_index,
	)
	.map_err(Error::CandidateReceiptCheck)?;

	metrics.on_collation_generated();
	Ok(SegmentEntry {
		relay_parent,
		session_index,
		validation_code_hash,
		persisted_validation_data_hash,
		erasure_root,
		commitments_hash: commitments.hash(),
		output_head_data_hash: commitments.head_data.hash(),
		pov,
		parent_head_data,
	})
}

/// Takes a prepared collation and distributes it to validators as a
/// single-candidate V2 segment.
async fn construct_and_distribute_v2_receipt(
	collation: PreparedCollation,
	sender: &mut impl overseer::CollationGenerationSenderTrait,
	metrics: &Metrics,
	transposed_claim_queue: &TransposedClaimQueue,
) -> Result<()> {
	let para_id = collation.para_id;
	let core_index = collation.core_index;
	let segment_entry = construct_segment_entry(
		collation,
		metrics,
		transposed_claim_queue,
		CandidateDescriptorVersion::V2,
	)?;

	let segment = Segment::V2(segment_entry);
	sender
		.send_message(CollatorProtocolMessage::DistributeSegment { core_index, para_id, segment })
		.await;

	Ok(())
}

fn erasure_root(
	n_validators: usize,
	persisted_validation: PersistedValidationData,
	pov: PoV,
) -> Result<Hash> {
	let available_data =
		AvailableData { validation_data: persisted_validation, pov: Arc::new(pov) };

	let chunks = polkadot_erasure_coding::obtain_chunks_v1(n_validators, &available_data)?;
	Ok(polkadot_erasure_coding::branches(&chunks).root())
}
