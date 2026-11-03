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

use crate::task::{REGULAR_CHUNKS_REQ_RETRY_LIMIT, SYSTEMATIC_CHUNKS_REQ_RETRY_LIMIT};

use super::*;
use std::{result::Result, sync::Arc, time::Duration};

use assert_matches::assert_matches;
use futures::{executor, future};
use futures_timer::Delay;
use rstest::rstest;

use codec::Encode;
use polkadot_node_network_protocol::request_response::{
	self as req_res,
	v1::{AvailableDataFetchingRequest, ChunkResponse},
	IncomingRequest, Protocol, Recipient, ReqProtocolNames, Requests,
};

use polkadot_node_primitives::{BlockData, ErasureChunk, PoV, Proof};
use polkadot_node_subsystem::messages::{
	AllMessages, NetworkBridgeTxMessage, RuntimeApiMessage, RuntimeApiRequest,
};
use polkadot_node_subsystem_test_helpers::{
	derive_erasure_chunks_with_proofs_and_root, make_subsystem_context, mock::new_leaf,
	TestSubsystemContextHandle,
};
use polkadot_node_subsystem_util::TimeoutExt;
use polkadot_primitives::{
	node_features, ApprovalVotingParams, AuthorityDiscoveryId, Block, Hash, HeadData, IndexedVec,
	MutateDescriptorV2, NodeFeatures, PersistedValidationData, SessionInfo, ValidatorId,
};
use polkadot_primitives_test_helpers::{dummy_candidate_receipt, dummy_hash};
use sc_network::{IfDisconnected, OutboundFailure, ProtocolName, RequestFailure};
use sp_keyring::Sr25519Keyring;

type VirtualOverseer = TestSubsystemContextHandle<AvailabilityRecoveryMessage>;

// Implement some helper constructors for the AvailabilityRecoverySubsystem

/// Create a new instance of `AvailabilityRecoverySubsystem` which starts with a fast path to
/// request data from backers.
fn with_fast_path(
	req_receiver: IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	req_protocol_names: &ReqProtocolNames,
	metrics: Metrics,
) -> AvailabilityRecoverySubsystem {
	AvailabilityRecoverySubsystem::with_recovery_strategy_kind(
		req_receiver,
		req_protocol_names,
		metrics,
		RecoveryStrategyKind::BackersFirstAlways,
	)
}

/// Create a new instance of `AvailabilityRecoverySubsystem` which requests only chunks
fn with_chunks_only(
	req_receiver: IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	req_protocol_names: &ReqProtocolNames,
	metrics: Metrics,
) -> AvailabilityRecoverySubsystem {
	AvailabilityRecoverySubsystem::with_recovery_strategy_kind(
		req_receiver,
		req_protocol_names,
		metrics,
		RecoveryStrategyKind::ChunksAlways,
	)
}

/// Create a new instance of `AvailabilityRecoverySubsystem` which requests chunks if PoV is
/// above a threshold.
fn with_chunks_if_pov_large(
	req_receiver: IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	req_protocol_names: &ReqProtocolNames,
	metrics: Metrics,
) -> AvailabilityRecoverySubsystem {
	AvailabilityRecoverySubsystem::with_recovery_strategy_kind(
		req_receiver,
		req_protocol_names,
		metrics,
		RecoveryStrategyKind::BackersFirstIfSizeLower(FETCH_CHUNKS_THRESHOLD),
	)
}

/// Create a new instance of `AvailabilityRecoverySubsystem` which requests systematic chunks if
/// PoV is above a threshold.
fn with_systematic_chunks_if_pov_large(
	req_receiver: IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	req_protocol_names: &ReqProtocolNames,
	metrics: Metrics,
) -> AvailabilityRecoverySubsystem {
	AvailabilityRecoverySubsystem::for_validator(
		Some(FETCH_CHUNKS_THRESHOLD),
		req_receiver,
		req_protocol_names,
		metrics,
	)
}

/// Create a new instance of `AvailabilityRecoverySubsystem` which first requests full data
/// from backers, with a fallback to recover from systematic chunks.
fn with_fast_path_then_systematic_chunks(
	req_receiver: IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	req_protocol_names: &ReqProtocolNames,
	metrics: Metrics,
) -> AvailabilityRecoverySubsystem {
	AvailabilityRecoverySubsystem::with_recovery_strategy_kind(
		req_receiver,
		req_protocol_names,
		metrics,
		RecoveryStrategyKind::BackersThenSystematicChunks,
	)
}

/// Create a new instance of `AvailabilityRecoverySubsystem` which first attempts to request
/// systematic chunks, with a fallback to requesting regular chunks.
fn with_systematic_chunks(
	req_receiver: IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	req_protocol_names: &ReqProtocolNames,
	metrics: Metrics,
) -> AvailabilityRecoverySubsystem {
	AvailabilityRecoverySubsystem::with_recovery_strategy_kind(
		req_receiver,
		req_protocol_names,
		metrics,
		RecoveryStrategyKind::SystematicChunks,
	)
}

// Deterministic genesis hash for protocol names
const GENESIS_HASH: Hash = Hash::repeat_byte(0xff);

fn request_receiver(
	req_protocol_names: &ReqProtocolNames,
) -> IncomingRequestReceiver<AvailableDataFetchingRequest> {
	let receiver = IncomingRequest::get_config_receiver::<
		Block,
		sc_network::NetworkWorker<Block, Hash>,
	>(req_protocol_names);
	// Don't close the sending end of the request protocol. Otherwise, the subsystem will terminate.
	std::mem::forget(receiver.1.inbound_queue);
	receiver.0
}

