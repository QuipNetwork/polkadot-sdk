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

//! Overview over request/responses as used in `Polkadot`.
//!
//! `enum Protocol` .... List of all supported protocols.
//!
//! `enum Requests`  .... List of all supported requests, each entry matches one in protocols, but
//! has the actual request as payload.
//!
//! `struct IncomingRequest` .... wrapper for incoming requests, containing a sender for sending
//! responses.
//!
//! `struct OutgoingRequest` .... wrapper for outgoing requests, containing a sender used by the
//! networking code for delivering responses/delivery errors.
//!
//! `trait IsRequest` .... A trait describing a particular request. It is used for gathering meta
//! data, like what is the corresponding response type.
//!
//!  ## Versioning
//!
//! Versioning for request-response protocols can be done in multiple ways.
//!
//! If you're just changing the protocol name but the binary payloads are the same, just add a new
//! `fallback_name` to the protocol config.
//!
//! One way in which versioning has historically been achieved for req-response protocols is to
//! bundle the new req-resp version with an upgrade of a notifications protocol. The subsystem would
//! then know which request version to use based on stored data about the peer's notifications
//! protocol version.
//!
//! When bumping a notifications protocol version is not needed/desirable, you may add a new
//! req-resp protocol and set the old request as a fallback (see
//! `OutgoingRequest::new_with_fallback`). A request with the new version will be attempted and if
//! the protocol is refused by the peer, the fallback protocol request will be used.
//! Information about the actually used protocol will be returned alongside the raw response, so
//! that you know how to decode it.

use std::{collections::HashMap, time::Duration, u64};

use sc_network::{NetworkBackend, MAX_RESPONSE_SIZE};
use sp_runtime::traits::Block;
use strum::{EnumIter, IntoEnumIterator};

pub use sc_network::{config as network, config::RequestResponseConfig, ProtocolName};

/// Everything related to handling of incoming requests.
pub mod incoming;
/// Everything related to handling of outgoing requests.
pub mod outgoing;

pub use incoming::{IncomingRequest, IncomingRequestReceiver};

pub use outgoing::{OutgoingRequest, OutgoingResult, Recipient, Requests, ResponseSender};

///// Multiplexer for incoming requests.
// pub mod multiplexer;

/// Actual versioned requests and responses that are sent over the wire.
pub mod v1;

/// Actual versioned requests and responses that are sent over the wire.
pub mod v2;

/// Actual versioned requests and responses that are sent over the wire.
pub mod v3;

/// A protocol per subsystem seems to make the most sense, this way we don't need any dispatching
/// within protocols.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, EnumIter)]
pub enum Protocol {
	/// Protocol for chunk fetching, used by availability distribution and availability recovery.
	ChunkFetchingV1,
	/// Protocol for fetching collations from collators.
	CollationFetchingV1,
	/// Protocol for fetching collations from collators when async backing is enabled.
	CollationFetchingV2,
	/// Protocol for fetching collations from collators by output head data hash.
	CollationFetchingV3,
	/// Protocol for fetching seconded PoVs from validators of the same group.
	PoVFetchingV1,
	/// Protocol for fetching available data.
	AvailableDataFetchingV1,
	/// Sending of dispute statements with application level confirmations.
	DisputeSendingV1,

	/// Protocol for requesting candidates with attestations in statement distribution
	/// when async backing is enabled.
	AttestedCandidateV2,

	/// Protocol for chunk fetching version 2, used by availability distribution and availability
	/// recovery.
	ChunkFetchingV2,
}

/// Minimum bandwidth we expect for validators - 500Mbit/s is the recommendation, so approximately
/// 50MB per second:
const MIN_BANDWIDTH_BYTES: u64 = 50 * 1024 * 1024;

