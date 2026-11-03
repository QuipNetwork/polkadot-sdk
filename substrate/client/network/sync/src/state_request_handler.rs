// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Substrate.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <https://www.gnu.org/licenses/>.

//! Helper for handling (i.e. answering) state requests from a remote peer via the
//! `crate::request_responses::RequestResponsesBehaviour`.

use crate::{
	schema::v1::{KeyValueStateEntry, StateEntry, StateRequest, StateResponse},
	LOG_TARGET,
};

use codec::{Decode, Encode};
use futures::{channel::oneshot, stream::StreamExt};
use log::{debug, trace};
use prost::Message;
use sc_network_types::PeerId;
use schnellru::{ByLength, LruMap};

use sc_client_api::{BlockBackend, ProofProvider};
use sc_network::{
	config::ProtocolId,
	request_responses::{IncomingRequest, OutgoingResponse},
	NetworkBackend, MAX_RESPONSE_SIZE,
};
use sp_runtime::traits::Block as BlockT;

use std::{
	hash::{Hash, Hasher},
	sync::Arc,
	time::{Duration, Instant},
};

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024; // Actual reponse may be bigger.
const MAX_NUMBER_OF_SAME_REQUESTS_PER_PEER: usize = 2;

/// Reset duplicate counts this long after the first fulfilled response, allowing legitimate
/// retries.
const SAME_REQUEST_WINDOW: Duration = Duration::from_secs(60);

mod rep {
	use sc_network::ReputationChange as Rep;

	/// Reputation change when a peer sent us the same request multiple times.
	pub const SAME_REQUEST: Rep = Rep::new(-(1 << 12), "Same state request multiple times");
}

/// Generates a `RequestResponseProtocolConfig` for the state request protocol, refusing incoming
/// requests.
pub fn generate_protocol_config<
	Hash: AsRef<[u8]>,
	B: BlockT,
	N: NetworkBackend<B, <B as BlockT>::Hash>,
>(
	protocol_id: &ProtocolId,
	genesis_hash: Hash,
	fork_id: Option<&str>,
	inbound_queue: async_channel::Sender<IncomingRequest>,
) -> N::RequestResponseProtocolConfig {
	N::request_response_config(
		generate_protocol_name(genesis_hash, fork_id).into(),
		std::iter::once(generate_legacy_protocol_name(protocol_id).into()).collect(),
		1024 * 1024,
		MAX_RESPONSE_SIZE,
		Duration::from_secs(40),
		Some(inbound_queue),
	)
}

/// Generate the state protocol name from the genesis hash and fork id.
fn generate_protocol_name<Hash: AsRef<[u8]>>(genesis_hash: Hash, fork_id: Option<&str>) -> String {
	let genesis_hash = genesis_hash.as_ref();
	if let Some(fork_id) = fork_id {
		format!("/{}/{}/state/2", array_bytes::bytes2hex("", genesis_hash), fork_id)
	} else {
		format!("/{}/state/2", array_bytes::bytes2hex("", genesis_hash))
	}
}

/// Generate the legacy state protocol name from chain specific protocol identifier.
fn generate_legacy_protocol_name(protocol_id: &ProtocolId) -> String {
	format!("/{}/state/2", protocol_id.as_ref())
}

/// The key of [`BlockRequestHandler::seen_requests`].
#[derive(Eq, PartialEq, Clone)]
struct SeenRequestsKey<B: BlockT> {
	peer: PeerId,
	block: B::Hash,
	start: Vec<Vec<u8>>,
}

#[allow(clippy::derived_hash_with_manual_eq)]
impl<B: BlockT> Hash for SeenRequestsKey<B> {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.peer.hash(state);
		self.block.hash(state);
		self.start.hash(state);
	}
}

/// The value of [`StateRequestHandler::seen_requests`].
enum SeenRequestsValue {
	/// First time we have seen the request.
	First,
	/// Requests seen since the first fulfilled response.
	Fulfilled { requests: usize, since: Instant },
}

/// Handler for incoming block requests from a remote peer.
pub struct StateRequestHandler<B: BlockT, Client> {
	client: Arc<Client>,
	request_receiver: async_channel::Receiver<IncomingRequest>,
	/// Maps from request to number of times we have seen this request.
	///
	/// This is used to check if a peer is spamming us with the same request.
	seen_requests: LruMap<SeenRequestsKey<B>, SeenRequestsValue>,
}