fn test_harness<Fut: Future<Output = VirtualOverseer>>(
	subsystem: AvailabilityRecoverySubsystem,
	test: impl FnOnce(VirtualOverseer) -> Fut,
) {
	sp_tracing::init_for_tests();

	let pool = sp_core::testing::TaskExecutor::new();

	let (context, virtual_overseer) = make_subsystem_context(pool.clone());

	let subsystem = async {
		subsystem.run(context).await.unwrap();
	};

	let test_fut = test(virtual_overseer);

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);

	executor::block_on(future::join(
		async move {
			let mut overseer = test_fut.await;
			overseer_signal(&mut overseer, OverseerSignal::Conclude).await;
		},
		subsystem,
	))
	.1
}

const TIMEOUT: Duration = Duration::from_millis(300);

macro_rules! delay {
	($delay:expr) => {
		Delay::new(Duration::from_millis($delay)).await;
	};
}

async fn overseer_signal(
	overseer: &mut TestSubsystemContextHandle<AvailabilityRecoveryMessage>,
	signal: OverseerSignal,
) {
	delay!(50);
	overseer
		.send(FromOrchestra::Signal(signal))
		.timeout(TIMEOUT)
		.await
		.unwrap_or_else(|| {
			panic!("{}ms is more than enough for sending signals.", TIMEOUT.as_millis())
		});
}

async fn overseer_send(
	overseer: &mut TestSubsystemContextHandle<AvailabilityRecoveryMessage>,
	msg: AvailabilityRecoveryMessage,
) {
	gum::trace!(msg = ?msg, "sending message");
	overseer
		.send(FromOrchestra::Communication { msg })
		.timeout(TIMEOUT)
		.await
		.unwrap_or_else(|| {
			panic!("{}ms is more than enough for sending messages.", TIMEOUT.as_millis())
		});
}

async fn overseer_recv(
	overseer: &mut TestSubsystemContextHandle<AvailabilityRecoveryMessage>,
) -> AllMessages {
	gum::trace!("waiting for message ...");
	let msg = overseer.recv().timeout(TIMEOUT).await.expect("TIMEOUT is enough to recv.");
	gum::trace!(msg = ?msg, "received message");
	msg
}

#[derive(Debug)]
enum Has {
	No,
	Yes,
	NetworkError(RequestFailure),
	/// Make request not return at all, instead the sender is returned from the function.
	///
	/// Note, if you use `DoesNotReturn` you have to keep the returned senders alive, otherwise the
	/// subsystem will receive a cancel event and the request actually does return.
	DoesNotReturn,
}

impl Has {
	fn timeout() -> Self {
		Has::NetworkError(RequestFailure::Network(OutboundFailure::Timeout))
	}
}

#[derive(Clone)]
struct TestState {
	validators: Vec<Sr25519Keyring>,
	validator_public: IndexedVec<ValidatorIndex, ValidatorId>,
	validator_authority_id: Vec<AuthorityDiscoveryId>,
	validator_groups: IndexedVec<GroupIndex, Vec<ValidatorIndex>>,
	current: Hash,
	candidate: CandidateReceipt,
	session_index: SessionIndex,
	core_index: CoreIndex,
	node_features: NodeFeatures,

	persisted_validation_data: PersistedValidationData,

	available_data: AvailableData,
	chunks: IndexedVec<ValidatorIndex, ErasureChunk>,
	invalid_chunks: IndexedVec<ValidatorIndex, ErasureChunk>,
}

impl TestState {
	fn new(node_features: NodeFeatures) -> Self {
		let validators = vec![
			Sr25519Keyring::Ferdie, // <- this node, role: validator
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::One,
			Sr25519Keyring::Two,
		];

		let validator_public = validator_pubkeys(&validators);
		let validator_authority_id = validator_authority_id(&validators);
		let validator_groups = vec![
			vec![1.into(), 0.into(), 3.into(), 4.into()],
			vec![5.into(), 6.into()],
			vec![2.into()],
		];

		let current = Hash::repeat_byte(1);

		let mut candidate = dummy_candidate_receipt(dummy_hash());

		let session_index = 10;

		let persisted_validation_data = PersistedValidationData {
			parent_head: HeadData(vec![7, 8, 9]),
			relay_parent_number: Default::default(),
			max_pov_size: 1024,
			relay_parent_storage_root: Default::default(),
		};

		let pov = PoV { block_data: BlockData(vec![42; 64]) };

		let available_data = AvailableData {
			validation_data: persisted_validation_data.clone(),
			pov: Arc::new(pov),
		};

		let core_index = CoreIndex(2);

		let (chunks, erasure_root) = derive_erasure_chunks_with_proofs_and_root(
			validators.len(),
			&available_data,
			|_, _| {},
		);
		let chunks = map_chunks(chunks, &node_features, validators.len(), core_index);

		// Mess around:
		let invalid_chunks = chunks
			.iter()
			.cloned()
			.map(|mut chunk| {
				if chunk.chunk.len() >= 2 && chunk.chunk[0] != chunk.chunk[1] {
					chunk.chunk[0] = chunk.chunk[1];
				} else if chunk.chunk.len() >= 1 {
					chunk.chunk[0] = !chunk.chunk[0];
				} else {
					chunk.proof = Proof::dummy_proof();
				}
				chunk
			})
			.collect();
		debug_assert_ne!(chunks, invalid_chunks);

		candidate.descriptor.erasure_root = erasure_root;
		candidate.descriptor.relay_parent = Hash::repeat_byte(10);
		candidate.descriptor.pov_hash = Hash::repeat_byte(3);

		Self {
			validators,
			validator_public,
			validator_authority_id,
			validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::try_from(
				validator_groups,
			)
			.unwrap(),
			current,
			candidate: candidate.into(),
			session_index,
			core_index,
			node_features,
			persisted_validation_data,
			available_data,
			chunks,
			invalid_chunks,
		}
	}

