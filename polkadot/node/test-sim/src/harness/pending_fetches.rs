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

//! Side table of `oneshot::Sender`s the harness extracted from outgoing
//! `NetworkBridgeTxMessage::SendRequests` messages.
//!
//! When the subsystem fires a fetch, the classifier hands the embedded
//! `OutgoingRequest::pending_response` to this table. Each entry is keyed by an opaque
//! [`RequestId`] which surfaces in the corresponding `Effect::SendRequest`. Tests then drive
//! the response via `Sim::respond_fetch(request_id, payload)`.
//!
//! [`RequestId`]: crate::contract::RequestId

use crate::contract::RequestId;
use polkadot_node_network_protocol::request_response::ResponseSender;
use sc_network::ProtocolName;
use std::collections::HashMap;

/// Side table of pending request response senders keyed by [`RequestId`].
#[derive(Default)]
pub struct PendingFetches {
	next_id: u64,
	by_id: HashMap<RequestId, ResponseSender>,
}

impl PendingFetches {
	/// Empty pending-fetches table.
	pub fn new() -> Self {
		Self::default()
	}

	/// Allocate a fresh [`RequestId`] and store the response sender.
	pub fn register(&mut self, sender: ResponseSender) -> RequestId {
		let id = RequestId(self.next_id);
		self.next_id += 1;
		self.by_id.insert(id, sender);
		id
	}

	/// Take ownership of the response sender for `id`. Returns `None` if no such pending
	/// fetch exists (already responded, or unknown id).
	pub fn take(&mut self, id: RequestId) -> Option<ResponseSender> {
		self.by_id.remove(&id)
	}

	/// Number of pending fetches currently outstanding.
	pub fn len(&self) -> usize {
		self.by_id.len()
	}

	/// Whether there are any pending fetches.
	pub fn is_empty(&self) -> bool {
		self.by_id.is_empty()
	}
}

/// Convenience: raw response shape the subsystem expects on the oneshot.
pub type RawResponse = std::result::Result<(Vec<u8>, ProtocolName), sc_network::RequestFailure>;
