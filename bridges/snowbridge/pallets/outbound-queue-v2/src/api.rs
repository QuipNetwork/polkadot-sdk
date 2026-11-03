// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Helpers for implementing runtime api

use crate::{Config, Messages, Pallet};
use snowbridge_merkle_tree::{merkle_proof, MerkleProof};

pub fn prove_message<T>(leaf_index: u64) -> Option<MerkleProof>
where
	T: Config,
{
	let messages = Messages::<T>::get();
	// `merkle_proof` panics if `leaf_index` is out of range, so guard against it here and return
	// `None` instead. This also covers the empty-`Messages` case.
	if leaf_index >= messages.len() as u64 {
		return None;
	}
	// `MessageLeaves` is not persisted to state, so the committed leaves are recomputed from the
	// `Messages` stored for the current block. They are produced in the same order in which they
	// were appended during block execution, so `leaf_index` stays valid. This runs off-chain only,
	// so reading `Messages` here does not enter any block's PoV.
	let leaves = messages.iter().map(Pallet::<T>::message_leaf);
	let proof = merkle_proof::<<T as Config>::Hashing, _>(leaves, leaf_index);
	Some(proof)
}