	fn with_empty_node_features() -> Self {
		Self::new(NodeFeatures::EMPTY)
	}

	fn threshold(&self) -> usize {
		recovery_threshold(self.validators.len()).unwrap()
	}

	fn systematic_threshold(&self) -> usize {
		systematic_recovery_threshold(self.validators.len()).unwrap()
	}

	fn impossibility_threshold(&self) -> usize {
		self.validators.len() - self.threshold() + 1
	}

	async fn test_runtime_api_session_info(&self, virtual_overseer: &mut VirtualOverseer) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionInfo(
					session_index,
					tx,
				)
			)) => {
				assert_eq!(relay_parent, self.current);
				assert_eq!(session_index, self.session_index);

				tx.send(Ok(Some(SessionInfo {
					validators: self.validator_public.clone(),
					discovery_keys: self.validator_authority_id.clone(),
					validator_groups: self.validator_groups.clone(),
					assignment_keys: vec![],
					n_cores: 0,
					zeroth_delay_tranche_width: 0,
					relay_vrf_modulo_samples: 0,
					n_delay_tranches: 0,
					no_show_slots: 0,
					needed_approvals: 0,
					active_validator_indices: vec![],
					dispute_period: 6,
					random_seed: [0u8; 32],
				}))).unwrap();
			}
		);
	}

	async fn test_runtime_api_node_features(&self, virtual_overseer: &mut VirtualOverseer) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_relay_parent,
				RuntimeApiRequest::NodeFeatures(
					_,
					tx,
				)
			)) => {
				tx.send(Ok(
					self.node_features.clone()
				)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_relay_parent,
				RuntimeApiRequest::ApprovalVotingParams(
					_,
					tx,
				)
			)) => {
				tx.send(Ok(ApprovalVotingParams::default())).unwrap();
			}
		);
	}

	async fn respond_to_available_data_query(
		&self,
		virtual_overseer: &mut VirtualOverseer,
		with_data: bool,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::QueryAvailableData(_, tx)
			) => {
				let _ = tx.send(if with_data {
					Some(self.available_data.clone())
				} else {
					gum::debug!("Sending None");
					None
				});
			}
		)
	}

	async fn respond_to_query_all_request(
		&self,
		virtual_overseer: &mut VirtualOverseer,
		send_chunk: impl Fn(ValidatorIndex) -> bool,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::QueryAllChunks(_, tx)
			) => {
				let v = self.chunks.iter().enumerate()
					.filter_map(|(val_idx, c)| if send_chunk(ValidatorIndex(val_idx as u32)) {
						Some((ValidatorIndex(val_idx as u32), c.clone()))
					} else {
						None
					})
					.collect();

				let _ = tx.send(v);
			}
		)
	}

	async fn respond_to_query_all_request_invalid(
		&self,
		virtual_overseer: &mut VirtualOverseer,
		send_chunk: impl Fn(ValidatorIndex) -> bool,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::QueryAllChunks(_, tx)
			) => {
				let v = self.invalid_chunks.iter().enumerate()
					.filter_map(|(val_idx, c)| if send_chunk(ValidatorIndex(val_idx as u32)) {
						Some((ValidatorIndex(val_idx as u32), c.clone()))
					} else {
						None
					})
					.collect();

				let _ = tx.send(v);
			}
		)
	}

	async fn test_chunk_requests_inner(
		&self,
		req_protocol_names: &ReqProtocolNames,
		candidate_hash: CandidateHash,
		virtual_overseer: &mut VirtualOverseer,
		n: usize,
		mut who_has: impl FnMut(ValidatorIndex) -> Has,
		systematic_recovery: bool,
		protocol: Protocol,
	) -> Vec<oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>> {
		// arbitrary order.
		let mut i = 0;
		let mut senders = Vec::new();
		while i < n {
			// Receive a request for a chunk.
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridgeTx(
					NetworkBridgeTxMessage::SendRequests(
						requests,
						_if_disconnected,
					)
				) => {
					for req in requests {
						i += 1;
						assert_matches!(
							req,
							Requests::ChunkFetching(req) => {
								assert_eq!(req.payload.candidate_hash, candidate_hash);

								let validator_index = req.payload.index;
								let chunk = self.chunks.get(validator_index).unwrap().clone();

								if systematic_recovery {
									assert!(chunk.index.0 as usize <= self.systematic_threshold(), "requested non-systematic chunk");
								}

								let available_data = match who_has(validator_index) {
									Has::No => Ok(None),
									Has::Yes => Ok(Some(chunk)),
									Has::NetworkError(e) => Err(e),
									Has::DoesNotReturn => {
										senders.push(req.pending_response);
										continue
									}
								};

								// Recovery cancels the requests it no longer needs as soon as it has
								// enough chunks to reconstruct, dropping the receiver. Answering a
								// request that has already been cancelled is not a failure.
								let _ = req.pending_response.send(
									available_data.map(|r|
										(
											match protocol {
												Protocol::ChunkFetchingV1 =>
													match r {
														None => req_res::v1::ChunkFetchingResponse::NoSuchChunk,
														Some(c) => req_res::v1::ChunkFetchingResponse::Chunk(
															ChunkResponse {
																chunk: c.chunk,
																proof: c.proof
															}
														)
													}.encode(),
												Protocol::ChunkFetchingV2 =>
													req_res::v2::ChunkFetchingResponse::from(r).encode(),

												_ => unreachable!()
											},
											req_protocol_names.get_name(protocol)
										)
									)
								);
							}
						)
					}
				}
			);
		}
		senders
	}

	async fn test_chunk_requests(
		&self,
		req_protocol_names: &ReqProtocolNames,
		candidate_hash: CandidateHash,
		virtual_overseer: &mut VirtualOverseer,
		n: usize,
		who_has: impl FnMut(ValidatorIndex) -> Has,
		systematic_recovery: bool,
	) -> Vec<oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>> {
		self.test_chunk_requests_inner(
			req_protocol_names,
			candidate_hash,
			virtual_overseer,
			n,
			who_has,
			systematic_recovery,
			Protocol::ChunkFetchingV2,
		)
		.await
	}

	// Use legacy network protocol version.
	async fn test_chunk_requests_v1(
		&self,
		req_protocol_names: &ReqProtocolNames,
		candidate_hash: CandidateHash,
		virtual_overseer: &mut VirtualOverseer,
		n: usize,
		who_has: impl FnMut(ValidatorIndex) -> Has,
		systematic_recovery: bool,
	) -> Vec<oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>> {
		self.test_chunk_requests_inner(
			req_protocol_names,
			candidate_hash,
			virtual_overseer,
			n,
			who_has,
			systematic_recovery,
			Protocol::ChunkFetchingV1,
		)
		.await
	}

	async fn test_full_data_requests(
		&self,
		req_protocol_names: &ReqProtocolNames,
		candidate_hash: CandidateHash,
		virtual_overseer: &mut VirtualOverseer,
		who_has: impl Fn(usize) -> Has,
		group_index: GroupIndex,
	) -> Vec<oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>> {
		let mut senders = Vec::new();
		let expected_validators = self.validator_groups.get(group_index).unwrap();
		for _ in 0..expected_validators.len() {
			// Receive a request for the full `AvailableData`.
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridgeTx(
					NetworkBridgeTxMessage::SendRequests(
						mut requests,
						IfDisconnected::ImmediateError,
					)
				) => {
					assert_eq!(requests.len(), 1);

					assert_matches!(
						requests.pop().unwrap(),
						Requests::AvailableDataFetchingV1(req) => {
							assert_eq!(req.payload.candidate_hash, candidate_hash);
							let validator_index = self.validator_authority_id
								.iter()
								.position(|a| Recipient::Authority(a.clone()) == req.peer)
								.unwrap();
							assert!(expected_validators.contains(&ValidatorIndex(validator_index as u32)));

							let available_data = match who_has(validator_index) {
								Has::No => Ok(None),
								Has::Yes => Ok(Some(self.available_data.clone())),
								Has::NetworkError(e) => Err(e),
								Has::DoesNotReturn => {
									senders.push(req.pending_response);
									continue
								}
							};

							let done = available_data.as_ref().ok().map_or(false, |x| x.is_some());

							let _ = req.pending_response.send(
								available_data.map(|r|(
									req_res::v1::AvailableDataFetchingResponse::from(r).encode(),
									req_protocol_names.get_name(Protocol::AvailableDataFetchingV1)
								))
							);

							if done { break }
						}
					)
				}
			);
		}
		senders
	}
}

