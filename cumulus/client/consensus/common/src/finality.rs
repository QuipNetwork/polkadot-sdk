// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! Shared helpers for finality-driven aux-storage cleanup.

use sc_client_api::HeaderBackend;
use sp_runtime::traits::{Block as BlockT, Header as _};

const LOG_TARGET: &str = "consensus::common::finality";

/// Resolve the previously-finalized block hash: the parent of the first block in `tree_route`,
/// or `fallback_parent` (the just-finalized header's parent) when the route is empty or that
/// header can't be loaded.
pub fn old_finalized_hash<C, Block>(
	client: &C,
	tree_route: &[Block::Hash],
	fallback_parent: Block::Hash,
) -> Block::Hash
where
	C: HeaderBackend<Block>,
	Block: BlockT,
{
	let Some(first) = tree_route.first() else {
		return fallback_parent;
	};

	match client.header(*first) {
		Ok(Some(header)) => *header.parent_hash(),
		Ok(None) => {
			tracing::warn!(
				target: LOG_TARGET,
				?first,
				"tree_route head header missing; falling back to notification parent hash",
			);
			fallback_parent
		},
		Err(error) => {
			tracing::warn!(
				target: LOG_TARGET,
				?first,
				?error,
				"tree_route head header lookup failed; falling back to notification parent hash",
			);
			fallback_parent
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_blockchain::Result as ClientResult;

	type Block = substrate_test_runtime::Block;
	type Hash = <Block as BlockT>::Hash;

	// Minimal `HeaderBackend` mock for the `old_finalized_hash_*` tests; only `header()` is used.
	struct MockHeaderBackend {
		lookup: Option<(Hash, substrate_test_runtime::Header)>,
	}

	impl HeaderBackend<Block> for MockHeaderBackend {
		fn header(
			&self,
			hash: <Block as BlockT>::Hash,
		) -> ClientResult<Option<<Block as BlockT>::Header>> {
			Ok(self.lookup.as_ref().and_then(|(h, hdr)| (*h == hash).then(|| hdr.clone())))
		}

		fn info(&self) -> sc_client_api::blockchain::Info<Block> {
			unimplemented!()
		}

		fn status(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> ClientResult<sc_client_api::blockchain::BlockStatus> {
			unimplemented!()
		}

		fn number(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> ClientResult<Option<sp_runtime::traits::NumberFor<Block>>> {
			unimplemented!()
		}

		fn hash(
			&self,
			_number: sp_runtime::traits::NumberFor<Block>,
		) -> ClientResult<Option<<Block as BlockT>::Hash>> {
			unimplemented!()
		}
	}

	#[test]
	fn old_finalized_hash_with_empty_tree_route() {
		let client = MockHeaderBackend { lookup: None };
		let fallback = Hash::repeat_byte(0x01);

		let old_hash = old_finalized_hash::<_, Block>(&client, &[], fallback);
		assert_eq!(
			old_hash, fallback,
			"empty tree_route should fall through to the supplied parent"
		);
	}

	#[test]
	fn old_finalized_hash_with_tree_route() {
		use substrate_test_runtime::Header as TestHeader;

		let expected_old = Hash::repeat_byte(0x01);
		let tree_block = Hash::repeat_byte(0x02);

		let header = TestHeader {
			parent_hash: expected_old,
			number: 2,
			state_root: Default::default(),
			extrinsics_root: Default::default(),
			digest: Default::default(),
		};

		let client = MockHeaderBackend { lookup: Some((tree_block, header)) };
		let fallback = Hash::repeat_byte(0xFF);

		let old_hash = old_finalized_hash::<_, Block>(&client, &[tree_block], fallback);
		assert_eq!(old_hash, expected_old, "should resolve to parent of first tree_route block");
	}

	#[test]
	fn old_finalized_hash_falls_back_when_header_missing() {
		// Non-empty tree_route, but the header lookup returns None — the function should fall back
		// to `fallback_parent` rather than panicking or returning a wrong hash.
		let client = MockHeaderBackend { lookup: None };
		let tree_block = Hash::repeat_byte(0x02);
		let fallback = Hash::repeat_byte(0xAA);

		let old_hash = old_finalized_hash::<_, Block>(&client, &[tree_block], fallback);
		assert_eq!(old_hash, fallback, "missing header should fall back to the supplied parent");
	}
}
