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

//! Collection of request-response protocols.
//!
//! The [`RequestResponsesBehaviour`] struct defined in this module provides support for zero or
//! more so-called "request-response" protocols.
//!
//! A request-response protocol works in the following way:
//!
//! - For every emitted request, a new substream is open and the protocol is negotiated. If the
//! remote supports the protocol, the size of the request is sent as a LEB128 number, followed
//! with the request itself. The remote then sends the size of the response as a LEB128 number,
//! followed with the response.
//!
//! - Requests have a certain time limit before they time out. This time includes the time it
//! takes to send/receive the request and response.
//!
//! - If provided, a ["requests processing"](ProtocolConfig::inbound_queue) channel
//! is used to handle incoming requests.

use crate::{
	peer_store::{PeerStoreProvider, BANNED_THRESHOLD},
	service::traits::RequestResponseConfig as RequestResponseConfigT,
	types::ProtocolName,
	ReputationChange,
};

use futures::{channel::oneshot, prelude::*};
use libp2p::{
	core::{transport::PortUse, Endpoint, Multiaddr},
	request_response::{self, Behaviour, Codec, Message, ProtocolSupport, ResponseChannel},
	swarm::{
		behaviour::FromSwarm, handler::multi::MultiHandler, ConnectionDenied, ConnectionId,
		NetworkBehaviour, THandler, THandlerInEvent, THandlerOutEvent, ToSwarm,
	},
	PeerId,
};

use std::{
	collections::{hash_map::Entry, HashMap},
	io, iter,
	ops::Deref,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
	time::{Duration, Instant},
};

pub use libp2p::request_response::{Config, InboundRequestId, OutboundRequestId};

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p::request-response";

/// Periodically check if requests are taking too long.
const PERIODIC_REQUEST_CHECK: Duration = Duration::from_secs(2);

/// Possible failures occurring in the context of sending an outbound request and receiving the
/// response.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OutboundFailure {
	/// The request could not be sent because a dialing attempt failed.
	#[error("Failed to dial the requested peer")]
	DialFailure,
	/// The request timed out before a response was received.
	#[error("Timeout while waiting for a response")]
	Timeout,
	/// The connection closed before a response was received.
	#[error("Connection was closed before a response was received")]
	ConnectionClosed,
	/// The remote supports none of the requested protocols.
	#[error("The remote supports none of the requested protocols")]
	UnsupportedProtocols,
	/// An IO failure happened on an outbound stream.
	#[error("An IO failure happened on an outbound stream")]
	Io(Arc<io::Error>),
}

impl From<request_response::OutboundFailure> for OutboundFailure {
	fn from(out: request_response::OutboundFailure) -> Self {
		match out {
			request_response::OutboundFailure::DialFailure => OutboundFailure::DialFailure,
			request_response::OutboundFailure::Timeout => OutboundFailure::Timeout,
			request_response::OutboundFailure::ConnectionClosed => {
				OutboundFailure::ConnectionClosed
			},
			request_response::OutboundFailure::UnsupportedProtocols => {
				OutboundFailure::UnsupportedProtocols
			},
			request_response::OutboundFailure::Io(error) => OutboundFailure::Io(Arc::new(error)),
		}
	}
}

/// Possible failures occurring in the context of receiving an inbound request and sending a
/// response.
#[derive(Debug, thiserror::Error)]
pub enum InboundFailure {
	/// The inbound request timed out, either while reading the incoming request or before a
	/// response is sent
	#[error("Timeout while receiving request or sending response")]
	Timeout,
	/// The connection closed before a response could be send.
	#[error("Connection was closed before a response could be sent")]
	ConnectionClosed,
	/// The local peer supports none of the protocols requested by the remote.
	#[error("The local peer supports none of the protocols requested by the remote")]
	UnsupportedProtocols,
	/// The local peer failed to respond to an inbound request
	#[error("The response channel was dropped without sending a response to the remote")]
	ResponseOmission,
	/// An IO failure happened on an inbound stream.
	#[error("An IO failure happened on an inbound stream")]
	Io(Arc<io::Error>),
}

impl From<request_response::InboundFailure> for InboundFailure {
	fn from(out: request_response::InboundFailure) -> Self {
		match out {
			request_response::InboundFailure::ResponseOmission => InboundFailure::ResponseOmission,
			request_response::InboundFailure::Timeout => InboundFailure::Timeout,
			request_response::InboundFailure::ConnectionClosed => InboundFailure::ConnectionClosed,
			request_response::InboundFailure::UnsupportedProtocols => {
				InboundFailure::UnsupportedProtocols
			},
			request_response::InboundFailure::Io(error) => InboundFailure::Io(Arc::new(error)),
		}
	}
}

/// Error in a request.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum RequestFailure {
	#[error("We are not currently connected to the requested peer.")]
	NotConnected,
	#[error("Given protocol hasn't been registered.")]
	UnknownProtocol,
	#[error("The outbound request payload or parameters are invalid for the selected protocol.")]
	InvalidRequest,
	#[error("Remote has closed the substream before answering, thereby signaling that it considers the request as valid, but refused to answer it.")]
	Refused,
	#[error("The remote replied, but the local node is no longer interested in the response.")]
	Obsolete,
	#[error("Problem on the network: {0}")]
	Network(OutboundFailure),
}

/// Configuration for a single request-response protocol.
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
	/// Name of the protocol on the wire. Should be something like `/foo/bar`.
	pub name: ProtocolName,

	/// Fallback on the wire protocol names to support.
	pub fallback_names: Vec<ProtocolName>,

	/// Maximum allowed size, in bytes, of a request.
	///
	/// Any request larger than this value will be declined as a way to avoid allocating too
	/// much memory for it.
	pub max_request_size: u64,

	/// Maximum allowed size, in bytes, of a response.
	///
	/// Any response larger than this value will be declined as a way to avoid allocating too
	/// much memory for it.
	pub max_response_size: u64,

	/// Duration after which emitted requests are considered timed out.
	///
	/// If you expect the response to come back quickly, you should set this to a smaller duration.
	pub request_timeout: Duration,

	/// Channel on which the networking service will send incoming requests.
	///
	/// Every time a peer sends a request to the local node using this protocol, the networking
	/// service will push an element on this channel. The receiving side of this channel then has
	/// to pull this element, process the request, and send back the response to send back to the
	/// peer.
	///
	/// The size of the channel has to be carefully chosen. If the channel is full, the networking
	/// service will discard the incoming request send back an error to the peer. Consequently,
	/// the channel being full is an indicator that the node is overloaded.
	///
	/// You can typically set the size of the channel to `T / d`, where `T` is the
	/// `request_timeout` and `d` is the expected average duration of CPU and I/O it takes to
	/// build a response.
	///
	/// Can be `None` if the local node does not support answering incoming requests.
	/// If this is `None`, then the local node will not advertise support for this protocol towards
	/// other peers. If this is `Some` but the channel is closed, then the local node will
	/// advertise support for this protocol, but any incoming request will lead to an error being
	/// sent back.
	pub inbound_queue: Option<async_channel::Sender<IncomingRequest>>,
}

impl RequestResponseConfigT for ProtocolConfig {
	fn protocol_name(&self) -> &ProtocolName {
		&self.name
	}
}

/// A single request received by a peer on a request-response protocol.
#[derive(Debug)]
pub struct IncomingRequest {
	/// Who sent the request.
	pub peer: sc_network_types::PeerId,