impl Default for TestState {
	fn default() -> Self {
		// Enable the chunk mapping node feature.
		let mut node_features = NodeFeatures::new();
		node_features
			.resize(node_features::FeatureIndex::AvailabilityChunkMapping as usize + 1, false);
		node_features
			.set(node_features::FeatureIndex::AvailabilityChunkMapping as u8 as usize, true);

		Self::new(node_features)
	}
}

fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> IndexedVec<ValidatorIndex, ValidatorId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

pub fn validator_authority_id(val_ids: &[Sr25519Keyring]) -> Vec<AuthorityDiscoveryId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

/// Map the chunks to the validators according to the availability chunk mapping algorithm.
fn map_chunks(
	chunks: Vec<ErasureChunk>,
	node_features: &NodeFeatures,
	n_validators: usize,
	core_index: CoreIndex,
) -> IndexedVec<ValidatorIndex, ErasureChunk> {
	let chunk_indices =
		availability_chunk_indices(node_features, n_validators, core_index).unwrap();

	(0..n_validators)
		.map(|val_idx| chunks[chunk_indices[val_idx].0 as usize].clone())
		.collect::<Vec<_>>()
		.into()
}

#[rstest]
#[case(true)]
#[case(false)]
fn availability_is_recovered_from_chunks_if_no_group_provided(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_fast_path_then_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_fast_path(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold,
				|_| Has::Yes,
				systematic_recovery,
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);

		let (tx, rx) = oneshot::channel();

		// Test another candidate, send no chunks.
		let mut new_candidate = dummy_candidate_receipt(dummy_hash());

		new_candidate.descriptor.relay_parent = test_state.candidate.descriptor.relay_parent();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				new_candidate.clone().into(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		if systematic_recovery {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					threshold,
					|_| Has::No,
					systematic_recovery,
				)
				.await;
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		// Even if the recovery is systematic, we'll always fall back to regular recovery, so keep
		// this around.
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				new_candidate.hash(),
				&mut virtual_overseer,
				test_state.impossibility_threshold(),
				|_| Has::No,
				false,
			)
			.await;

		// A request times out with `Unavailable` error.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn availability_is_recovered_from_chunks_even_if_backing_group_supplied_if_chunks_only(
	#[case] systematic_recovery: bool,
) {
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let test_state = TestState::default();
	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_chunks_only(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold,
				|_| Has::Yes,
				systematic_recovery,
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);

		let (tx, rx) = oneshot::channel();

		// Test another candidate, send no chunks.
		let mut new_candidate = dummy_candidate_receipt(dummy_hash());

		new_candidate.descriptor.relay_parent = test_state.candidate.descriptor.relay_parent();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				new_candidate.clone().into(),
				test_state.session_index,
				Some(GroupIndex(1)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		if systematic_recovery {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					threshold * SYSTEMATIC_CHUNKS_REQ_RETRY_LIMIT as usize,
					|_| Has::No,
					systematic_recovery,
				)
				.await;
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
			// Even if the recovery is systematic, we'll always fall back to regular recovery, so
			// keep this around.
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					test_state.impossibility_threshold() - threshold,
					|_| Has::No,
					false,
				)
				.await;

			// A request times out with `Unavailable` error.
			assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		} else {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					test_state.impossibility_threshold(),
					|_| Has::No,
					false,
				)
				.await;

			// A request times out with `Unavailable` error.
			assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		}
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn bad_merkle_path_leads_to_recovery_error(#[case] systematic_recovery: bool) {
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let mut test_state = TestState::default();
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		// Create some faulty chunks.
		for chunk in test_state.chunks.iter_mut() {
			chunk.chunk = vec![0; 32];
		}

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		if systematic_recovery {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					candidate_hash,
					&mut virtual_overseer,
					test_state.systematic_threshold(),
					|_| Has::No,
					systematic_recovery,
				)
				.await;
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.impossibility_threshold(),
				|_| Has::Yes,
				false,
			)
			.await;

		// A request times out with `Unavailable` error.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn wrong_chunk_index_leads_to_recovery_error(#[case] systematic_recovery: bool) {
	let mut test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		// Chunks should fail the index check as they don't have the correct index.

		// *(test_state.chunks.get_mut(0.into()).unwrap()) =
		// 	test_state.chunks.get(1.into()).unwrap().clone();
		let first_chunk = test_state.chunks.get(0.into()).unwrap().clone();
		for c_index in 1..test_state.chunks.len() {
			*(test_state.chunks.get_mut(ValidatorIndex(c_index as u32)).unwrap()) =
				first_chunk.clone();
		}

		if systematic_recovery {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					candidate_hash,
					&mut virtual_overseer,
					test_state.systematic_threshold(),
					|_| Has::Yes,
					// We set this to false, as we know we will be requesting the wrong indices.
					false,
				)
				.await;

			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.chunks.len() - 1,
				|_| Has::Yes,
				false,
			)
			.await;

		// A request times out with `Unavailable` error as there are no good peers.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn invalid_erasure_coding_leads_to_invalid_error(#[case] systematic_recovery: bool) {
	let mut test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_fast_path_then_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_fast_path(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		let pov = PoV { block_data: BlockData(vec![69; 64]) };

		let (bad_chunks, bad_erasure_root) = derive_erasure_chunks_with_proofs_and_root(
			test_state.chunks.len(),
			&AvailableData {
				validation_data: test_state.persisted_validation_data.clone(),
				pov: Arc::new(pov),
			},
			|i, chunk| *chunk = vec![i as u8; 32],
		);

		test_state.chunks = map_chunks(
			bad_chunks,
			&test_state.node_features,
			test_state.validators.len(),
			test_state.core_index,
		);
		test_state.candidate.descriptor.set_erasure_root(bad_erasure_root);

		let candidate_hash = test_state.candidate.hash();

		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold,
				|_| Has::Yes,
				systematic_recovery,
			)
			.await;

		// f+1 'valid' chunks can't produce correct data.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Invalid);
		virtual_overseer
	});
}