impl<B, Client> StateRequestHandler<B, Client>
where
	B: BlockT,
	Client: BlockBackend<B> + ProofProvider<B> + Send + Sync + 'static,
{
	/// Create a new [`StateRequestHandler`].
	pub fn new<N: NetworkBackend<B, <B as BlockT>::Hash>>(
		protocol_id: &ProtocolId,
		fork_id: Option<&str>,
		client: Arc<Client>,
		num_peer_hint: usize,
	) -> (Self, N::RequestResponseProtocolConfig) {
		// Reserve enough request slots for one request per peer when we are at the maximum
		// number of peers.
		let capacity = std::cmp::max(num_peer_hint, 1);
		let (tx, request_receiver) = async_channel::bounded(capacity);

		let protocol_config = generate_protocol_config::<_, B, N>(
			protocol_id,
			client
				.block_hash(0u32.into())
				.ok()
				.flatten()
				.expect("Genesis block exists; qed"),
			fork_id,
			tx,
		);

		let capacity = ByLength::new(num_peer_hint.max(1) as u32 * 2);
		let seen_requests = LruMap::new(capacity);

		(Self { client, request_receiver, seen_requests }, protocol_config)
	}

	/// Run [`StateRequestHandler`].
	pub async fn run(mut self) {
		while let Some(request) = self.request_receiver.next().await {
			let IncomingRequest { peer, payload, pending_response } = request;

			match self.handle_request(payload, pending_response, &peer) {
				Ok(()) => debug!(target: LOG_TARGET, "Handled block request from {}.", peer),
				Err(e) => debug!(
					target: LOG_TARGET,
					"Failed to handle state request from {}: {}", peer, e,
				),
			}
		}
	}

	fn handle_request(
		&mut self,
		payload: Vec<u8>,
		pending_response: oneshot::Sender<OutgoingResponse>,
		peer: &PeerId,
	) -> Result<(), HandleRequestError> {
		let request = StateRequest::decode(&payload[..])?;
		let block: B::Hash = Decode::decode(&mut request.block.as_ref())?;

		let key = SeenRequestsKey { peer: *peer, block, start: request.start.clone() };

		let mut reputation_changes = Vec::new();

		match self.seen_requests.get(&key) {
			Some(SeenRequestsValue::First) => {},
			Some(SeenRequestsValue::Fulfilled { requests, since })
				if since.elapsed() <= SAME_REQUEST_WINDOW =>
			{
				*requests = requests.saturating_add(1);

				if *requests > MAX_NUMBER_OF_SAME_REQUESTS_PER_PEER {
					reputation_changes.push(rep::SAME_REQUEST);
				}
			},
			Some(value @ SeenRequestsValue::Fulfilled { .. }) => {
				*value = SeenRequestsValue::First;
			},
			None => {
				self.seen_requests.insert(key.clone(), SeenRequestsValue::First);
			},
		}

		trace!(
			target: LOG_TARGET,
			"Handling state request from {}: Block {:?}, Starting at {:x?}, no_proof={}",
			peer,
			request.block,
			&request.start,
			request.no_proof,
		);

		let result = if reputation_changes.is_empty() {
			let mut response = StateResponse::default();

			if !request.no_proof {
				let (proof, _count) = self.client.read_proof_collection(
					block,
					request.start.as_slice(),
					MAX_RESPONSE_BYTES,
				)?;
				response.proof = proof.encode();
			} else {
				let entries = self.client.storage_collection(
					block,
					request.start.as_slice(),
					MAX_RESPONSE_BYTES,
				)?;
				response.entries = entries
					.into_iter()
					.map(|(state, complete)| KeyValueStateEntry {
						state_root: state.state_root,
						entries: state
							.key_values
							.into_iter()
							.map(|(key, value)| StateEntry { key, value })
							.collect(),
						complete,
					})
					.collect();
			}

			trace!(
				target: LOG_TARGET,
				"StateResponse contains {} keys, {}, proof nodes, from {:?} to {:?}",
				response.entries.len(),
				response.proof.len(),
				response.entries.get(0).and_then(|top| top
					.entries
					.first()
					.map(|e| sp_core::hexdisplay::HexDisplay::from(&e.key))),
				response.entries.get(0).and_then(|top| top
					.entries
					.last()
					.map(|e| sp_core::hexdisplay::HexDisplay::from(&e.key))),
			);
			if let Some(value) = self.seen_requests.get(&key) {
				if let SeenRequestsValue::First = value {
					*value = SeenRequestsValue::Fulfilled { requests: 1, since: Instant::now() };
				}
			}

			let mut data = Vec::with_capacity(response.encoded_len());
			response.encode(&mut data)?;
			Ok(data)
		} else {
			Err(())
		};

		pending_response
			.send(OutgoingResponse { result, reputation_changes, sent_feedback: None })
			.map_err(|_| HandleRequestError::SendResponse)
	}
}