	/// Request sent by the remote. Will always be smaller than
	/// [`ProtocolConfig::max_request_size`].
	pub payload: Vec<u8>,

	/// Channel to send back the response.
	///
	/// There are two ways to indicate that handling the request failed:
	///
	/// 1. Drop `pending_response` and thus not changing the reputation of the peer.
	///
	/// 2. Sending an `Err(())` via `pending_response`, optionally including reputation changes for
	/// the given peer.
	pub pending_response: oneshot::Sender<OutgoingResponse>,
}

/// Response for an incoming request to be send by a request protocol handler.
#[derive(Debug)]
pub struct OutgoingResponse {
	/// The payload of the response.
	///
	/// `Err(())` if none is available e.g. due an error while handling the request.
	pub result: Result<Vec<u8>, ()>,

	/// Reputation changes accrued while handling the request. To be applied to the reputation of
	/// the peer sending the request.
	pub reputation_changes: Vec<ReputationChange>,

	/// If provided, the `oneshot::Sender` will be notified when the request has been sent to the
	/// peer.
	///
	/// > **Note**: Operating systems typically maintain a buffer of a few dozen kilobytes of
	/// >			outgoing data for each TCP socket, and it is not possible for a user
	/// >			application to inspect this buffer. This channel here is not actually notified
	/// >			when the response has been fully sent out, but rather when it has fully been
	/// >			written to the buffer managed by the operating system.
	pub sent_feedback: Option<oneshot::Sender<()>>,
}

/// Information stored about a pending request.
struct PendingRequest {
	/// The time when the request was sent to the libp2p request-response protocol.
	started_at: Instant,
	/// The channel to send the response back to the caller.
	///
	/// This is wrapped in an `Option` to allow for the channel to be taken out
	/// on force-detected timeouts.
	response_tx: Option<oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>>,
	/// Fallback request to send if the primary request fails.
	fallback_request: Option<(Vec<u8>, ProtocolName)>,
}

/// When sending a request, what to do on a disconnected recipient.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum IfDisconnected {
	/// Try to connect to the peer.
	TryConnect,
	/// Just fail if the destination is not yet connected.
	ImmediateError,
}

/// Convenience functions for `IfDisconnected`.
impl IfDisconnected {
	/// Shall we connect to a disconnected peer?
	pub fn should_connect(self) -> bool {
		match self {
			Self::TryConnect => true,
			Self::ImmediateError => false,
		}
	}
}

/// Event generated by the [`RequestResponsesBehaviour`].
#[derive(Debug)]
pub enum Event {
	/// A remote sent a request and either we have successfully answered it or an error happened.
	///
	/// This event is generated for statistics purposes.
	InboundRequest {
		/// Peer which has emitted the request.
		peer: PeerId,
		/// Name of the protocol in question.
		protocol: ProtocolName,
		/// Whether handling the request was successful or unsuccessful.
		///
		/// When successful contains the time elapsed between when we received the request and when
		/// we sent back the response. When unsuccessful contains the failure reason.
		result: Result<Duration, ResponseFailure>,
	},

	/// A request initiated using [`RequestResponsesBehaviour::send_request`] has succeeded or
	/// failed.
	///
	/// This event is generated for statistics purposes.
	RequestFinished {
		/// Peer that we send a request to.
		peer: PeerId,
		/// Name of the protocol in question.
		protocol: ProtocolName,
		/// Duration the request took.
		duration: Duration,
		/// Result of the request.
		result: Result<(), RequestFailure>,
	},

	/// A request protocol handler issued reputation changes for the given peer.
	ReputationChanges {
		/// Peer whose reputation needs to be adjust.
		peer: PeerId,
		/// Reputation changes.
		changes: Vec<ReputationChange>,
	},
}

/// Combination of a protocol name and a request id.
///
/// Uniquely identifies an inbound or outbound request among all handled protocols. Note however
/// that uniqueness is only guaranteed between two inbound and likewise between two outbound
/// requests. There is no uniqueness guarantee in a set of both inbound and outbound
/// [`ProtocolRequestId`]s.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProtocolRequestId<RequestId> {
	protocol: ProtocolName,
	request_id: RequestId,
}

impl<RequestId> From<(ProtocolName, RequestId)> for ProtocolRequestId<RequestId> {
	fn from((protocol, request_id): (ProtocolName, RequestId)) -> Self {
		Self { protocol, request_id }
	}
}

/// Details of a request-response protocol.
struct ProtocolDetails {
	behaviour: Behaviour<GenericCodec>,
	inbound_queue: Option<async_channel::Sender<IncomingRequest>>,
	request_timeout: Duration,
}

/// Implementation of `NetworkBehaviour` that provides support for request-response protocols.
pub struct RequestResponsesBehaviour {
	/// The multiple sub-protocols, by name.
	///
	/// Contains the underlying libp2p request-response [`Behaviour`], plus an optional
	/// "response builder" used to build responses for incoming requests.
	protocols: HashMap<ProtocolName, ProtocolDetails>,

	/// Pending requests, passed down to a request-response [`Behaviour`], awaiting a reply.
	pending_requests: HashMap<ProtocolRequestId<OutboundRequestId>, PendingRequest>,

	/// Whenever an incoming request arrives, a `Future` is added to this list and will yield the
	/// start time and the response to send back to the remote.
	pending_responses: stream::FuturesUnordered<
		Pin<Box<dyn Future<Output = Option<RequestProcessingOutcome>> + Send>>,
	>,

	/// Whenever an incoming request arrives, the arrival [`Instant`] is recorded here.
	pending_responses_arrival_time: HashMap<ProtocolRequestId<InboundRequestId>, Instant>,

	/// Whenever a response is received on `pending_responses`, insert a channel to be notified
	/// when the request has been sent out.
	send_feedback: HashMap<ProtocolRequestId<InboundRequestId>, oneshot::Sender<()>>,

	/// Primarily used to get a reputation of a node.
	peer_store: Arc<dyn PeerStoreProvider>,

	/// Interval to check that the requests are not taking too long.
	///
	/// We had issues in the past where libp2p did not produce a timeout event in due time.
	///
	/// For more details, see:
	/// - <https://github.com/paritytech/polkadot-sdk/issues/7076#issuecomment-2596085096>
	periodic_request_check: tokio::time::Interval,
}

/// Generated by the response builder and waiting to be processed.
struct RequestProcessingOutcome {
	peer: PeerId,
	request_id: InboundRequestId,
	protocol: ProtocolName,
	inner_channel: ResponseChannel<Result<Vec<u8>, ()>>,
	response: OutgoingResponse,
}

impl RequestResponsesBehaviour {
	/// Creates a new behaviour. Must be passed a list of supported protocols. Returns an error if
	/// the same protocol is passed twice.
	pub fn new(
		list: impl Iterator<Item = ProtocolConfig>,
		peer_store: Arc<dyn PeerStoreProvider>,
	) -> Result<Self, RegisterError> {
		let mut protocols = HashMap::new();
		for protocol in list {
			let cfg = Config::default().with_request_timeout(protocol.request_timeout);

			let protocol_support = if protocol.inbound_queue.is_some() {
				ProtocolSupport::Full
			} else {
				ProtocolSupport::Outbound
			};

			let behaviour = Behaviour::with_codec(
				GenericCodec {
					max_request_size: protocol.max_request_size,
					max_response_size: protocol.max_response_size,
				},
				iter::once(protocol.name.clone())
					.chain(protocol.fallback_names)
					.zip(iter::repeat(protocol_support)),
				cfg,
			);

			match protocols.entry(protocol.name) {
				Entry::Vacant(e) => e.insert(ProtocolDetails {
					behaviour,
					inbound_queue: protocol.inbound_queue,
					request_timeout: protocol.request_timeout,
				}),
				Entry::Occupied(e) => {
					return Err(RegisterError::DuplicateProtocol(e.key().clone()))
				},
			};
		}

		Ok(Self {
			protocols,
			pending_requests: Default::default(),
			pending_responses: Default::default(),
			pending_responses_arrival_time: Default::default(),
			send_feedback: Default::default(),
			peer_store,
			periodic_request_check: tokio::time::interval(PERIODIC_REQUEST_CHECK),
		})
	}