#[test]
fn invalid_pov_hash_leads_to_invalid_error() {
	let mut test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = AvailabilityRecoverySubsystem::for_collator(
		None,
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		let pov = PoV { block_data: BlockData(vec![69; 64]) };

		test_state.candidate.descriptor.set_pov_hash(pov.hash());

		let candidate_hash = test_state.candidate.hash();

		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.threshold(),
				|_| Has::Yes,
				false,
			)
			.await;

		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Invalid);
		virtual_overseer
	});
}

#[test]
fn fast_path_backing_group_recovers() {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = with_fast_path(
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;
		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		let who_has = |i| match i {
			3 => Has::Yes,
			_ => Has::No,
		};

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;

		test_state
			.test_full_data_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				who_has,
				GroupIndex(0),
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true, false)]
#[case(false, true)]
#[case(false, false)]
fn recovers_from_only_chunks_if_pov_large(
	#[case] systematic_recovery: bool,
	#[case] for_collator: bool,
) {
	let mut test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (subsystem, threshold) = match (systematic_recovery, for_collator) {
		(true, false) => (
			with_systematic_chunks_if_pov_large(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		(false, false) => (
			with_chunks_if_pov_large(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
		(false, true) => {
			test_state
				.candidate
				.descriptor
				.set_pov_hash(test_state.available_data.pov.hash());
			(
				AvailabilityRecoverySubsystem::for_collator(
					None,
					request_receiver(&req_protocol_names),
					&req_protocol_names,
					Metrics::new_dummy(),
				),
				test_state.threshold(),
			)
		},
		(_, _) => unreachable!(),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::QueryChunkSize(_, tx)
			) => {
				let _ = tx.send(Some(crate::FETCH_CHUNKS_THRESHOLD + 1));
			}
		);

		if !for_collator {
			test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold,
				|_| Has::Yes,
				systematic_recovery,
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);

		let (tx, rx) = oneshot::channel();

		// Test another candidate, send no chunks.
		let mut new_candidate = dummy_candidate_receipt(dummy_hash());

		new_candidate.descriptor.relay_parent = test_state.candidate.descriptor.relay_parent();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				new_candidate.clone().into(),
				test_state.session_index,
				Some(GroupIndex(1)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::QueryChunkSize(_, tx)
			) => {
				let _ = tx.send(Some(crate::FETCH_CHUNKS_THRESHOLD + 1));
			}
		);

		if !for_collator {
			test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		if systematic_recovery {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					test_state.systematic_threshold() * SYSTEMATIC_CHUNKS_REQ_RETRY_LIMIT as usize,
					|_| Has::No,
					systematic_recovery,
				)
				.await;
			if !for_collator {
				test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
			}
			// Even if the recovery is systematic, we'll always fall back to regular recovery.
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					test_state.impossibility_threshold() - threshold,
					|_| Has::No,
					false,
				)
				.await;
		} else {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					new_candidate.hash(),
					&mut virtual_overseer,
					test_state.impossibility_threshold(),
					|_| Has::No,
					false,
				)
				.await;
		}

		// A request times out with `Unavailable` error.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[rstest]
#[case(true, false)]
#[case(false, true)]
#[case(false, false)]
fn fast_path_backing_group_recovers_if_pov_small(
	#[case] systematic_recovery: bool,
	#[case] for_collator: bool,
) {
	let mut test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);

	let subsystem = match (systematic_recovery, for_collator) {
		(true, false) => with_systematic_chunks_if_pov_large(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),

		(false, false) => with_chunks_if_pov_large(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		(false, true) => {
			test_state
				.candidate
				.descriptor
				.set_pov_hash(test_state.available_data.pov.hash());
			AvailabilityRecoverySubsystem::for_collator(
				None,
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			)
		},
		(_, _) => unreachable!(),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		let who_has = |i| match i {
			3 => Has::Yes,
			_ => Has::No,
		};

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::QueryChunkSize(_, tx)
			) => {
				let _ = tx.send(Some(100));
			}
		);

		if !for_collator {
			test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		}

		test_state
			.test_full_data_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				who_has,
				GroupIndex(0),
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn no_answers_in_fast_path_causes_chunk_requests(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);

	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_fast_path_then_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_fast_path(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		// mix of timeout and no.
		let who_has = |i| match i {
			0 | 3 => Has::No,
			_ => Has::timeout(),
		};

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;

		test_state
			.test_full_data_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				who_has,
				GroupIndex(0),
			)
			.await;

		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold,
				|_| Has::Yes,
				systematic_recovery,
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn task_canceled_when_receivers_dropped(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);

	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, _) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		for _ in 0..test_state.validators.len() {
			match virtual_overseer.recv().timeout(TIMEOUT).await {
				None => return virtual_overseer,
				Some(_) => continue,
			}
		}

		panic!("task requested all validators without concluding")
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn chunks_retry_until_all_nodes_respond(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		if systematic_recovery {
			for _ in 0..SYSTEMATIC_CHUNKS_REQ_RETRY_LIMIT {
				test_state
					.test_chunk_requests(
						&req_protocol_names,
						candidate_hash,
						&mut virtual_overseer,
						test_state.systematic_threshold(),
						|_| Has::timeout(),
						true,
					)
					.await;
			}
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.impossibility_threshold(),
				|_| Has::timeout(),
				false,
			)
			.await;

		// We get to go another round! Actually, we get to go `REGULAR_CHUNKS_REQ_RETRY_LIMIT`
		// number of times.
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.impossibility_threshold(),
				|_| Has::No,
				false,
			)
			.await;

		// Recovery is impossible.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[test]