/// Default request timeout in seconds.
///
/// When decreasing this value, take into account that the very first request might need to open a
/// connection, which can be slow. If this causes problems, we should ensure connectivity via peer
/// sets.
#[allow(dead_code)]
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Request timeout where we can assume the connection is already open (e.g. we have peers in a
/// peer set as well).
const DEFAULT_REQUEST_TIMEOUT_CONNECTED: Duration = Duration::from_secs(1);

/// Timeout for requesting availability chunks.
pub const CHUNK_REQUEST_TIMEOUT: Duration = DEFAULT_REQUEST_TIMEOUT_CONNECTED;

/// This timeout is based on the following parameters, assuming we use asynchronous backing with no
/// time budget within a relay block:
/// - 500 Mbit/s networking speed
/// - 10 MB PoV
/// - 10 parallel executions
const POV_REQUEST_TIMEOUT_CONNECTED: Duration = Duration::from_millis(2000);

/// We want attested candidate requests to time out relatively fast,
/// because slow requests will bottleneck the backing system. Ideally, we'd have
/// an adaptive timeout based on the candidate size, because there will be a lot of variance
/// in candidate sizes: candidates with no code and no messages vs candidates with code
/// and messages.
///
/// We supply leniency because there are often large candidates and asynchronous
/// backing allows them to be included over a longer window of time. Exponential back-off
/// up to a maximum of 10 seconds would be ideal, but isn't supported by the
/// infrastructure here yet: see https://github.com/paritytech/polkadot/issues/6009
const ATTESTED_CANDIDATE_TIMEOUT: Duration = Duration::from_millis(2500);

/// We don't want a slow peer to slow down all the others, at the same time we want to get out the
/// data quickly in full to at least some peers (as this will reduce load on us as they then can
/// start serving the data). So this value is a tradeoff. 5 seems to be sensible. So we would need
/// to have 5 slow nodes connected, to delay transfer for others by `ATTESTED_CANDIDATE_TIMEOUT`.
pub const MAX_PARALLEL_ATTESTED_CANDIDATE_REQUESTS: u32 = 5;

/// Response size limit for responses of POV like data.
///
/// Same as what we use in substrate networking.
const POV_RESPONSE_SIZE: u64 = MAX_RESPONSE_SIZE;

/// Maximum response sizes for `AttestedCandidateV2`.
///
/// Chosen as a safe upper bound above the governance ceiling on validation code size
/// (`polkadot_primitives::MAX_CODE_SIZE`), leaving headroom for backing statements and
/// protocol overhead. This is a transport-level DoS cap only; the effective policy is
/// enforced by the runtime and the node-side inclusion emulator against the on-chain
/// `HostConfiguration.max_code_size`.
const ATTESTED_CANDIDATE_RESPONSE_SIZE: u64 = 8 * 1024 * 1024;

/// We can have relative large timeouts here, there is no value of hitting a
/// timeout as we want to get statements through to each node in any case.
pub const DISPUTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

impl Protocol {
	/// Get a configuration for a given Request response protocol.
	///
	/// Returns a `ProtocolConfig` for this protocol.
	/// Use this if you plan only to send requests for this protocol.
	pub fn get_outbound_only_config<B: Block, N: NetworkBackend<B, <B as Block>::Hash>>(
		self,
		req_protocol_names: &ReqProtocolNames,
	) -> N::RequestResponseProtocolConfig {
		self.create_config::<B, N>(req_protocol_names, None)
	}

	/// Get a configuration for a given Request response protocol.
	///
	/// Returns a receiver for messages received on this protocol and the requested
	/// `ProtocolConfig`.
	pub fn get_config<B: Block, N: NetworkBackend<B, <B as Block>::Hash>>(
		self,
		req_protocol_names: &ReqProtocolNames,
	) -> (async_channel::Receiver<network::IncomingRequest>, N::RequestResponseProtocolConfig) {
		let (tx, rx) = async_channel::bounded(self.get_channel_size());
		let cfg = self.create_config::<B, N>(req_protocol_names, Some(tx));
		(rx, cfg)
	}

	fn create_config<B: Block, N: NetworkBackend<B, <B as Block>::Hash>>(
		self,
		req_protocol_names: &ReqProtocolNames,
		tx: Option<async_channel::Sender<network::IncomingRequest>>,
	) -> N::RequestResponseProtocolConfig {
		let name = req_protocol_names.get_name(self);
		let legacy_names = self.get_legacy_name().into_iter().map(Into::into).collect();
		match self {
			Protocol::ChunkFetchingV1 | Protocol::ChunkFetchingV2 => N::request_response_config(
				name,
				legacy_names,
				1_000,
				POV_RESPONSE_SIZE,
				// We are connected to all validators:
				CHUNK_REQUEST_TIMEOUT,
				tx,
			),
			Protocol::CollationFetchingV1 |
			Protocol::CollationFetchingV2 |
			Protocol::CollationFetchingV3 => {
				N::request_response_config(
					name,
					legacy_names,
					1_000,
					POV_RESPONSE_SIZE,
					// Taken from initial implementation in collator protocol:
					POV_REQUEST_TIMEOUT_CONNECTED,
					tx,
				)
			},
			Protocol::PoVFetchingV1 => N::request_response_config(
				name,
				legacy_names,
				1_000,
				POV_RESPONSE_SIZE,
				POV_REQUEST_TIMEOUT_CONNECTED,
				tx,
			),
			Protocol::AvailableDataFetchingV1 => N::request_response_config(
				name,
				legacy_names,
				1_000,
				// Available data size is dominated by the PoV size.
				POV_RESPONSE_SIZE,
				POV_REQUEST_TIMEOUT_CONNECTED,
				tx,
			),
			Protocol::DisputeSendingV1 => N::request_response_config(
				name,
				legacy_names,
				1_000,
				// Responses are just confirmation, in essence not even a bit. So 100 seems
				// plenty.
				100,
				DISPUTE_REQUEST_TIMEOUT,
				tx,
			),
			Protocol::AttestedCandidateV2 => N::request_response_config(
				name,
				legacy_names,
				1_000,
				ATTESTED_CANDIDATE_RESPONSE_SIZE,
				ATTESTED_CANDIDATE_TIMEOUT,
				tx,
			),
		}
	}

	// Channel sizes for the supported protocols.
	fn get_channel_size(self) -> usize {
		match self {
			// Hundreds of validators will start requesting their chunks once they see a candidate
			// awaiting availability on chain. Given that they will see that block at different
			// times (due to network delays), 100 seems big enough to accommodate for "bursts",
			// assuming we can service requests relatively quickly, which would need to be measured
			// as well.
			Protocol::ChunkFetchingV1 | Protocol::ChunkFetchingV2 => 100,
			// 10 seems reasonable, considering group sizes of max 10 validators.
			Protocol::CollationFetchingV1 |
			Protocol::CollationFetchingV2 |
			Protocol::CollationFetchingV3 => 10,
			// 10 seems reasonable, considering group sizes of max 10 validators.
			Protocol::PoVFetchingV1 => 10,
			// Validators are constantly self-selecting to request available data which may lead
			// to constant load and occasional burstiness.
			Protocol::AvailableDataFetchingV1 => 100,
			// Incoming requests can get bursty, we should also be able to handle them fast on
			// average, so something in the ballpark of 100 should be fine. Nodes will retry on
			// failure, so having a good value here is mostly about performance tuning.
			Protocol::DisputeSendingV1 => 100,

			Protocol::AttestedCandidateV2 => {
				// We assume we can utilize up to 70% of the available bandwidth for statements.
				// This is just a guess/estimate, with the following considerations: If we are
				// faster than that, queue size will stay low anyway, even if not - requesters will
				// get an immediate error, but if we are slower, requesters will run in a timeout -
				// wasting precious time.
				let available_bandwidth = 7 * MIN_BANDWIDTH_BYTES / 10;
				let size = u64::saturating_sub(
					ATTESTED_CANDIDATE_TIMEOUT.as_millis() as u64 * available_bandwidth /
						(1000 * ATTESTED_CANDIDATE_RESPONSE_SIZE),
					MAX_PARALLEL_ATTESTED_CANDIDATE_REQUESTS as u64,
				);
				debug_assert!(
					size > 0,
					"We should have a channel size greater zero, otherwise we won't accept any requests."
				);
				size as usize
			},
		}
	}

	/// Legacy protocol name associated with each peer set, if any.
	/// The request will be tried on this legacy protocol name if the remote refuses to speak the
	/// protocol.
	const fn get_legacy_name(self) -> Option<&'static str> {
		match self {
			Protocol::ChunkFetchingV1 => Some("/polkadot/req_chunk/1"),
			Protocol::CollationFetchingV1 => Some("/polkadot/req_collation/1"),
			Protocol::PoVFetchingV1 => Some("/polkadot/req_pov/1"),
			Protocol::AvailableDataFetchingV1 => Some("/polkadot/req_available_data/1"),
			Protocol::DisputeSendingV1 => Some("/polkadot/send_dispute/1"),

			// Introduced after legacy names became legacy.
			Protocol::AttestedCandidateV2 => None,
			Protocol::CollationFetchingV2 => None,
			Protocol::ChunkFetchingV2 => None,
			Protocol::CollationFetchingV3 => None,
		}
	}
}