	/// Initiates sending a request.
	///
	/// If there is no established connection to the target peer, the behavior is determined by the
	/// choice of `connect`.
	///
	/// An error is returned if the protocol doesn't match one that has been registered.
	pub fn send_request(
		&mut self,
		target: &PeerId,
		protocol_name: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		pending_response: oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>,
		connect: IfDisconnected,
	) {
		log::trace!(target: LOG_TARGET, "send request to {target} ({protocol_name:?}), {} bytes", request.len());

		if let Some(ProtocolDetails { behaviour, .. }) =
			self.protocols.get_mut(protocol_name.deref())
		{
			Self::send_request_inner(
				behaviour,
				&mut self.pending_requests,
				target,
				protocol_name,
				request,
				fallback_request,
				pending_response,
				connect,
			)
		} else if pending_response.send(Err(RequestFailure::UnknownProtocol)).is_err() {
			log::debug!(
				target: LOG_TARGET,
				"Unknown protocol {:?}. At the same time local \
				 node is no longer interested in the result.",
				protocol_name,
			);
		}
	}

	fn send_request_inner(
		behaviour: &mut Behaviour<GenericCodec>,
		pending_requests: &mut HashMap<ProtocolRequestId<OutboundRequestId>, PendingRequest>,
		target: &PeerId,
		protocol_name: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		pending_response: oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>,
		connect: IfDisconnected,
	) {
		if behaviour.is_connected(target) || connect.should_connect() {
			let request_id = behaviour.send_request(target, request);
			let prev_req_id = pending_requests.insert(
				(protocol_name.to_string().into(), request_id).into(),
				PendingRequest {
					started_at: Instant::now(),
					response_tx: Some(pending_response),
					fallback_request,
				},
			);
			debug_assert!(prev_req_id.is_none(), "Expect request id to be unique.");
		} else if pending_response.send(Err(RequestFailure::NotConnected)).is_err() {
			log::debug!(
				target: LOG_TARGET,
				"Not connected to peer {:?}. At the same time local \
				 node is no longer interested in the result.",
				target,
			);
		}
	}
}

impl NetworkBehaviour for RequestResponsesBehaviour {
	type ConnectionHandler =
		MultiHandler<String, <Behaviour<GenericCodec> as NetworkBehaviour>::ConnectionHandler>;
	type ToSwarm = Event;

	fn handle_pending_inbound_connection(
		&mut self,
		_connection_id: ConnectionId,
		_local_addr: &Multiaddr,
		_remote_addr: &Multiaddr,
	) -> Result<(), ConnectionDenied> {
		Ok(())
	}

	fn handle_pending_outbound_connection(
		&mut self,
		_connection_id: ConnectionId,
		_maybe_peer: Option<PeerId>,
		_addresses: &[Multiaddr],
		_effective_role: Endpoint,
	) -> Result<Vec<Multiaddr>, ConnectionDenied> {
		Ok(Vec::new())
	}

	fn handle_established_inbound_connection(
		&mut self,
		connection_id: ConnectionId,
		peer: PeerId,
		local_addr: &Multiaddr,
		remote_addr: &Multiaddr,
	) -> Result<THandler<Self>, ConnectionDenied> {
		let iter =
			self.protocols.iter_mut().filter_map(|(p, ProtocolDetails { behaviour, .. })| {
				if let Ok(handler) = behaviour.handle_established_inbound_connection(
					connection_id,
					peer,
					local_addr,
					remote_addr,
				) {
					Some((p.to_string(), handler))
				} else {
					None
				}
			});

		Ok(MultiHandler::try_from_iter(iter).expect(
			"Protocols are in a HashMap and there can be at most one handler per protocol name, \
			 which is the only possible error; qed",
		))
	}

	fn handle_established_outbound_connection(
		&mut self,
		connection_id: ConnectionId,
		peer: PeerId,
		addr: &Multiaddr,
		role_override: Endpoint,
		port_use: PortUse,
	) -> Result<THandler<Self>, ConnectionDenied> {
		let iter =
			self.protocols.iter_mut().filter_map(|(p, ProtocolDetails { behaviour, .. })| {
				if let Ok(handler) = behaviour.handle_established_outbound_connection(
					connection_id,
					peer,
					addr,
					role_override,
					port_use,
				) {
					Some((p.to_string(), handler))
				} else {
					None
				}
			});

		Ok(MultiHandler::try_from_iter(iter).expect(
			"Protocols are in a HashMap and there can be at most one handler per protocol name, \
			 which is the only possible error; qed",
		))
	}

	fn on_swarm_event(&mut self, event: FromSwarm) {
		for ProtocolDetails { behaviour, .. } in self.protocols.values_mut() {
			behaviour.on_swarm_event(event);
		}
	}

	fn on_connection_handler_event(
		&mut self,
		peer_id: PeerId,
		connection_id: ConnectionId,
		event: THandlerOutEvent<Self>,
	) {
		let p_name = event.0;
		if let Some(ProtocolDetails { behaviour, .. }) = self.protocols.get_mut(p_name.as_str()) {
			return behaviour.on_connection_handler_event(peer_id, connection_id, event.1);
		} else {
			log::warn!(
				target: LOG_TARGET,
				"on_connection_handler_event: no request-response instance registered for protocol {:?}",
				p_name
			);
		}
	}

	fn poll(&mut self, cx: &mut Context) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
		'poll_all: loop {
			// Poll the periodic request check.
			if self.periodic_request_check.poll_tick(cx).is_ready() {
				self.pending_requests.retain(|id, req| {
					let Some(ProtocolDetails { request_timeout, .. }) =
						self.protocols.get(&id.protocol)
					else {
						log::warn!(
							target: LOG_TARGET,
							"Request {id:?} has no protocol registered.",
						);

						if let Some(response_tx) = req.response_tx.take() {
							if response_tx.send(Err(RequestFailure::UnknownProtocol)).is_err() {
								log::debug!(
									target: LOG_TARGET,
									"Request {id:?} has no protocol registered. At the same time local node is no longer interested in the result.",
								);
							}
						}
						return false
					};

					let elapsed = req.started_at.elapsed();
					if elapsed > *request_timeout {
						log::debug!(
							target: LOG_TARGET,
							"Request {id:?} force detected as timeout.",
						);

						if let Some(response_tx) = req.response_tx.take() {
							if response_tx.send(Err(RequestFailure::Network(OutboundFailure::Timeout))).is_err() {
								log::debug!(
									target: LOG_TARGET,
									"Request {id:?} force detected as timeout. At the same time local node is no longer interested in the result.",
								);
							}
						}

						false
					} else {
						true
					}
				});
			}