#[derive(Debug, thiserror::Error)]
enum HandleRequestError {
	#[error("Failed to decode request: {0}.")]
	DecodeProto(#[from] prost::DecodeError),

	#[error("Failed to encode response: {0}.")]
	EncodeProto(#[from] prost::EncodeError),

	#[error("Failed to decode block hash: {0}.")]
	InvalidHash(#[from] codec::Error),

	#[error(transparent)]
	Client(#[from] sp_blockchain::Error),

	#[error("Failed to send response.")]
	SendResponse,
}

#[cfg(test)]
mod tests {
	use super::*;
	use substrate_test_runtime_client::{
		runtime::Block, DefaultTestClientBuilderExt, TestClient, TestClientBuilder,
		TestClientBuilderExt,
	};

	fn test_handler() -> StateRequestHandler<Block, TestClient> {
		let client = Arc::new(TestClientBuilder::new().build());
		let (_tx, request_receiver) = async_channel::bounded(1);
		StateRequestHandler {
			client,
			request_receiver,
			seen_requests: LruMap::new(ByLength::new(16)),
		}
	}

	fn send_request(
		handler: &mut StateRequestHandler<Block, TestClient>,
		peer: &PeerId,
		no_proof: bool,
	) -> OutgoingResponse {
		let request = StateRequest {
			block: Encode::encode(&handler.client.chain_info().genesis_hash),
			start: Vec::new(),
			no_proof,
		};
		let (tx, mut rx) = oneshot::channel();
		handler.handle_request(request.encode_to_vec(), tx, peer).unwrap();
		rx.try_recv().unwrap().unwrap()
	}

	fn check_same_request_limit_resets_after_window(no_proof: bool) {
		let mut handler = test_handler();
		let peer = PeerId::random();

		for _ in 0..MAX_NUMBER_OF_SAME_REQUESTS_PER_PEER {
			let response = send_request(&mut handler, &peer, no_proof);
			assert!(response.result.is_ok());
			assert!(response.reputation_changes.is_empty());
		}

		let response = send_request(&mut handler, &peer, no_proof);
		assert!(response.result.is_err());
		assert_eq!(response.reputation_changes, vec![rep::SAME_REQUEST]);
		assert!(rep::SAME_REQUEST.value > i32::MIN);

		// Expire the window without sleeping.
		let key = SeenRequestsKey::<Block> {
			peer,
			block: handler.client.chain_info().genesis_hash,
			start: Vec::new(),
		};
		match handler.seen_requests.get(&key) {
			Some(SeenRequestsValue::Fulfilled { since, .. }) => {
				*since = Instant::now() - SAME_REQUEST_WINDOW - Duration::from_secs(1)
			},
			_ => panic!("entry must be in the fulfilled state"),
		}

		// The new window has the same limit and no penalties for allowed requests.
		for _ in 0..MAX_NUMBER_OF_SAME_REQUESTS_PER_PEER {
			let response = send_request(&mut handler, &peer, no_proof);
			assert!(response.result.is_ok());
			assert!(response.reputation_changes.is_empty());
		}
		let response = send_request(&mut handler, &peer, no_proof);
		assert!(response.result.is_err());
		assert_eq!(response.reputation_changes, vec![rep::SAME_REQUEST]);
	}

	#[test]
	fn same_request_limit_resets_after_window_with_proof() {
		check_same_request_limit_resets_after_window(false);
	}

	#[test]
	fn same_request_limit_resets_after_window_without_proof() {
		check_same_request_limit_resets_after_window(true);
	}
}
