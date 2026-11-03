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

//! Implementation of the `bitswap_v1_get` RPC method.
//!
//! See <https://github.com/paritytech/json-rpc-interface-spec/blob/main/src/api/bitswap_unstable_get.md>

use crate::bitswap::{api::BitswapApiServer, error::Error};
use cid::Cid;
use jsonrpsee::core::RpcResult;
use multihash_codetable::Code;
use sc_client_api::BlockBackend;
use sp_core::H256;
use sp_runtime::traits::Block as BlockT;
use std::sync::Arc;

/// Log target for this file.
const LOG_TARGET: &str = "rpc-spec-v2";

/// Bitswap RPC implementation.
pub struct Bitswap<Block, Client> {
	client: Arc<Client>,
	sync_oracle: Arc<dyn sp_consensus::SyncOracle + Send + Sync>,
	_phantom: std::marker::PhantomData<Block>,
}

impl<Block, Client> Bitswap<Block, Client> {
	/// Creates a new [`Bitswap`] instance.
	pub fn new(
		client: Arc<Client>,
		sync_oracle: Arc<dyn sp_consensus::SyncOracle + Send + Sync>,
	) -> Self {
		Self { client, sync_oracle, _phantom: std::marker::PhantomData }
	}
}

impl<Block, Client> BitswapApiServer for Bitswap<Block, Client>
where
	Block: BlockT,
	Client: BlockBackend<Block> + Send + Sync + 'static,
{
	fn bitswap_v1_get(&self, cid_str: String) -> RpcResult<String> {
		let cid = Cid::try_from(cid_str.as_str()).map_err(|e| Error::InvalidCid(format!("{e}")))?;

		// Only CIDv1 version is supported according to the spec.
		if cid.version() != cid::Version::V1 {
			return Err(Error::InvalidCid("Only CIDv1 is supported".into()).into());
		}

		let hash = cid.hash();

		// Only sha2-256, blake2b-256 & keccak-256 hash functions are supported according to the
		// spec.
		if hash.code() != u64::from(Code::Sha2_256) &&
			hash.code() != u64::from(Code::Blake2b256) &&
			hash.code() != u64::from(Code::Keccak256)
		{
			return Err(Error::InvalidCid(
				"Only sha2-256, blake2b-256 & keccak-256 hash functions are supported".into(),
			)
			.into());
		}

		// `H256::from_slice` panics below if the size is incorrect, so double-check the size is
		// correct, even though we checked the hash function type above.
		if hash.size() != 32 {
			return Err(Error::InvalidCid("Only 256-bit hash digests are supported".into()).into());
		}

		let digest = H256::from_slice(hash.digest());

		match self.client.indexed_transaction(digest) {
			Ok(Some(data)) => Ok(crate::hex_string(&data)),
			Ok(None) => {
				if self.sync_oracle.is_major_syncing() {
					Err(Error::MajorSyncing.into())
				} else {
					Err(Error::NotFound.into())
				}
			},
			Err(err) => {
				// Note: this never happens in practice, because `indexed_transaction`
				// implementation in `substrate/client/db` always returns Ok(_), and is only
				// needed to handle possible future API changes.
				log::warn!(target: LOG_TARGET, "Indexed transaction fetch failed: {err:?}");

				Err(Error::Internal(err).into())
			},
		}
	}
}