			// Poll to see if any response is ready to be sent back.
			while let Poll::Ready(Some(outcome)) = self.pending_responses.poll_next_unpin(cx) {
				let RequestProcessingOutcome {
					peer,
					request_id,
					protocol: protocol_name,
					inner_channel,
					response: OutgoingResponse { result, reputation_changes, sent_feedback },
				} = match outcome {
					Some(outcome) => outcome,
					// The response builder was too busy or handling the request failed. This is
					// later on reported as a `InboundFailure::Omission`.
					None => continue,
				};

				if let Ok(payload) = result {
					if let Some(ProtocolDetails { behaviour, .. }) =
						self.protocols.get_mut(&*protocol_name)
					{
						log::trace!(target: LOG_TARGET, "send response to {peer} ({protocol_name:?}), {} bytes", payload.len());

						if behaviour.send_response(inner_channel, Ok(payload)).is_err() {
							// Note: Failure is handled further below when receiving
							// `InboundFailure` event from request-response [`Behaviour`].
							log::debug!(
								target: LOG_TARGET,
								"Failed to send response for {:?} on protocol {:?} due to a \
								 timeout or due to the connection to the peer being closed. \
								 Dropping response",
								request_id, protocol_name,
							);
						} else if let Some(sent_feedback) = sent_feedback {
							self.send_feedback
								.insert((protocol_name, request_id).into(), sent_feedback);
						}
					}
				}

				if !reputation_changes.is_empty() {
					return Poll::Ready(ToSwarm::GenerateEvent(Event::ReputationChanges {
						peer,
						changes: reputation_changes,
					}));
				}
			}

			let mut fallback_requests = vec![];

			// Poll request-responses protocols.
			for (protocol, ProtocolDetails { behaviour, inbound_queue, .. }) in &mut self.protocols
			{
				'poll_protocol: while let Poll::Ready(ev) = behaviour.poll(cx) {
					let ev = match ev {
						// Main events we are interested in.
						ToSwarm::GenerateEvent(ev) => ev,

						// Other events generated by the underlying behaviour are transparently
						// passed through.
						ToSwarm::Dial { opts } => {
							if opts.get_peer_id().is_none() {
								log::error!(
									target: LOG_TARGET,
									"The request-response isn't supposed to start dialing addresses"
								);
							}
							return Poll::Ready(ToSwarm::Dial { opts });
						},
						event => {
							return Poll::Ready(
								event.map_in(|event| ((*protocol).to_string(), event)).map_out(
									|_| {
										unreachable!(
											"`GenerateEvent` is handled in a branch above; qed"
										)
									},
								),
							);
						},
					};

					match ev {
						// Received a request from a remote.
						request_response::Event::Message {
							peer,
							message: Message::Request { request_id, request, channel, .. },
						} => {
							self.pending_responses_arrival_time
								.insert((protocol.clone(), request_id).into(), Instant::now());

							let reputation = self.peer_store.peer_reputation(&peer.into());

							if reputation < BANNED_THRESHOLD {
								log::debug!(
									target: LOG_TARGET,
									"Cannot handle requests from a node with a low reputation {}: {}",
									peer,
									reputation,
								);
								continue 'poll_protocol;
							}

							let (tx, rx) = oneshot::channel();

							// Submit the request to the "response builder" passed by the user at
							// initialization.
							if let Some(resp_builder) = inbound_queue {
								// If the response builder is too busy, silently drop `tx`. This
								// will be reported by the corresponding request-response
								// [`Behaviour`] through an `InboundFailure::Omission` event.
								// Note that we use `async_channel::bounded` and not `mpsc::channel`
								// because the latter allocates an extra slot for every cloned
								// sender.
								let _ = resp_builder.try_send(IncomingRequest {
									peer: peer.into(),
									payload: request,
									pending_response: tx,
								});
							} else {
								debug_assert!(false, "Received message on outbound-only protocol.");
							}

							let protocol = protocol.clone();

							self.pending_responses.push(Box::pin(async move {
								// The `tx` created above can be dropped if we are not capable of
								// processing this request, which is reflected as a
								// `InboundFailure::Omission` event.
								rx.await.map_or(None, |response| {
									Some(RequestProcessingOutcome {
										peer,
										request_id,
										protocol,
										inner_channel: channel,
										response,
									})
								})
							}));

							// This `continue` makes sure that `pending_responses` gets polled
							// after we have added the new element.
							continue 'poll_all;
						},

						// Received a response from a remote to one of our requests.
						request_response::Event::Message {
							peer,
							message: Message::Response { request_id, response },
							..
						} => {
							let (started, delivered) = match self
								.pending_requests
								.remove(&(protocol.clone(), request_id).into())
							{
								Some(PendingRequest {
									started_at,
									response_tx: Some(response_tx),
									..
								}) => {
									log::trace!(
										target: LOG_TARGET,
										"received response from {peer} ({protocol:?}), {} bytes",
										response.as_ref().map_or(0usize, |response| response.len()),
									);

									let delivered = response_tx
										.send(
											response
												.map_err(|()| RequestFailure::Refused)
												.map(|resp| (resp, protocol.clone())),
										)
										.map_err(|_| RequestFailure::Obsolete);
									(started_at, delivered)
								},
								_ => {
									log::debug!(
										target: LOG_TARGET,
										"Received `RequestResponseEvent::Message` with unexpected request id {:?} from {:?}",
										request_id,
										peer,
									);
									continue;
								},
							};

							let out = Event::RequestFinished {
								peer,
								protocol: protocol.clone(),
								duration: started.elapsed(),
								result: delivered,
							};

							return Poll::Ready(ToSwarm::GenerateEvent(out));
						},

						// One of our requests has failed.
						request_response::Event::OutboundFailure {
							peer,
							request_id,
							error,
							..
						} => {
							let error = OutboundFailure::from(error);
							let started = match self
								.pending_requests
								.remove(&(protocol.clone(), request_id).into())
							{
								Some(PendingRequest {
									started_at,
									response_tx: Some(response_tx),
									fallback_request,
								}) => {
									// Try using the fallback request if the protocol was not
									// supported.
									if matches!(error, OutboundFailure::UnsupportedProtocols) {
										if let Some((fallback_request, fallback_protocol)) =
											fallback_request
										{
											log::trace!(
												target: LOG_TARGET,
												"Request with id {:?} failed. Trying the fallback protocol. {}",
												request_id,
												fallback_protocol.deref()
											);
											fallback_requests.push((
												peer,
												fallback_protocol,
												fallback_request,
												response_tx,
											));
											continue;
										}
									}

									if response_tx
										.send(Err(RequestFailure::Network(error.clone())))
										.is_err()
									{
										log::debug!(
											target: LOG_TARGET,
											"Request with id {:?} failed. At the same time local \
											 node is no longer interested in the result.",
											request_id,
										);
									}
									started_at
								},
								_ => {
									log::debug!(
										target: LOG_TARGET,
										"Received `RequestResponseEvent::OutboundFailure` with unexpected request id {:?} error {:?} from {:?}",
										request_id,
										error,
										peer
									);
									continue;
								},
							};

							let out = Event::RequestFinished {
								peer,
								protocol: protocol.clone(),
								duration: started.elapsed(),
								result: Err(RequestFailure::Network(error)),
							};

							return Poll::Ready(ToSwarm::GenerateEvent(out));
						},

						// An inbound request failed, either while reading the request or due to
						// failing to send a response.
						request_response::Event::InboundFailure {
							request_id, peer, error, ..
						} => {
							self.pending_responses_arrival_time
								.remove(&(protocol.clone(), request_id).into());
							self.send_feedback.remove(&(protocol.clone(), request_id).into());
							let out = Event::InboundRequest {
								peer,
								protocol: protocol.clone(),
								result: Err(ResponseFailure::Network(error.into())),
							};
							return Poll::Ready(ToSwarm::GenerateEvent(out));
						},

						// A response to an inbound request has been sent.
						request_response::Event::ResponseSent { request_id, peer } => {
							let arrival_time = self
								.pending_responses_arrival_time
								.remove(&(protocol.clone(), request_id).into())
								.map(|t| t.elapsed())
								.expect(
									"Time is added for each inbound request on arrival and only \
									 removed on success (`ResponseSent`) or failure \
									 (`InboundFailure`). One can not receive a success event for a \
									 request that either never arrived, or that has previously \
									 failed; qed.",
								);

							if let Some(send_feedback) =
								self.send_feedback.remove(&(protocol.clone(), request_id).into())
							{
								let _ = send_feedback.send(());
							}

							let out = Event::InboundRequest {
								peer,
								protocol: protocol.clone(),
								result: Ok(arrival_time),
							};

							return Poll::Ready(ToSwarm::GenerateEvent(out));
						},
					};
				}
			}

			// Send out fallback requests.
			for (peer, protocol, request, pending_response) in fallback_requests.drain(..) {
				if let Some(ProtocolDetails { behaviour, .. }) = self.protocols.get_mut(&protocol) {
					Self::send_request_inner(
						behaviour,
						&mut self.pending_requests,
						&peer,
						protocol,
						request,
						None,
						pending_response,
						// We can error if not connected because the
						// previous attempt would have tried to establish a
						// connection already or errored and we wouldn't have gotten here.
						IfDisconnected::ImmediateError,
					);
				}
			}

			break Poll::Pending;
		}
	}
}

