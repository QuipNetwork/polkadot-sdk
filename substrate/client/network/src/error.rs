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

//! Substrate network possible errors.

use crate::{config::TransportConfig, types::ProtocolName};

use sc_network_types::{multiaddr::Multiaddr, PeerId};

use std::fmt;

/// Result type alias for the network.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for the network.
#[derive(thiserror::Error)]
pub enum Error {
	/// Io error
	#[error(transparent)]
	Io(#[from] std::io::Error),

	/// Client error
	#[error(transparent)]
	Client(#[from] Box<sp_blockchain::Error>),
	/// The same bootnode (based on address) is registered with two different peer ids.
	#[error(
		"The same bootnode (`{address}`) is registered with two different peer ids: `{first_id}` and `{second_id}`"
	)]
	DuplicateBootnode {
		/// The address of the bootnode.
		address: Multiaddr,
		/// The first peer id that was found for the bootnode.
		first_id: PeerId,
		/// The second peer id that was found for the bootnode.
		second_id: PeerId,
	},
	/// Prometheus metrics error.
	#[error(transparent)]
	Prometheus(#[from] prometheus_endpoint::PrometheusError),
	/// The network addresses are invalid because they don't match the transport.
	#[error(
		"The following addresses are invalid because they don't match the transport: {addresses:?}"
	)]
	AddressesForAnotherTransport {
		/// Transport used.
		transport: TransportConfig,
		/// The invalid addresses.
		addresses: Vec<Multiaddr>,
	},
	/// An invalid `webrtc-direct` address.
	#[error(
		"Invalid WebRTC address `{address}`: expected `/<host>/udp/<port>/webrtc-direct` and \
		 nothing after it."
	)]
	InvalidWebRtcAddress {
		/// The invalid address.
		address: Multiaddr,
	},
	/// A public `webrtc-direct` address was configured without a WebRTC transport to back it.
	#[error(
		"A WebRTC public address has been configured, but the node does not listen on any \
		 `webrtc-direct` address"
	)]
	WebRtcTransportNotConfigured,
	/// A `webrtc-direct` address was configured on a backend that cannot serve WebRTC.
	#[error(
		"A WebRTC address is configured, \
		 but WebRTC is only supported by the litep2p network backend."
	)]
	WebRtcNotSupportedByBackend,
	/// The same request-response protocol has been registered multiple times.
	#[error("Request-response protocol registered multiple times: {protocol}")]
	DuplicateRequestResponseProtocol {
		/// Name of the protocol registered multiple times.
		protocol: ProtocolName,
	},
	/// Peer does not exist.
	#[error("Peer `{0}` does not exist.")]
	PeerDoesntExist(PeerId),
	/// Channel closed.
	#[error("Channel closed")]
	ChannelClosed,
	/// Connection closed.
	#[error("Connection closed")]
	ConnectionClosed,
	/// Litep2p error.
	#[error("Litep2p error: `{0}`")]
	Litep2p(litep2p::Error),
}

// Make `Debug` use the `Display` implementation.
impl fmt::Debug for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, f)
	}
}