fn network_bridge_not_returning_responses_wont_stall_retrieval() {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = with_chunks_only(
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		// How many validators should not respond at all:
		let not_returning_count = 1;

		// Not returning senders won't cause the retrieval to stall:
		let _senders = test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				not_returning_count,
				|_| Has::DoesNotReturn,
				false,
			)
			.await;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				// Should start over:
				test_state.validators.len() + 3,
				|_| Has::timeout(),
				false,
			)
			.await;

		// we get to go another round!
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.threshold(),
				|_| Has::Yes,
				false,
			)
			.await;

		// Recovered data should match the original one:
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn all_not_returning_requests_still_recovers_on_return(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		let n = if systematic_recovery {
			test_state.systematic_threshold()
		} else {
			test_state.validators.len()
		};

		let senders = test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				n,
				|_| Has::DoesNotReturn,
				systematic_recovery,
			)
			.await;

		future::join(
			async {
				Delay::new(Duration::from_millis(10)).await;
				// Now retrieval should be able progress.
				std::mem::drop(senders);
			},
			async {
				test_state
					.test_chunk_requests(
						&req_protocol_names,
						candidate_hash,
						&mut virtual_overseer,
						// Should start over:
						n,
						|_| Has::timeout(),
						systematic_recovery,
					)
					.await
			},
		)
		.await;

		if systematic_recovery {
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		// we get to go another round!
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.threshold(),
				|_| Has::Yes,
				false,
			)
			.await;

		// Recovered data should match the original one:
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn returns_early_if_we_have_the_data(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;
		test_state.respond_to_available_data_query(&mut virtual_overseer, true).await;

		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[test]
fn returns_early_if_present_in_the_subsystem_cache() {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = with_fast_path(
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		let who_has = |i| match i {
			3 => Has::Yes,
			_ => Has::No,
		};

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;

		test_state
			.test_full_data_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				who_has,
				GroupIndex(0),
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);

		// A second recovery for the same candidate will return early as it'll be present in the
		// cache.
		let (tx, rx) = oneshot::channel();
		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(GroupIndex(0)),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);

		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn does_not_query_local_validator(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_chunks_only(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;
		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state
			.respond_to_query_all_request(&mut virtual_overseer, |i| i.0 == 0)
			.await;

		let candidate_hash = test_state.candidate.hash();

		// second round, make sure it uses the local chunk.
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold - 1,
				|i| if i.0 == 0 { panic!("requested from local validator") } else { Has::Yes },
				systematic_recovery,
			)
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn invalid_local_chunk(#[case] systematic_recovery: bool) {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;
		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;

		let validator_index_for_first_chunk = test_state
			.chunks
			.iter()
			.enumerate()
			.find_map(|(val_idx, chunk)| if chunk.index.0 == 0 { Some(val_idx) } else { None })
			.unwrap() as u32;

		test_state
			.respond_to_query_all_request_invalid(&mut virtual_overseer, |i| {
				i.0 == validator_index_for_first_chunk
			})
			.await;

		let candidate_hash = test_state.candidate.hash();

		// If systematic recovery detects invalid local chunk, it'll directly go to regular
		// recovery, if we were the one holding an invalid chunk.
		if systematic_recovery {
			test_state
				.respond_to_query_all_request_invalid(&mut virtual_overseer, |i| {
					i.0 == validator_index_for_first_chunk
				})
				.await;
		}

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.threshold(),
				|i| {
					if i.0 == validator_index_for_first_chunk {
						panic!("requested from local validator")
					} else {
						Has::Yes
					}
				},
				false,
			)
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[test]
fn systematic_chunks_are_not_requested_again_in_regular_recovery() {
	// Run this test multiple times, as the order in which requests are made is random and we want
	// to make sure that we catch regressions.
	for _ in 0..TestState::default().chunks.len() {
		let test_state = TestState::default();
		let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
		let subsystem = with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		);

		test_harness(subsystem, |mut virtual_overseer| async move {
			overseer_signal(
				&mut virtual_overseer,
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
					test_state.current,
					1,
				))),
			)
			.await;

			let (tx, rx) = oneshot::channel();

			overseer_send(
				&mut virtual_overseer,
				AvailabilityRecoveryMessage::RecoverAvailableData(
					test_state.candidate.clone(),
					test_state.session_index,
					None,
					Some(test_state.core_index),
					tx,
				),
			)
			.await;

			test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

			test_state.test_runtime_api_node_features(&mut virtual_overseer).await;
			test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

			let validator_index_for_first_chunk = test_state
				.chunks
				.iter()
				.enumerate()
				.find_map(|(val_idx, chunk)| if chunk.index.0 == 0 { Some(val_idx) } else { None })
				.unwrap() as u32;

			test_state
				.test_chunk_requests(
					&req_protocol_names,
					test_state.candidate.hash(),
					&mut virtual_overseer,
					test_state.systematic_threshold(),
					|i| if i.0 == validator_index_for_first_chunk { Has::No } else { Has::Yes },
					true,
				)
				.await;

			// Falls back to regular recovery, since one validator returned a fatal error.
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

			test_state
				.test_chunk_requests(
					&req_protocol_names,
					test_state.candidate.hash(),
					&mut virtual_overseer,
					1,
					|i| {
						if (test_state.chunks.get(i).unwrap().index.0 as usize) <
							test_state.systematic_threshold()
						{
							panic!("Already requested")
						} else {
							Has::Yes
						}
					},
					false,
				)
				.await;

			assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
			virtual_overseer
		});
	}
}