/// Error when registering a protocol.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
	/// A protocol has been specified multiple times.
	#[error("{0}")]
	DuplicateProtocol(ProtocolName),
}

/// Error when processing a request sent by a remote.
#[derive(Debug, thiserror::Error)]
pub enum ResponseFailure {
	/// Problem on the network.
	#[error("Problem on the network: {0}")]
	Network(InboundFailure),
}

/// Implements the libp2p [`Codec`] trait. Defines how streams of bytes are turned
/// into requests and responses and vice-versa.
#[derive(Debug, Clone)]
#[doc(hidden)] // Needs to be public in order to satisfy the Rust compiler.
pub struct GenericCodec {
	max_request_size: u64,
	max_response_size: u64,
}

#[async_trait::async_trait]
impl Codec for GenericCodec {
	type Protocol = ProtocolName;
	type Request = Vec<u8>;
	type Response = Result<Vec<u8>, ()>;

	async fn read_request<T>(
		&mut self,
		_: &Self::Protocol,
		mut io: &mut T,
	) -> io::Result<Self::Request>
	where
		T: AsyncRead + Unpin + Send,
	{
		// Read the length.
		let length = unsigned_varint::aio::read_usize(&mut io)
			.await
			.map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
		if length > usize::try_from(self.max_request_size).unwrap_or(usize::MAX) {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("Request size exceeds limit: {} > {}", length, self.max_request_size),
			));
		}

		// Read the payload.
		let mut buffer = vec![0; length];
		io.read_exact(&mut buffer).await?;
		Ok(buffer)
	}

	async fn read_response<T>(
		&mut self,
		_: &Self::Protocol,
		mut io: &mut T,
	) -> io::Result<Self::Response>
	where
		T: AsyncRead + Unpin + Send,
	{
		// Note that this function returns a `Result<Result<...>>`. Returning an `Err` is
		// considered as a protocol error and will result in the entire connection being closed.
		// Returning `Ok(Err(_))` signifies that a response has successfully been fetched, and
		// that this response is an error.

		// Read the length.
		let length = match unsigned_varint::aio::read_usize(&mut io).await {
			Ok(l) => l,
			Err(unsigned_varint::io::ReadError::Io(err))
				if matches!(err.kind(), io::ErrorKind::UnexpectedEof) =>
			{
				return Ok(Err(()))
			},
			Err(err) => return Err(io::Error::new(io::ErrorKind::InvalidInput, err)),
		};

		if length > usize::try_from(self.max_response_size).unwrap_or(usize::MAX) {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("Response size exceeds limit: {} > {}", length, self.max_response_size),
			));
		}

		// Read the payload.
		let mut buffer = vec![0; length];
		io.read_exact(&mut buffer).await?;
		Ok(Ok(buffer))
	}

	async fn write_request<T>(
		&mut self,
		_: &Self::Protocol,
		io: &mut T,
		req: Self::Request,
	) -> io::Result<()>
	where
		T: AsyncWrite + Unpin + Send,
	{
		// TODO: check the length?
		// Write the length.
		{
			let mut buffer = unsigned_varint::encode::usize_buffer();
			io.write_all(unsigned_varint::encode::usize(req.len(), &mut buffer)).await?;
		}

		// Write the payload.
		io.write_all(&req).await?;

		io.close().await?;
		Ok(())
	}

	async fn write_response<T>(
		&mut self,
		_: &Self::Protocol,
		io: &mut T,
		res: Self::Response,
	) -> io::Result<()>
	where
		T: AsyncWrite + Unpin + Send,
	{
		// If `res` is an `Err`, we jump to closing the substream without writing anything on it.
		if let Ok(res) = res {
			// TODO: check the length?
			// Write the length.
			{
				let mut buffer = unsigned_varint::encode::usize_buffer();
				io.write_all(unsigned_varint::encode::usize(res.len(), &mut buffer)).await?;
			}

			// Write the payload.
			io.write_all(&res).await?;
		}

		io.close().await?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use crate::mock::MockPeerStore;
	use assert_matches::assert_matches;
	use futures::channel::oneshot;
	use libp2p::{
		core::{
			transport::{MemoryTransport, Transport},
			upgrade,
		},
		identity::Keypair,
		noise,
		swarm::{Config as SwarmConfig, Executor, Swarm, SwarmEvent},
		Multiaddr,
	};
	use std::{iter, time::Duration};

	struct TokioExecutor;
	impl Executor for TokioExecutor {
		fn exec(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) {
			tokio::spawn(f);
		}
	}

	fn build_swarm(
		list: impl Iterator<Item = ProtocolConfig>,
	) -> (Swarm<RequestResponsesBehaviour>, Multiaddr) {
		let keypair = Keypair::generate_ed25519();

		let transport = MemoryTransport::new()
			.upgrade(upgrade::Version::V1)
			.authenticate(noise::Config::new(&keypair).unwrap())
			.multiplex(libp2p::yamux::Config::default())
			.boxed();

		let behaviour = RequestResponsesBehaviour::new(list, Arc::new(MockPeerStore {})).unwrap();

		let mut swarm = Swarm::new(
			transport,
			behaviour,
			keypair.public().to_peer_id(),
			SwarmConfig::with_executor(TokioExecutor {})
				// This is taken care of by notification protocols in non-test environment
				// It is very slow in test environment for some reason, hence larger timeout
				.with_idle_connection_timeout(Duration::from_secs(10)),
		);

		let listen_addr: Multiaddr = format!("/memory/{}", rand::random::<u64>()).parse().unwrap();

		swarm.listen_on(listen_addr.clone()).unwrap();

		(swarm, listen_addr)
	}

	#[tokio::test]
	async fn basic_request_response_works() {
		let protocol_name = ProtocolName::from("/test/req-resp/1");

		// Build swarms whose behaviour is [`RequestResponsesBehaviour`].
		let mut swarms = (0..2)
			.map(|_| {
				let (tx, mut rx) = async_channel::bounded::<IncomingRequest>(64);

				tokio::spawn(async move {
					while let Some(rq) = rx.next().await {
						let (fb_tx, fb_rx) = oneshot::channel();
						assert_eq!(rq.payload, b"this is a request");
						let _ = rq.pending_response.send(super::OutgoingResponse {
							result: Ok(b"this is a response".to_vec()),
							reputation_changes: Vec::new(),
							sent_feedback: Some(fb_tx),
						});
						fb_rx.await.unwrap();
					}
				});

				let protocol_config = ProtocolConfig {
					name: protocol_name.clone(),
					fallback_names: Vec::new(),
					max_request_size: 1024,
					max_response_size: 1024 * 1024,
					request_timeout: Duration::from_secs(30),
					inbound_queue: Some(tx),
				};

				build_swarm(iter::once(protocol_config))
			})
			.collect::<Vec<_>>();

		// Ask `swarm[0]` to dial `swarm[1]`. There isn't any discovery mechanism in place in
		// this test, so they wouldn't connect to each other.
		{
			let dial_addr = swarms[1].1.clone();
			Swarm::dial(&mut swarms[0].0, dial_addr).unwrap();
		}

		let (mut swarm, _) = swarms.remove(0);
		// Running `swarm[0]` in the background.
		tokio::spawn(async move {
			loop {
				match swarm.select_next_some().await {
					SwarmEvent::Behaviour(Event::InboundRequest { result, .. }) => {
						result.unwrap();
					},
					_ => {},
				}
			}
		});

		// Remove and run the remaining swarm.
		let (mut swarm, _) = swarms.remove(0);
		let mut response_receiver = None;

		loop {
			match swarm.select_next_some().await {
				SwarmEvent::ConnectionEstablished { peer_id, .. } => {
					let (sender, receiver) = oneshot::channel();
					swarm.behaviour_mut().send_request(
						&peer_id,
						protocol_name.clone(),
						b"this is a request".to_vec(),
						None,
						sender,
						IfDisconnected::ImmediateError,
					);
					assert!(response_receiver.is_none());
					response_receiver = Some(receiver);
				},
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					result.unwrap();
					break;
				},
				_ => {},
			}
		}

		assert_eq!(
			response_receiver.unwrap().await.unwrap().unwrap(),
			(b"this is a response".to_vec(), protocol_name)
		);
	}

	#[tokio::test]
	async fn max_response_size_exceeded() {
		let protocol_name = ProtocolName::from("/test/req-resp/1");

		// Build swarms whose behaviour is [`RequestResponsesBehaviour`].
		let mut swarms = (0..2)
			.map(|_| {
				let (tx, mut rx) = async_channel::bounded::<IncomingRequest>(64);

				tokio::spawn(async move {
					while let Some(rq) = rx.next().await {
						assert_eq!(rq.payload, b"this is a request");
						let _ = rq.pending_response.send(super::OutgoingResponse {
							result: Ok(b"this response exceeds the limit".to_vec()),
							reputation_changes: Vec::new(),
							sent_feedback: None,
						});
					}
				});

				let protocol_config = ProtocolConfig {
					name: protocol_name.clone(),
					fallback_names: Vec::new(),
					max_request_size: 1024,
					max_response_size: 8, // <-- important for the test
					request_timeout: Duration::from_secs(30),
					inbound_queue: Some(tx),
				};

				build_swarm(iter::once(protocol_config))
			})
			.collect::<Vec<_>>();

		// Ask `swarm[0]` to dial `swarm[1]`. There isn't any discovery mechanism in place in
		// this test, so they wouldn't connect to each other.
		{
			let dial_addr = swarms[1].1.clone();
			Swarm::dial(&mut swarms[0].0, dial_addr).unwrap();
		}

		// Running `swarm[0]` in the background until a `InboundRequest` event happens,
		// which is a hint about the test having ended.
		let (mut swarm, _) = swarms.remove(0);
		tokio::spawn(async move {
			loop {
				match swarm.select_next_some().await {
					SwarmEvent::Behaviour(Event::InboundRequest { result, .. }) => {
						assert!(result.is_ok());
					},
					SwarmEvent::ConnectionClosed { .. } => {
						break;
					},
					_ => {},
				}
			}
		});

		// Remove and run the remaining swarm.
		let (mut swarm, _) = swarms.remove(0);

		let mut response_receiver = None;

		loop {
			match swarm.select_next_some().await {
				SwarmEvent::ConnectionEstablished { peer_id, .. } => {
					let (sender, receiver) = oneshot::channel();
					swarm.behaviour_mut().send_request(
						&peer_id,
						protocol_name.clone(),
						b"this is a request".to_vec(),
						None,
						sender,
						IfDisconnected::ImmediateError,
					);
					assert!(response_receiver.is_none());
					response_receiver = Some(receiver);
				},
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					assert!(result.is_err());
					break;
				},
				_ => {},
			}
		}

		match response_receiver.unwrap().await.unwrap().unwrap_err() {
			RequestFailure::Network(OutboundFailure::Io(_)) => {},
			request_failure => panic!("Unexpected failure: {request_failure:?}"),
		}
	}

	/// A `RequestId` is a unique identifier among either all inbound or all outbound requests for
	/// a single [`RequestResponsesBehaviour`] behaviour. It is not guaranteed to be unique across
	/// multiple [`RequestResponsesBehaviour`] behaviours. Thus, when handling `RequestId` in the
	/// context of multiple [`RequestResponsesBehaviour`] behaviours, one needs to couple the
	/// protocol name with the `RequestId` to get a unique request identifier.
	///
	/// This test ensures that two requests on different protocols can be handled concurrently
	/// without a `RequestId` collision.
	///
	/// See [`ProtocolRequestId`] for additional information.
	#[tokio::test]
	async fn request_id_collision() {
		let protocol_name_1 = ProtocolName::from("/test/req-resp-1/1");
		let protocol_name_2 = ProtocolName::from("/test/req-resp-2/1");

		let mut swarm_1 = {
			let protocol_configs = vec![
				ProtocolConfig {
					name: protocol_name_1.clone(),
					fallback_names: Vec::new(),
					max_request_size: 1024,
					max_response_size: 1024 * 1024,
					request_timeout: Duration::from_secs(30),
					inbound_queue: None,
				},
				ProtocolConfig {
					name: protocol_name_2.clone(),
					fallback_names: Vec::new(),
					max_request_size: 1024,
					max_response_size: 1024 * 1024,
					request_timeout: Duration::from_secs(30),
					inbound_queue: None,
				},
			];

			build_swarm(protocol_configs.into_iter()).0
		};

		let (mut swarm_2, mut swarm_2_handler_1, mut swarm_2_handler_2, listen_add_2) = {
			let (tx_1, rx_1) = async_channel::bounded(64);
			let (tx_2, rx_2) = async_channel::bounded(64);

			let protocol_configs = vec![
				ProtocolConfig {
					name: protocol_name_1.clone(),
					fallback_names: Vec::new(),
					max_request_size: 1024,
					max_response_size: 1024 * 1024,
					request_timeout: Duration::from_secs(30),
					inbound_queue: Some(tx_1),
				},
				ProtocolConfig {
					name: protocol_name_2.clone(),
					fallback_names: Vec::new(),
					max_request_size: 1024,
					max_response_size: 1024 * 1024,
					request_timeout: Duration::from_secs(30),
					inbound_queue: Some(tx_2),
				},
			];

			let (swarm, listen_addr) = build_swarm(protocol_configs.into_iter());

			(swarm, rx_1, rx_2, listen_addr)
		};

		// Ask swarm 1 to dial swarm 2. There isn't any discovery mechanism in place in this test,
		// so they wouldn't connect to each other.
		swarm_1.dial(listen_add_2).unwrap();

		// Run swarm 2 in the background, receiving two requests.
		tokio::spawn(async move {
			loop {
				match swarm_2.select_next_some().await {
					SwarmEvent::Behaviour(Event::InboundRequest { result, .. }) => {
						result.unwrap();
					},
					_ => {},
				}
			}
		});

		// Handle both requests sent by swarm 1 to swarm 2 in the background.
		//
		// Make sure both requests overlap, by answering the first only after receiving the
		// second.
		tokio::spawn(async move {
			let protocol_1_request = swarm_2_handler_1.next().await;
			let protocol_2_request = swarm_2_handler_2.next().await;

			protocol_1_request
				.unwrap()
				.pending_response
				.send(OutgoingResponse {
					result: Ok(b"this is a response".to_vec()),
					reputation_changes: Vec::new(),
					sent_feedback: None,
				})
				.unwrap();
			protocol_2_request
				.unwrap()
				.pending_response
				.send(OutgoingResponse {
					result: Ok(b"this is a response".to_vec()),
					reputation_changes: Vec::new(),
					sent_feedback: None,
				})
				.unwrap();
		});

		// Have swarm 1 send two requests to swarm 2 and await responses.

		let mut response_receivers = None;
		let mut num_responses = 0;

		loop {
			match swarm_1.select_next_some().await {
				SwarmEvent::ConnectionEstablished { peer_id, .. } => {
					let (sender_1, receiver_1) = oneshot::channel();
					let (sender_2, receiver_2) = oneshot::channel();
					swarm_1.behaviour_mut().send_request(
						&peer_id,
						protocol_name_1.clone(),
						b"this is a request".to_vec(),
						None,
						sender_1,
						IfDisconnected::ImmediateError,
					);
					swarm_1.behaviour_mut().send_request(
						&peer_id,
						protocol_name_2.clone(),
						b"this is a request".to_vec(),
						None,
						sender_2,
						IfDisconnected::ImmediateError,
					);
					assert!(response_receivers.is_none());
					response_receivers = Some((receiver_1, receiver_2));
				},
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					num_responses += 1;
					result.unwrap();
					if num_responses == 2 {
						break;
					}
				},
				_ => {},
			}
		}
		let (response_receiver_1, response_receiver_2) = response_receivers.unwrap();
		assert_eq!(
			response_receiver_1.await.unwrap().unwrap(),
			(b"this is a response".to_vec(), protocol_name_1)
		);
		assert_eq!(
			response_receiver_2.await.unwrap().unwrap(),
			(b"this is a response".to_vec(), protocol_name_2)
		);
	}

	#[tokio::test]
	async fn request_fallback() {
		let protocol_name_1 = ProtocolName::from("/test/req-resp/2");
		let protocol_name_1_fallback = ProtocolName::from("/test/req-resp/1");
		let protocol_name_2 = ProtocolName::from("/test/another");

		let protocol_config_1 = ProtocolConfig {
			name: protocol_name_1.clone(),
			fallback_names: Vec::new(),
			max_request_size: 1024,
			max_response_size: 1024 * 1024,
			request_timeout: Duration::from_secs(30),
			inbound_queue: None,
		};
		let protocol_config_1_fallback = ProtocolConfig {
			name: protocol_name_1_fallback.clone(),
			fallback_names: Vec::new(),
			max_request_size: 1024,
			max_response_size: 1024 * 1024,
			request_timeout: Duration::from_secs(30),
			inbound_queue: None,
		};
		let protocol_config_2 = ProtocolConfig {
			name: protocol_name_2.clone(),
			fallback_names: Vec::new(),
			max_request_size: 1024,
			max_response_size: 1024 * 1024,
			request_timeout: Duration::from_secs(30),
			inbound_queue: None,
		};

		// This swarm only speaks protocol_name_1_fallback and protocol_name_2.
		// It only responds to requests.
		let mut older_swarm = {
			let (tx_1, mut rx_1) = async_channel::bounded::<IncomingRequest>(64);
			let (tx_2, mut rx_2) = async_channel::bounded::<IncomingRequest>(64);
			let mut protocol_config_1_fallback = protocol_config_1_fallback.clone();
			protocol_config_1_fallback.inbound_queue = Some(tx_1);

			let mut protocol_config_2 = protocol_config_2.clone();
			protocol_config_2.inbound_queue = Some(tx_2);

			tokio::spawn(async move {
				for _ in 0..2 {
					if let Some(rq) = rx_1.next().await {
						let (fb_tx, fb_rx) = oneshot::channel();
						assert_eq!(rq.payload, b"request on protocol /test/req-resp/1");
						let _ = rq.pending_response.send(super::OutgoingResponse {
							result: Ok(b"this is a response on protocol /test/req-resp/1".to_vec()),
							reputation_changes: Vec::new(),
							sent_feedback: Some(fb_tx),
						});
						fb_rx.await.unwrap();
					}
				}

				if let Some(rq) = rx_2.next().await {
					let (fb_tx, fb_rx) = oneshot::channel();
					assert_eq!(rq.payload, b"request on protocol /test/other");
					let _ = rq.pending_response.send(super::OutgoingResponse {
						result: Ok(b"this is a response on protocol /test/other".to_vec()),
						reputation_changes: Vec::new(),
						sent_feedback: Some(fb_tx),
					});
					fb_rx.await.unwrap();
				}
			});

			build_swarm(vec![protocol_config_1_fallback, protocol_config_2].into_iter())
		};

		// This swarm speaks all protocols.
		let mut new_swarm = build_swarm(
			vec![
				protocol_config_1.clone(),
				protocol_config_1_fallback.clone(),
				protocol_config_2.clone(),
			]
			.into_iter(),
		);

		{
			let dial_addr = older_swarm.1.clone();
			Swarm::dial(&mut new_swarm.0, dial_addr).unwrap();
		}

		// Running `older_swarm`` in the background.
		tokio::spawn(async move {
			loop {
				_ = older_swarm.0.select_next_some().await;
			}
		});

		// Run the newer swarm. Attempt to make requests on all protocols.
		let (mut swarm, _) = new_swarm;
		let mut older_peer_id = None;

		let mut response_receiver = None;
		// Try the new protocol with a fallback.
		loop {
			match swarm.select_next_some().await {
				SwarmEvent::ConnectionEstablished { peer_id, .. } => {
					older_peer_id = Some(peer_id);
					let (sender, receiver) = oneshot::channel();
					swarm.behaviour_mut().send_request(
						&peer_id,
						protocol_name_1.clone(),
						b"request on protocol /test/req-resp/2".to_vec(),
						Some((
							b"request on protocol /test/req-resp/1".to_vec(),
							protocol_config_1_fallback.name.clone(),
						)),
						sender,
						IfDisconnected::ImmediateError,
					);
					response_receiver = Some(receiver);
				},
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					result.unwrap();
					break;
				},
				_ => {},
			}
		}
		assert_eq!(
			response_receiver.unwrap().await.unwrap().unwrap(),
			(
				b"this is a response on protocol /test/req-resp/1".to_vec(),
				protocol_name_1_fallback.clone()
			)
		);
		// Try the old protocol with a useless fallback.
		let (sender, response_receiver) = oneshot::channel();
		swarm.behaviour_mut().send_request(
			older_peer_id.as_ref().unwrap(),
			protocol_name_1_fallback.clone(),
			b"request on protocol /test/req-resp/1".to_vec(),
			Some((
				b"dummy request, will fail if processed".to_vec(),
				protocol_config_1_fallback.name.clone(),
			)),
			sender,
			IfDisconnected::ImmediateError,
		);
		loop {
			match swarm.select_next_some().await {
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					result.unwrap();
					break;
				},
				_ => {},
			}
		}
		assert_eq!(
			response_receiver.await.unwrap().unwrap(),
			(
				b"this is a response on protocol /test/req-resp/1".to_vec(),
				protocol_name_1_fallback.clone()
			)
		);
		// Try the new protocol with no fallback. Should fail.
		let (sender, response_receiver) = oneshot::channel();
		swarm.behaviour_mut().send_request(
			older_peer_id.as_ref().unwrap(),
			protocol_name_1.clone(),
			b"request on protocol /test/req-resp-2".to_vec(),
			None,
			sender,
			IfDisconnected::ImmediateError,
		);
		loop {
			match swarm.select_next_some().await {
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					assert_matches!(
						result.unwrap_err(),
						RequestFailure::Network(OutboundFailure::UnsupportedProtocols)
					);
					break;
				},
				_ => {},
			}
		}
		assert!(response_receiver.await.unwrap().is_err());
		// Try the other protocol with no fallback.
		let (sender, response_receiver) = oneshot::channel();
		swarm.behaviour_mut().send_request(
			older_peer_id.as_ref().unwrap(),
			protocol_name_2.clone(),
			b"request on protocol /test/other".to_vec(),
			None,
			sender,
			IfDisconnected::ImmediateError,
		);
		loop {
			match swarm.select_next_some().await {
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					result.unwrap();
					break;
				},
				_ => {},
			}
		}
		assert_eq!(
			response_receiver.await.unwrap().unwrap(),
			(b"this is a response on protocol /test/other".to_vec(), protocol_name_2.clone())
		);
	}

	/// This test ensures the `RequestResponsesBehaviour` propagates back the Request::Timeout error
	/// even if the libp2p component hangs.
	///
	/// For testing purposes, the communication happens on the `/test/req-resp/1` protocol.
	///
	/// This is achieved by:
	/// - Two swarms are connected, the first one is slow to respond and has the timeout set to 10
	///   seconds. The second swarm is configured with a timeout of 10 seconds in libp2p, however in
	///   substrate this is set to 1 second.
	///
	/// - The first swarm introduces a delay of 2 seconds before responding to the request.
	///
	/// - The second swarm must enforce the 1 second timeout.
	#[tokio::test]
	async fn enforce_outbound_timeouts() {
		const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
		const REQUEST_TIMEOUT_SHORT: Duration = Duration::from_secs(1);

		// These swarms only speaks protocol_name.
		let protocol_name = ProtocolName::from("/test/req-resp/1");

		let protocol_config = ProtocolConfig {
			name: protocol_name.clone(),
			fallback_names: Vec::new(),
			max_request_size: 1024,
			max_response_size: 1024 * 1024,
			request_timeout: REQUEST_TIMEOUT, // <-- important for the test
			inbound_queue: None,
		};

		// Build swarms whose behaviour is [`RequestResponsesBehaviour`].
		let (mut first_swarm, _) = {
			let (tx, mut rx) = async_channel::bounded::<IncomingRequest>(64);

			tokio::spawn(async move {
				if let Some(rq) = rx.next().await {
					assert_eq!(rq.payload, b"this is a request");

					// Sleep for more than `REQUEST_TIMEOUT_SHORT` and less than
					// `REQUEST_TIMEOUT`.
					tokio::time::sleep(REQUEST_TIMEOUT_SHORT * 2).await;

					// By the time the response is sent back, the second swarm
					// received Timeout.
					let _ = rq.pending_response.send(super::OutgoingResponse {
						result: Ok(b"Second swarm already timedout".to_vec()),
						reputation_changes: Vec::new(),
						sent_feedback: None,
					});
				}
			});

			let mut protocol_config = protocol_config.clone();
			protocol_config.inbound_queue = Some(tx);

			build_swarm(iter::once(protocol_config))
		};

		let (mut second_swarm, second_address) = {
			let (tx, mut rx) = async_channel::bounded::<IncomingRequest>(64);

			tokio::spawn(async move {
				while let Some(rq) = rx.next().await {
					let _ = rq.pending_response.send(super::OutgoingResponse {
						result: Ok(b"This is the response".to_vec()),
						reputation_changes: Vec::new(),
						sent_feedback: None,
					});
				}
			});
			let mut protocol_config = protocol_config.clone();
			protocol_config.inbound_queue = Some(tx);

			build_swarm(iter::once(protocol_config.clone()))
		};
		// Modify the second swarm to have a shorter timeout.
		second_swarm
			.behaviour_mut()
			.protocols
			.get_mut(&protocol_name)
			.unwrap()
			.request_timeout = REQUEST_TIMEOUT_SHORT;

		// Ask first swarm to dial the second swarm.
		{
			Swarm::dial(&mut first_swarm, second_address).unwrap();
		}

		// Running the first swarm in the background until a `InboundRequest` event happens,
		// which is a hint about the test having ended.
		tokio::spawn(async move {
			loop {
				let event = first_swarm.select_next_some().await;
				match event {
					SwarmEvent::Behaviour(Event::InboundRequest { result, .. }) => {
						assert!(result.is_ok());
						break;
					},
					SwarmEvent::ConnectionClosed { .. } => {
						break;
					},
					_ => {},
				}
			}
		});

		// Run the second swarm.
		// - on connection established send the request to the first swarm
		// - expect to receive a timeout
		let mut response_receiver = None;
		loop {
			let event = second_swarm.select_next_some().await;

			match event {
				SwarmEvent::ConnectionEstablished { peer_id, .. } => {
					let (sender, receiver) = oneshot::channel();
					second_swarm.behaviour_mut().send_request(
						&peer_id,
						protocol_name.clone(),
						b"this is a request".to_vec(),
						None,
						sender,
						IfDisconnected::ImmediateError,
					);
					assert!(response_receiver.is_none());
					response_receiver = Some(receiver);
				},
				SwarmEvent::ConnectionClosed { .. } => {
					break;
				},
				SwarmEvent::Behaviour(Event::RequestFinished { result, .. }) => {
					assert!(result.is_err());
					break;
				},
				_ => {},
			}
		}

		// Expect the timeout.
		match response_receiver.unwrap().await.unwrap().unwrap_err() {
			RequestFailure::Network(OutboundFailure::Timeout) => {},
			request_failure => panic!("Unexpected failure: {request_failure:?}"),
		}
	}
}
