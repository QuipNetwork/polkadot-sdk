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

//! Bitswap request API.

use super::{is_cid_supported, Cid};

use bytes::Bytes;
use std::collections::HashSet;
use tokio::sync::mpsc;

/// Bitswap request errors.
#[derive(Debug, thiserror::Error)]
pub enum BitswapError {
	/// The service is unavailable.
	#[error("Bitswap service is closed")]
	ServiceClosed,
	/// A CID is unsupported.
	#[error("invalid CID for Bitswap: {cid}")]
	InvalidCid {
		/// Unsupported CID.
		cid: Cid,
	},
	/// The service is at capacity.
	#[error("Bitswap service is overloaded")]
	Overloaded,
}

/// A fetched block or request error. Block bytes are [`Bytes`] because a block may be
/// delivered to multiple concurrent requests without copying.
pub type FetchItem = Result<(Cid, Bytes), BitswapError>;

/// Handle for submitting Bitswap requests.
#[derive(Debug, Clone)]
pub struct BitswapHandle {
	cmd_tx: mpsc::Sender<BitswapCommand>,
}

impl BitswapHandle {
	pub(crate) fn new(cmd_tx: mpsc::Sender<BitswapCommand>) -> Self {
		Self { cmd_tx }
	}

	/// Submit a wantlist. Returns a receiver that yields `Ok((cid, bytes))` with
	/// hash-verified bytes for each requested CID, in the order they resolve.
	///
	/// The stream closes once every CID has been delivered. Unresolved CIDs are retried
	/// until the receiver is dropped.
	///
	/// `Err(BitswapError::ServiceClosed)` is yielded once, as the final item, if the
	/// service shuts down mid-request.
	pub fn request_stream(
		&self,
		cids: HashSet<Cid>,
	) -> Result<mpsc::Receiver<FetchItem>, BitswapError> {
		if cids.is_empty() {
			let (_tx, rx) = mpsc::channel(1);
			return Ok(rx);
		}

		for cid in &cids {
			if !is_cid_supported(cid) {
				return Err(BitswapError::InvalidCid { cid: *cid });
			}
		}

		let (sink, rx) = mpsc::channel(cids.len() + 1);

		self.cmd_tx.try_send(BitswapCommand::RequestStream { cids, sink }).map_err(
			|e| match e {
				mpsc::error::TrySendError::Full(_) => BitswapError::Overloaded,
				mpsc::error::TrySendError::Closed(_) => BitswapError::ServiceClosed,
			},
		)?;

		Ok(rx)
	}
}

#[derive(Debug)]
pub(crate) enum BitswapCommand {
	RequestStream { cids: HashSet<Cid>, sink: mpsc::Sender<FetchItem> },
}
