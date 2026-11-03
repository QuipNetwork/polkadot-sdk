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

//! Indexed-transaction fetching over Bitswap.

use crate::RenewWant;
use cid::Cid;
use sc_network_bitswap::{BitswapError, BitswapHandle, FetchItem};
use sp_transaction_storage_proof::ContentHash;
use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, OnceLock},
	time::Duration,
};
use tokio::sync::mpsc;

const LOG_TARGET: &str = "storage-chain-fetcher";

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Source of Bitswap response streams.
pub trait BitswapRequest: Send + Sync {
	/// Requests blocks by CID. Successful stream items contain only requested CIDs.
	fn request_stream(&self, cids: HashSet<Cid>)
		-> Result<mpsc::Receiver<FetchItem>, BitswapError>;
}

impl BitswapRequest for BitswapHandle {
	fn request_stream(
		&self,
		cids: HashSet<Cid>,
	) -> Result<mpsc::Receiver<FetchItem>, BitswapError> {
		BitswapHandle::request_stream(self, cids)
	}
}

/// Late-bound Bitswap request source.
pub type BitswapHandleSlot = Arc<OnceLock<Arc<dyn BitswapRequest>>>;

/// Indexed-transaction fetch errors.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
	/// Bitswap is unavailable.
	#[error("bitswap unavailable: disabled on this node, or network not yet initialized")]
	BitswapUnavailable,
	/// The Bitswap request failed.
	#[error("bitswap service error: {0}")]
	Bitswap(#[from] BitswapError),
}

/// Fetches indexed transactions through Bitswap.
#[derive(Clone)]
pub struct IndexedTransactionFetcher {
	bitswap: BitswapHandleSlot,
}

impl IndexedTransactionFetcher {
	/// Creates a fetcher.
	pub fn new(bitswap: BitswapHandleSlot) -> Self {
		Self { bitswap }
	}

	/// Fetches a batch of indexed transactions.
	pub(crate) async fn fetch_many(
		&self,
		wants: &[RenewWant],
	) -> Result<HashMap<ContentHash, Vec<u8>>, FetchError> {
		if wants.is_empty() {
			return Ok(HashMap::new());
		}
		let handle = self.bitswap.get().ok_or(FetchError::BitswapUnavailable)?;

		let cids: HashSet<Cid> = wants.iter().copied().map(Cid::from).collect();

		let mut rx = match handle.request_stream(cids) {
			Ok(rx) => rx,
			Err(BitswapError::Overloaded) => {
				log::debug!(target: LOG_TARGET, "bitswap service overloaded, deferring fetch");
				return Ok(HashMap::new());
			},
			Err(other) => return Err(FetchError::Bitswap(other)),
		};

		let deadline = tokio::time::Instant::now() + FETCH_TIMEOUT;
		let mut acquired: HashMap<ContentHash, Vec<u8>> = HashMap::with_capacity(wants.len());
		loop {
			match tokio::time::timeout_at(deadline, rx.recv()).await {
				Ok(Some(Ok((cid, bytes)))) => {
					let hash: ContentHash = cid
						.hash()
						.digest()
						.try_into()
						.map_err(|_| BitswapError::InvalidCid { cid })?;
					log::debug!(
						target: LOG_TARGET,
						"bitswap fetched {} bytes for {hash:?}",
						bytes.len(),
					);
					// Zero-copy when this stream holds the only reference to the block.
					acquired.insert(hash, bytes.into());
				},
				Ok(Some(Err(BitswapError::ServiceClosed))) => {
					log::warn!(
						target: LOG_TARGET,
						"bitswap service closed mid-stream; returning partial result",
					);
					return Ok(acquired);
				},
				Ok(Some(Err(BitswapError::Overloaded))) => {
					log::debug!(
						target: LOG_TARGET,
						"bitswap service overloaded; returning partial result",
					);
					return Ok(acquired);
				},
				Ok(Some(Err(other))) => return Err(FetchError::Bitswap(other)),
				Ok(None) => break,
				Err(_) => {
					log::debug!(
						target: LOG_TARGET,
						"bitswap fetch timed out with {}/{} entries resolved",
						acquired.len(),
						wants.len(),
					);
					break;
				},
			}
		}

		Ok(acquired)
	}
}
