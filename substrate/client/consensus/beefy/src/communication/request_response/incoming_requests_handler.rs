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

//! Helper for handling (i.e. answering) BEEFY justifications requests from a remote peer.

use codec::DecodeAll;
use futures::{channel::oneshot, StreamExt};
use log::{debug, trace};
use sc_client_api::BlockBackend;
use sc_network::{
	config as netconfig, service::traits::RequestResponseConfig, types::ProtocolName,
	NetworkBackend, ReputationChange,
};
use sc_network_types::PeerId;
use sp_consensus_beefy::BEEFY_ENGINE_ID;
use sp_runtime::traits::Block;
use std::{marker::PhantomData, sync::Arc};

use crate::{
	communication::{
		cost,
		request_response::{
			on_demand_justifications_protocol_config, Error, JustificationRequest,
			BEEFY_SYNC_LOG_TARGET,
		},
	},
	metric_inc,
	metrics::{register_metrics, OnDemandIncomingRequestsMetrics},
};

/// A request coming in, including a sender for sending responses.
#[derive(Debug)]
pub(crate) struct IncomingRequest<B: Block> {
	/// `PeerId` of sending peer.
	pub peer: PeerId,
	/// The sent request.
	pub payload: JustificationRequest<B>,
	/// Sender for sending response back.
	pub pending_response: oneshot::Sender<netconfig::OutgoingResponse>,
}

impl<B: Block> IncomingRequest<B> {
	/// Create new `IncomingRequest`.
	pub fn new(
		peer: PeerId,
		payload: JustificationRequest<B>,
		pending_response: oneshot::Sender<netconfig::OutgoingResponse>,
	) -> Self {
		Self { peer, payload, pending_response }
	}

	/// Try building from raw network request.
	///
	/// This function will fail if the request cannot be decoded and will apply passed in
	/// reputation changes in that case.
	///
	/// Params:
	/// 	- The raw request to decode
	/// 	- Reputation changes to apply for the peer in case decoding fails.
	pub fn try_from_raw<F>(
		raw: netconfig::IncomingRequest,
		reputation_changes_on_err: F,
	) -> Result<Self, Error>
	where
		F: FnOnce(usize) -> Vec<ReputationChange>,
	{
		let netconfig::IncomingRequest { payload, peer, pending_response } = raw;
		let payload = match JustificationRequest::decode_all(&mut payload.as_ref()) {
			Ok(payload) => payload,
			Err(err) => {
				let response = netconfig::OutgoingResponse {
					result: Err(()),
					reputation_changes: reputation_changes_on_err(payload.len()),
					sent_feedback: None,
				};
				if let Err(_) = pending_response.send(response) {
					return Err(Error::DecodingErrorNoReputationChange(peer, err));
				}
				return Err(Error::DecodingError(peer, err));
			},
		};
		Ok(Self::new(peer, payload, pending_response))
	}
}

/// Receiver for incoming BEEFY justifications requests.
///
/// Takes care of decoding and handling of invalid encoded requests.
pub(crate) struct IncomingRequestReceiver {
	raw: async_channel::Receiver<netconfig::IncomingRequest>,
}

impl IncomingRequestReceiver {
	pub fn new(inner: async_channel::Receiver<netconfig::IncomingRequest>) -> Self {
		Self { raw: inner }
	}

	/// Try to receive the next incoming request.
	///
	/// Any received request will be decoded, on decoding errors the provided reputation changes
	/// will be applied and an error will be reported.
	pub async fn recv<B, F>(&mut self, reputation_changes: F) -> Result<IncomingRequest<B>, Error>
	where
		B: Block,
		F: FnOnce(usize) -> Vec<ReputationChange>,
	{
		let req = match self.raw.next().await {
			None => return Err(Error::RequestChannelExhausted),
			Some(raw) => IncomingRequest::<B>::try_from_raw(raw, reputation_changes)?,
		};
		Ok(req)
	}
}