#[rstest]
#[case(true, true)]
#[case(true, false)]
#[case(false, true)]
#[case(false, false)]
fn chunk_indices_are_mapped_to_different_validators(
	#[case] systematic_recovery: bool,
	#[case] mapping_enabled: bool,
) {
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let test_state = match mapping_enabled {
		true => TestState::default(),
		false => TestState::with_empty_node_features(),
	};
	let subsystem = match systematic_recovery {
		true => with_systematic_chunks(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
		false => with_chunks_only(
			request_receiver(&req_protocol_names),
			&req_protocol_names,
			Metrics::new_dummy(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, _rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		let mut chunk_indices: Vec<(u32, u32)> = vec![];

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::SendRequests(
					requests,
					_if_disconnected,
				)
			) => {
				for req in requests {
					assert_matches!(
						req,
						Requests::ChunkFetching(req) => {
							assert_eq!(req.payload.candidate_hash, test_state.candidate.hash());

							let validator_index = req.payload.index;
							let chunk_index = test_state.chunks.get(validator_index).unwrap().index;

							if systematic_recovery && mapping_enabled {
								assert!((chunk_index.0 as usize) <= test_state.systematic_threshold(), "requested non-systematic chunk");
							}

							chunk_indices.push((chunk_index.0, validator_index.0));
						}
					)
				}
			}
		);

		if mapping_enabled {
			assert!(!chunk_indices.iter().any(|(c_index, v_index)| c_index == v_index));
		} else {
			assert!(chunk_indices.iter().all(|(c_index, v_index)| c_index == v_index));
		}

		virtual_overseer
	});
}