/// Common properties of any `Request`.
pub trait IsRequest {
	/// Each request has a corresponding `Response`.
	type Response;

	/// What protocol this `Request` implements.
	const PROTOCOL: Protocol;
}

/// Type for getting on the wire [`Protocol`] names using genesis hash & fork id.
#[derive(Clone)]
pub struct ReqProtocolNames {
	names: HashMap<Protocol, ProtocolName>,
}

impl ReqProtocolNames {
	/// Construct [`ReqProtocolNames`] from `genesis_hash` and `fork_id`.
	pub fn new<Hash: AsRef<[u8]>>(genesis_hash: Hash, fork_id: Option<&str>) -> Self {
		let mut names = HashMap::new();
		for protocol in Protocol::iter() {
			names.insert(protocol, Self::generate_name(protocol, &genesis_hash, fork_id));
		}
		Self { names }
	}

	/// Get on the wire [`Protocol`] name.
	pub fn get_name(&self, protocol: Protocol) -> ProtocolName {
		self.names
			.get(&protocol)
			.expect("All `Protocol` enum variants are added above via `strum`; qed")
			.clone()
	}

	/// Protocol name of this protocol based on `genesis_hash` and `fork_id`.
	fn generate_name<Hash: AsRef<[u8]>>(
		protocol: Protocol,
		genesis_hash: &Hash,
		fork_id: Option<&str>,
	) -> ProtocolName {
		let prefix = if let Some(fork_id) = fork_id {
			format!("/{}/{}", hex::encode(genesis_hash), fork_id)
		} else {
			format!("/{}", hex::encode(genesis_hash))
		};

		let short_name = match protocol {
			// V1:
			Protocol::ChunkFetchingV1 => "/req_chunk/1",
			Protocol::CollationFetchingV1 => "/req_collation/1",
			Protocol::PoVFetchingV1 => "/req_pov/1",
			Protocol::AvailableDataFetchingV1 => "/req_available_data/1",
			Protocol::DisputeSendingV1 => "/send_dispute/1",

			// V2:
			Protocol::CollationFetchingV2 => "/req_collation/2",
			Protocol::AttestedCandidateV2 => "/req_attested_candidate/2",
			Protocol::ChunkFetchingV2 => "/req_chunk/2",
			// V3:
			Protocol::CollationFetchingV3 => "/req_collation/3",
		};

		format!("{}{}", prefix, short_name).into()
	}
}