/// Handler for incoming BEEFY justifications requests from a remote peer.
pub struct BeefyJustifsRequestHandler<B, Client> {
	pub(crate) request_receiver: IncomingRequestReceiver,
	pub(crate) justif_protocol_name: ProtocolName,
	pub(crate) client: Arc<Client>,
	pub(crate) metrics: Option<OnDemandIncomingRequestsMetrics>,
	pub(crate) _block: PhantomData<B>,
}

impl<B, Client> BeefyJustifsRequestHandler<B, Client>
where
	B: Block,
	Client: BlockBackend<B> + Send + Sync,
{
	/// Create a new [`BeefyJustifsRequestHandler`].
	pub fn new<Hash: AsRef<[u8]>, Network: NetworkBackend<B, <B as Block>::Hash>>(
		genesis_hash: Hash,
		fork_id: Option<&str>,
		client: Arc<Client>,
		prometheus_registry: Option<prometheus_endpoint::Registry>,
	) -> (Self, Network::RequestResponseProtocolConfig) {
		let (request_receiver, config): (_, Network::RequestResponseProtocolConfig) =
			on_demand_justifications_protocol_config::<_, _, Network>(genesis_hash, fork_id);
		let justif_protocol_name = config.protocol_name().clone();
		let metrics = register_metrics(prometheus_registry);
		(
			Self { request_receiver, justif_protocol_name, client, metrics, _block: PhantomData },
			config,
		)
	}

	/// Network request-response protocol name used by this handler.
	pub fn protocol_name(&self) -> ProtocolName {
		self.justif_protocol_name.clone()
	}

	// Sends back justification response if justification found in client backend.
	fn handle_request(&self, request: IncomingRequest<B>) -> Result<(), Error> {
		let mut reputation_changes = vec![];
		let maybe_encoded_proof = self
			.client
			.block_hash(request.payload.begin)
			.ok()
			.flatten()
			.and_then(|hash| self.client.justifications(hash).ok().flatten())
			.and_then(|justifs| justifs.get(BEEFY_ENGINE_ID).cloned())
			.ok_or_else(|| reputation_changes.push(cost::UNKNOWN_PROOF_REQUEST));
		request
			.pending_response
			.send(netconfig::OutgoingResponse {
				result: maybe_encoded_proof,
				reputation_changes,
				sent_feedback: None,
			})
			.map_err(|_| Error::SendResponse)
	}

	/// Run [`BeefyJustifsRequestHandler`].
	///
	/// Should never end, returns `Error` otherwise.
	pub async fn run(&mut self) -> Error {
		trace!(target: BEEFY_SYNC_LOG_TARGET, "🥩 Running BeefyJustifsRequestHandler");

		loop {
			let request = match self
				.request_receiver
				.recv(|bytes| {
					let bytes = bytes.min(i32::MAX as usize) as i32;
					vec![ReputationChange::new(
						bytes.saturating_mul(cost::PER_UNDECODABLE_BYTE),
						"BEEFY: Bad request payload",
					)]
				})
				.await
			{
				Ok(request) => request,
				Err(
					e @ (Error::DecodingError(_, _) | Error::DecodingErrorNoReputationChange(_, _)),
				) => {
					// Malformed request from a peer: it has already been refused and the peer
					// penalized in `recv()`. Keep serving other peers.
					metric_inc!(self.metrics, beefy_failed_justification_responses);
					debug!(
						target: BEEFY_SYNC_LOG_TARGET,
						"🥩 Ignoring invalid BEEFY justification request: {}", e,
					);
					continue;
				},
				Err(e) => return e,
			};

			let peer = request.peer;
			match self.handle_request(request) {
				Ok(()) => {
					metric_inc!(self.metrics, beefy_successful_justification_responses);
					debug!(
						target: BEEFY_SYNC_LOG_TARGET,
						"🥩 Handled BEEFY justification request from {:?}.", peer
					)
				},
				Err(e) => {
					// peer reputation changes already applied in `self.handle_request()`
					metric_inc!(self.metrics, beefy_failed_justification_responses);
					debug!(
						target: BEEFY_SYNC_LOG_TARGET,
						"🥩 Failed to handle BEEFY justification request from {:?}: {}", peer, e,
					)
				},
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::communication::request_response::JUSTIF_CHANNEL_SIZE;
	use codec::Encode;
	use sc_block_builder::BlockBuilderBuilder;
	use sc_network::config::OutgoingResponse;
	use sp_blockchain::HeaderBackend;
	use substrate_test_runtime_client::{
		runtime::Block as TestBlock, Backend, Client, ClientBlockImportExt, ClientExt,
		DefaultTestClientBuilderExt, TestClientBuilder, TestClientBuilderExt,
	};

	type TestClient = Client<Backend>;

	fn test_handler(
		client: TestClient,
	) -> (
		async_channel::Sender<netconfig::IncomingRequest>,
		BeefyJustifsRequestHandler<TestBlock, TestClient>,
	) {
		let (tx, rx) = async_channel::bounded(JUSTIF_CHANNEL_SIZE);
		let handler = BeefyJustifsRequestHandler {
			request_receiver: IncomingRequestReceiver::new(rx),
			justif_protocol_name: ProtocolName::Static("/beefy/justifications/1"),
			client: Arc::new(client),
			metrics: None,
			_block: PhantomData,
		};
		(tx, handler)
	}

	async fn send_request(
		tx: &async_channel::Sender<netconfig::IncomingRequest>,
		payload: Vec<u8>,
	) -> Result<OutgoingResponse, oneshot::Canceled> {
		let (pending_response, rx) = oneshot::channel();
		tx.send(netconfig::IncomingRequest { peer: PeerId::random(), payload, pending_response })
			.await
			.unwrap();
		rx.await
	}

	#[tokio::test]
	async fn misbehaving_sending_peers_are_penalized() {
		let (tx, mut handler) = test_handler(TestClientBuilder::new().build());
		let handler_task = tokio::spawn(async move { handler.run().await });

		// A malformed payload is refused and the sending peer penalized.
		let response = send_request(&tx, vec![0xff]).await.expect("handler answers the request");
		assert_eq!(response.result, Err(()));
		assert!(!response.reputation_changes.is_empty());

		// A valid payload with trailing garbage is rejected by `decode_all` just the same.
		let mut payload = JustificationRequest::<TestBlock> { begin: 1 }.encode();
		payload.push(0x00);
		let response = send_request(&tx, payload).await.expect("handler answers the request");
		assert_eq!(response.result, Err(()));
		assert!(!response.reputation_changes.is_empty());

		// The handler is still running and serves well-formed requests. There's no BEEFY
		// justification for block #1 in the test client, so the request is refused, but getting
		// any answer at all proves the loop survived the malformed requests above.
		let payload = JustificationRequest::<TestBlock> { begin: 1 }.encode();
		let response = send_request(&tx, payload).await.expect("handler answers the request");
		assert_eq!(response.result, Err(()));
		assert_eq!(response.reputation_changes, vec![cost::UNKNOWN_PROOF_REQUEST]);

		// Closing the incoming requests channel is fatal and does end the handler.
		drop(tx);
		handler_task.await.unwrap();
	}

	#[tokio::test]
	async fn known_justification_is_served() {
		let client = TestClientBuilder::new().build();
		let justif = vec![42u8];

		// Give the client a BEEFY justification for block #1.
		let block = BlockBuilderBuilder::new(&client)
			.on_parent_block(client.info().genesis_hash)
			.with_parent_block_number(0)
			.build()
			.unwrap()
			.build()
			.unwrap()
			.block;
		let hash = block.header.hash();
		client.import(sp_consensus::BlockOrigin::Own, block).await.unwrap();
		client.finalize_block(hash, Some((BEEFY_ENGINE_ID, justif.clone()))).unwrap();

		let (tx, mut handler) = test_handler(client);
		let handler_task = tokio::spawn(async move { handler.run().await });

		let payload = JustificationRequest::<TestBlock> { begin: 1 }.encode();
		let response = send_request(&tx, payload).await.expect("handler answers the request");
		assert_eq!(response.result, Ok(justif));
		assert!(response.reputation_changes.is_empty());

		drop(tx);
		handler_task.await.unwrap();
	}
}