#[rstest]
#[case(true, false)]
#[case(false, true)]
#[case(false, false)]
fn number_of_request_retries_is_bounded(
	#[case] systematic_recovery: bool,
	#[case] should_fail: bool,
) {
	let mut test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	// We need the number of validators to be evenly divisible by the threshold for this test to be
	// easier to write.
	let n_validators = 6;
	test_state.validators.truncate(n_validators);
	test_state.validator_authority_id.truncate(n_validators);
	let mut temp = test_state.validator_public.to_vec();
	temp.truncate(n_validators);
	test_state.validator_public = temp.into();

	let (chunks, erasure_root) = derive_erasure_chunks_with_proofs_and_root(
		n_validators,
		&test_state.available_data,
		|_, _| {},
	);
	test_state.chunks =
		map_chunks(chunks, &test_state.node_features, n_validators, test_state.core_index);
	test_state.candidate.descriptor.set_erasure_root(erasure_root);

	let (subsystem, retry_limit) = match systematic_recovery {
		false => (
			with_chunks_only(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			REGULAR_CHUNKS_REQ_RETRY_LIMIT,
		),
		true => (
			with_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			SYSTEMATIC_CHUNKS_REQ_RETRY_LIMIT,
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;
		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		let validator_count_per_iteration = if systematic_recovery {
			test_state.systematic_threshold()
		} else {
			test_state.chunks.len()
		};

		// Network errors are considered non-fatal but should be retried a limited number of times.
		for _ in 1..retry_limit {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					test_state.candidate.hash(),
					&mut virtual_overseer,
					validator_count_per_iteration,
					|_| Has::timeout(),
					systematic_recovery,
				)
				.await;
		}

		if should_fail {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					test_state.candidate.hash(),
					&mut virtual_overseer,
					validator_count_per_iteration,
					|_| Has::timeout(),
					systematic_recovery,
				)
				.await;

			assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		} else {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					test_state.candidate.hash(),
					&mut virtual_overseer,
					test_state.threshold(),
					|_| Has::Yes,
					systematic_recovery,
				)
				.await;

			assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		}

		virtual_overseer
	});
}

#[test]
fn systematic_recovery_retries_from_backers() {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = with_systematic_chunks(
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();
		let group_index = GroupIndex(2);
		let group_size = test_state.validator_groups.get(group_index).unwrap().len();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				Some(group_index),
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;
		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		let mut cnt = 0;

		test_state
			.test_chunk_requests(
				&req_protocol_names,
				test_state.candidate.hash(),
				&mut virtual_overseer,
				test_state.systematic_threshold(),
				|_| {
					let res = if cnt < group_size { Has::timeout() } else { Has::Yes };
					cnt += 1;
					res
				},
				true,
			)
			.await;

		// Exhaust retries.
		for _ in 0..(SYSTEMATIC_CHUNKS_REQ_RETRY_LIMIT - 1) {
			test_state
				.test_chunk_requests(
					&req_protocol_names,
					test_state.candidate.hash(),
					&mut virtual_overseer,
					group_size,
					|_| Has::No,
					true,
				)
				.await;
		}

		// Now, final chance is to try from a backer.
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				test_state.candidate.hash(),
				&mut virtual_overseer,
				group_size,
				|_| Has::Yes,
				true,
			)
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn test_legacy_network_protocol_with_mapping_disabled(#[case] systematic_recovery: bool) {
	// In this case, when the mapping is disabled, recovery will work with both v2 and v1 requests,
	// under the assumption that ValidatorIndex is always equal to ChunkIndex. However, systematic
	// recovery will not be possible, it will fall back to regular recovery.
	let test_state = TestState::with_empty_node_features();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_fast_path(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		test_state
			.test_chunk_requests_v1(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				threshold,
				|_| Has::Yes,
				false,
			)
			.await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);
		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn test_legacy_network_protocol_with_mapping_enabled(#[case] systematic_recovery: bool) {
	// In this case, when the mapping is enabled, we MUST only use v2. Recovery should fail for v1.
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let (subsystem, threshold) = match systematic_recovery {
		true => (
			with_systematic_chunks(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.systematic_threshold(),
		),
		false => (
			with_fast_path(
				request_receiver(&req_protocol_names),
				&req_protocol_names,
				Metrics::new_dummy(),
			),
			test_state.threshold(),
		),
	};

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		if systematic_recovery {
			test_state
				.test_chunk_requests_v1(
					&req_protocol_names,
					candidate_hash,
					&mut virtual_overseer,
					threshold,
					|_| Has::Yes,
					systematic_recovery,
				)
				.await;

			// Systematic recovery failed, trying regular recovery.
			test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;
		}

		test_state
			.test_chunk_requests_v1(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.validators.len() - test_state.threshold(),
				|_| Has::Yes,
				false,
			)
			.await;

		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[test]
fn test_systematic_recovery_skipped_if_no_core_index() {
	let test_state = TestState::default();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = with_systematic_chunks(
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				None,
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		// Systematic recovery not possible without core index, falling back to regular recovery.
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.validators.len() - test_state.threshold(),
				|_| Has::No,
				false,
			)
			.await;

		// Make it fail, in order to assert that indeed regular recovery was attempted. If it were
		// systematic recovery, we would have had one more attempt for regular reconstruction.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}

#[test]
fn test_systematic_recovery_skipped_if_mapping_disabled() {
	let test_state = TestState::with_empty_node_features();
	let req_protocol_names = ReqProtocolNames::new(&GENESIS_HASH, None);
	let subsystem = AvailabilityRecoverySubsystem::for_validator(
		None,
		request_receiver(&req_protocol_names),
		&req_protocol_names,
		Metrics::new_dummy(),
	);

	test_harness(subsystem, |mut virtual_overseer| async move {
		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(new_leaf(
				test_state.current,
				1,
			))),
		)
		.await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				test_state.session_index,
				None,
				Some(test_state.core_index),
				tx,
			),
		)
		.await;

		test_state.test_runtime_api_session_info(&mut virtual_overseer).await;

		test_state.test_runtime_api_node_features(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.respond_to_available_data_query(&mut virtual_overseer, false).await;
		test_state.respond_to_query_all_request(&mut virtual_overseer, |_| false).await;

		// Systematic recovery not possible without core index, falling back to regular recovery.
		test_state
			.test_chunk_requests(
				&req_protocol_names,
				candidate_hash,
				&mut virtual_overseer,
				test_state.validators.len() - test_state.threshold(),
				|_| Has::No,
				false,
			)
			.await;

		// Make it fail, in order to assert that indeed regular recovery was attempted. If it were
		// systematic recovery, we would have had one more attempt for regular reconstruction.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
		virtual_overseer
	});
}
