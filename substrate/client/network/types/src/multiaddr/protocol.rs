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

use crate::multihash::Multihash;
use libp2p_identity::PeerId;
use litep2p::types::multiaddr::Protocol as LiteP2pProtocol;
use multiaddr::Protocol as LibP2pProtocol;
use std::{
	borrow::Cow,
	fmt::{self, Debug, Display},
	net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

// Log target for this file.
const LOG_TARGET: &str = "sub-libp2p";

/// [`Protocol`] describes all possible multiaddress protocols.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Protocol<'a> {
	Dccp(u16),
	Dns(Cow<'a, str>),
	Dns4(Cow<'a, str>),
	Dns6(Cow<'a, str>),
	Dnsaddr(Cow<'a, str>),
	Http,
	Https,
	Ip4(Ipv4Addr),
	Ip6(Ipv6Addr),
	P2pWebRtcDirect,
	P2pWebRtcStar,
	WebRTCDirect,
	Certhash(Multihash),
	P2pWebSocketStar,
	/// Contains the "port" to contact. Similar to TCP or UDP, 0 means "assign me a port".
	Memory(u64),
	Onion(Cow<'a, [u8; 10]>, u16),
	Onion3(Cow<'a, [u8; 35]>, u16),
	P2p(Multihash),
	P2pCircuit,
	Quic,
	QuicV1,
	Sctp(u16),
	Tcp(u16),
	Tls,
	Noise,
	Udp(u16),
	Udt,
	Unix(Cow<'a, str>),
	Utp,
	Ws(Cow<'a, str>),
	Wss(Cow<'a, str>),
}

impl Display for Protocol<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let protocol = LiteP2pProtocol::from(self.clone());
		Display::fmt(&protocol, f)
	}
}

impl From<IpAddr> for Protocol<'_> {
	#[inline]
	fn from(addr: IpAddr) -> Self {
		match addr {
			IpAddr::V4(addr) => Protocol::Ip4(addr),
			IpAddr::V6(addr) => Protocol::Ip6(addr),
		}
	}
}

impl From<Ipv4Addr> for Protocol<'_> {
	#[inline]
	fn from(addr: Ipv4Addr) -> Self {
		Protocol::Ip4(addr)
	}
}

impl From<Ipv6Addr> for Protocol<'_> {
	#[inline]
	fn from(addr: Ipv6Addr) -> Self {
		Protocol::Ip6(addr)
	}
}

impl<'a> From<LibP2pProtocol<'a>> for Protocol<'a> {
	fn from(protocol: LibP2pProtocol<'a>) -> Self {
		match protocol {
			LibP2pProtocol::Dccp(port) => Protocol::Dccp(port),
			LibP2pProtocol::Dns(str) => Protocol::Dns(str),
			LibP2pProtocol::Dns4(str) => Protocol::Dns4(str),
			LibP2pProtocol::Dns6(str) => Protocol::Dns6(str),
			LibP2pProtocol::Dnsaddr(str) => Protocol::Dnsaddr(str),
			LibP2pProtocol::Http => Protocol::Http,
			LibP2pProtocol::Https => Protocol::Https,
			LibP2pProtocol::Ip4(ipv4_addr) => Protocol::Ip4(ipv4_addr),
			LibP2pProtocol::Ip6(ipv6_addr) => Protocol::Ip6(ipv6_addr),
			LibP2pProtocol::P2pWebRtcDirect => Protocol::P2pWebRtcDirect,
			LibP2pProtocol::P2pWebRtcStar => Protocol::P2pWebRtcStar,
			LibP2pProtocol::Certhash(multihash) => Protocol::Certhash(multihash.into()),
			LibP2pProtocol::P2pWebSocketStar => Protocol::P2pWebSocketStar,
			LibP2pProtocol::Memory(port) => Protocol::Memory(port),
			LibP2pProtocol::Onion(str, port) => Protocol::Onion(str, port),
			LibP2pProtocol::Onion3(addr) => Protocol::Onion3(Cow::Owned(*addr.hash()), addr.port()),
			LibP2pProtocol::P2p(peer_id) => Protocol::P2p((*peer_id.as_ref()).into()),
			LibP2pProtocol::P2pCircuit => Protocol::P2pCircuit,
			LibP2pProtocol::Quic => Protocol::Quic,
			LibP2pProtocol::QuicV1 => Protocol::QuicV1,
			LibP2pProtocol::Sctp(port) => Protocol::Sctp(port),
			LibP2pProtocol::Tcp(port) => Protocol::Tcp(port),
			LibP2pProtocol::Tls => Protocol::Tls,
			LibP2pProtocol::Noise => Protocol::Noise,
			LibP2pProtocol::Udp(port) => Protocol::Udp(port),
			LibP2pProtocol::Udt => Protocol::Udt,
			LibP2pProtocol::Unix(str) => Protocol::Unix(str),
			LibP2pProtocol::Utp => Protocol::Utp,
			LibP2pProtocol::Ws(str) => Protocol::Ws(str),
			LibP2pProtocol::Wss(str) => Protocol::Wss(str),
			LibP2pProtocol::WebRTCDirect => Protocol::WebRTCDirect,
			protocol => {
				log::error!(
					target: LOG_TARGET,
					"Got unsupported multiaddr protocol '{}'",
					protocol.tag(),
				);
				// Strictly speaking, this conversion is incorrect. But making protocol conversion
				// fallible would significantly complicate the client code. As DCCP transport is not
				// used by substrate, this conversion should be safe.
				// Also, as of `multiaddr-18.1`, all enum variants are actually covered.
				Protocol::Dccp(0)
			},
		}
	}
}

impl<'a> From<Protocol<'a>> for LibP2pProtocol<'a> {
	fn from(protocol: Protocol<'a>) -> Self {
		match protocol {
			Protocol::Dccp(port) => LibP2pProtocol::Dccp(port),
			Protocol::Dns(str) => LibP2pProtocol::Dns(str),
			Protocol::Dns4(str) => LibP2pProtocol::Dns4(str),
			Protocol::Dns6(str) => LibP2pProtocol::Dns6(str),
			Protocol::Dnsaddr(str) => LibP2pProtocol::Dnsaddr(str),
			Protocol::Http => LibP2pProtocol::Http,
			Protocol::Https => LibP2pProtocol::Https,
			Protocol::Ip4(ipv4_addr) => LibP2pProtocol::Ip4(ipv4_addr),
			Protocol::Ip6(ipv6_addr) => LibP2pProtocol::Ip6(ipv6_addr),
			Protocol::P2pWebRtcDirect => LibP2pProtocol::P2pWebRtcDirect,
			Protocol::P2pWebRtcStar => LibP2pProtocol::P2pWebRtcStar,
			// Protocol #280 is called `WebRTC` in multiaddr-17.0 and `WebRTCDirect` in
			// multiaddr-18.1.
			Protocol::WebRTCDirect => LibP2pProtocol::WebRTCDirect,
			Protocol::Certhash(multihash) => LibP2pProtocol::Certhash(multihash.into()),
			Protocol::P2pWebSocketStar => LibP2pProtocol::P2pWebSocketStar,
			Protocol::Memory(port) => LibP2pProtocol::Memory(port),
			Protocol::Onion(str, port) => LibP2pProtocol::Onion(str, port),
			Protocol::Onion3(str, port) => LibP2pProtocol::Onion3((str.into_owned(), port).into()),
			Protocol::P2p(multihash) => {
				LibP2pProtocol::P2p(PeerId::from_multihash(multihash.into()).unwrap_or_else(|_| {
					// This is better than making conversion fallible and complicating the
					// client code.
					log::error!(
						target: LOG_TARGET,
						"Received multiaddr with p2p multihash which is not a valid \
						 peer_id. Replacing with random peer_id."
					);
					PeerId::random()
				}))
			},
			Protocol::P2pCircuit => LibP2pProtocol::P2pCircuit,
			Protocol::Quic => LibP2pProtocol::Quic,
			Protocol::QuicV1 => LibP2pProtocol::QuicV1,
			Protocol::Sctp(port) => LibP2pProtocol::Sctp(port),
			Protocol::Tcp(port) => LibP2pProtocol::Tcp(port),
			Protocol::Tls => LibP2pProtocol::Tls,
			Protocol::Noise => LibP2pProtocol::Noise,
			Protocol::Udp(port) => LibP2pProtocol::Udp(port),
			Protocol::Udt => LibP2pProtocol::Udt,
			Protocol::Unix(str) => LibP2pProtocol::Unix(str),
			Protocol::Utp => LibP2pProtocol::Utp,
			Protocol::Ws(str) => LibP2pProtocol::Ws(str),
			Protocol::Wss(str) => LibP2pProtocol::Wss(str),
		}
	}
}
