// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Provides [`ParachainBlockData`] and its historical versions.

use crate::SchedulingProof;
use alloc::vec::Vec;
use codec::{Decode, Encode};
use sp_runtime::traits::Block as BlockT;
use sp_trie::CompactProof;

/// Special prefix used by [`ParachainBlockData`] from version 1 and upwards to distinguish from the
/// unversioned legacy/v0 version.
const VERSIONED_PARACHAIN_BLOCK_DATA_PREFIX: &[u8] = b"VERSIONEDPBD";

// Struct which allows prepending bytes after reading from an input.
pub(crate) struct PrependBytesInput<'a, I> {
	prepend: &'a [u8],
	read: usize,
	inner: &'a mut I,
}

impl<'a, I: codec::Input> codec::Input for PrependBytesInput<'a, I> {
	fn remaining_len(&mut self) -> Result<Option<usize>, codec::Error> {
		let remaining_compact = self.prepend.len().saturating_sub(self.read);
		Ok(self.inner.remaining_len()?.map(|len| len.saturating_add(remaining_compact)))
	}

	fn read(&mut self, into: &mut [u8]) -> Result<(), codec::Error> {
		if into.is_empty() {
			return Ok(());
		}

		let remaining_compact = self.prepend.len().saturating_sub(self.read);
		if remaining_compact > 0 {
			let to_read = into.len().min(remaining_compact);
			into[..to_read].copy_from_slice(&self.prepend[self.read..][..to_read]);
			self.read += to_read;

			if to_read < into.len() {
				// Buffer not full, keep reading the inner.
				self.inner.read(&mut into[to_read..])
			} else {
				// Buffer was filled by the bytes.
				Ok(())
			}
		} else {
			// Prepended bytes has been read, just read from inner.
			self.inner.read(into)
		}
	}
}

/// The parachain block that is created by a collator.
///
/// This is send as PoV (proof of validity block) to the relay-chain validators. There it will be
/// passed to the parachain validation Wasm blob to be validated.
#[derive(Clone)]
pub enum ParachainBlockData<Block> {
	V0 {
		block: [Block; 1],
		proof: CompactProof,
	},
	V1 {
		blocks: Vec<Block>,
		proof: CompactProof,
	},
	/// V2 adds scheduling proof for V3 candidates.
	V2 {
		blocks: Vec<Block>,
		proof: CompactProof,
		scheduling_proof: SchedulingProof,
	},
}

impl<Block: Encode> Encode for ParachainBlockData<Block> {
	fn encode(&self) -> Vec<u8> {
		match self {
			Self::V0 { block, proof } => (&block[0], &proof).encode(),
			Self::V1 { blocks, proof } => {
				let mut res = VERSIONED_PARACHAIN_BLOCK_DATA_PREFIX.to_vec();
				1u8.encode_to(&mut res);
				blocks.encode_to(&mut res);
				proof.encode_to(&mut res);
				res
			},
			Self::V2 { blocks, proof, scheduling_proof } => {
				let mut res = VERSIONED_PARACHAIN_BLOCK_DATA_PREFIX.to_vec();
				2u8.encode_to(&mut res);
				blocks.encode_to(&mut res);
				proof.encode_to(&mut res);
				scheduling_proof.encode_to(&mut res);
				res
			},
		}
	}
}

impl<Block: Decode> Decode for ParachainBlockData<Block> {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let mut prefix = [0u8; VERSIONED_PARACHAIN_BLOCK_DATA_PREFIX.len()];
		input.read(&mut prefix)?;

		if prefix == VERSIONED_PARACHAIN_BLOCK_DATA_PREFIX {
			match input.read_byte()? {
				1 => {
					let blocks = Vec::<Block>::decode(input)?;
					let proof = CompactProof::decode(input)?;

					Ok(Self::V1 { blocks, proof })
				},
				2 => {
					let blocks = Vec::<Block>::decode(input)?;
					let proof = CompactProof::decode(input)?;
					let scheduling_proof = crate::SchedulingProof::decode(input)?;

					Ok(Self::V2 { blocks, proof, scheduling_proof })
				},
				_ => Err("Unknown `ParachainBlockData` version".into()),
			}
		} else {
			let mut input = PrependBytesInput { prepend: &prefix, read: 0, inner: input };
			let block = Block::decode(&mut input)?;
			let proof = CompactProof::decode(&mut input)?;

			Ok(Self::V0 { block: [block], proof })
		}
	}
}

impl<Block> ParachainBlockData<Block> {
	/// Creates a new instance of `Self`.
	pub fn new(
		blocks: Vec<Block>,
		proof: CompactProof,
		scheduling_proof: Option<SchedulingProof>,
	) -> Self {
		match scheduling_proof {
			Some(sp) => Self::V2 { blocks, proof, scheduling_proof: sp },
			None => Self::V1 { blocks, proof },
		}
	}

	/// Returns references to the stored blocks.
	pub fn blocks(&self) -> &[Block] {
		match self {
			Self::V0 { block, .. } => &block[..],
			Self::V1 { blocks, .. } => &blocks,
			Self::V2 { blocks, .. } => &blocks,
		}
	}

	/// Returns mutable references to the stored blocks.
	pub fn blocks_mut(&mut self) -> &mut [Block] {
		match self {
			Self::V0 { ref mut block, .. } => block,
			Self::V1 { ref mut blocks, .. } => blocks,
			Self::V2 { ref mut blocks, .. } => blocks,
		}
	}

	/// Returns the stored blocks.
	pub fn into_blocks(self) -> Vec<Block> {
		match self {
			Self::V0 { block, .. } => block.into_iter().collect(),
			Self::V1 { blocks, .. } => blocks,
			Self::V2 { blocks, .. } => blocks,
		}
	}

	/// Returns a reference to the stored proof.
	pub fn proof(&self) -> &CompactProof {
		match self {
			Self::V0 { proof, .. } => &proof,
			Self::V1 { proof, .. } => proof,
			Self::V2 { proof, .. } => proof,
		}
	}

	/// Deconstruct into the inner parts.
	pub fn into_inner(self) -> (Vec<Block>, CompactProof) {
		match self {
			Self::V0 { block, proof } => (block.into_iter().collect(), proof),
			Self::V1 { blocks, proof } => (blocks, proof),
			Self::V2 { blocks, proof, .. } => (blocks, proof),
		}
	}

	/// Returns the scheduling proof if this is a V2 POV.
	pub fn scheduling_proof(&self) -> Option<&crate::SchedulingProof> {
		match self {
			Self::V2 { scheduling_proof, .. } => Some(scheduling_proof),
			_ => None,
		}
	}
}

impl<Block: BlockT> ParachainBlockData<Block> {
	/// Log the size of the individual components (header, extrinsics, storage proof) as info.
	pub fn log_size_info(&self) {
		tracing::info!(
			target: "cumulus",
			header_kb = %self.blocks().iter().map(|b| b.header().encoded_size()).sum::<usize>() as f64 / 1024f64,
			extrinsics_kb = %self.blocks().iter().map(|b| b.extrinsics().encoded_size()).sum::<usize>() as f64 / 1024f64,
			storage_proof_kb = %self.proof().encoded_size() as f64 / 1024f64,
			"PoV size",
		);
	}

	/// Converts into [`ParachainBlockData::V0`].
	///
	/// Returns `None` if there is not exactly one block.
	pub fn as_v0(&self) -> Option<Self> {
		match self {
			Self::V0 { .. } => Some(self.clone()),
			Self::V1 { blocks, proof } => {
				if blocks.len() != 1 {
					return None;
				}

				blocks
					.first()
					.map(|block| Self::V0 { block: [block.clone()], proof: proof.clone() })
			},
			Self::V2 { blocks, proof, .. } => {
				if blocks.len() != 1 {
					return None;
				}

				blocks
					.first()
					.map(|block| Self::V0 { block: [block.clone()], proof: proof.clone() })
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_runtime::testing::*;

	#[derive(codec::Encode, codec::Decode, Clone, PartialEq, Debug)]
	struct ParachainBlockDataV0<B: BlockT> {
		/// The header of the parachain block.
		pub header: B::Header,
		/// The extrinsics of the parachain block.
		pub extrinsics: alloc::vec::Vec<B::Extrinsic>,
		/// The data that is required to emulate the storage accesses executed by all extrinsics.
		pub storage_proof: sp_trie::CompactProof,
	}

	type TestExtrinsic = TestXt<MockCallU64, ()>;
	type TestBlock = Block<TestExtrinsic>;

	#[test]
	fn decoding_encoding_v0_works() {
		let v0 = ParachainBlockDataV0::<TestBlock> {
			header: Header::new_from_number(10),
			extrinsics: vec![
				TestExtrinsic::new_bare(MockCallU64(10)),
				TestExtrinsic::new_bare(MockCallU64(100)),
			],
			storage_proof: CompactProof { encoded_nodes: vec![vec![10u8; 200], vec![20u8; 30]] },
		};

		let encoded = v0.encode();
		let decoded = ParachainBlockData::<TestBlock>::decode(&mut &encoded[..]).unwrap();

		match &decoded {
			ParachainBlockData::V0 { block, proof } => {
				assert_eq!(v0.header, block[0].header);
				assert_eq!(v0.extrinsics, block[0].extrinsics);
				assert_eq!(&v0.storage_proof, proof);
			},
			_ => panic!("Invalid decoding"),
		}

		let encoded = decoded.as_v0().unwrap().encode();

		let decoded = ParachainBlockDataV0::<TestBlock>::decode(&mut &encoded[..]).unwrap();
		assert_eq!(decoded, v0);
	}

	#[test]
	fn decoding_encoding_v1_works() {
		let v1 = ParachainBlockData::<TestBlock>::V1 {
			blocks: vec![TestBlock::new(
				Header::new_from_number(10),
				vec![
					TestExtrinsic::new_bare(MockCallU64(10)),
					TestExtrinsic::new_bare(MockCallU64(100)),
				],
			)],
			proof: CompactProof { encoded_nodes: vec![vec![10u8; 200], vec![20u8; 30]] },
		};

		let encoded = v1.encode();
		let decoded = ParachainBlockData::<TestBlock>::decode(&mut &encoded[..]).unwrap();

		assert_eq!(v1.blocks(), decoded.blocks());
		assert_eq!(v1.proof(), decoded.proof());
	}

	// Helper to create a relay chain header for tests
	fn make_relay_header(number: u32) -> polkadot_primitives::Header {
		use sp_runtime::traits::Header as _;
		polkadot_primitives::Header::new(
			number,
			polkadot_core_primitives::Hash::repeat_byte(1),
			polkadot_core_primitives::Hash::repeat_byte(2),
			polkadot_core_primitives::Hash::repeat_byte(3),
			Default::default(),
		)
	}

	#[test]
	fn decoding_encoding_v2_works() {
		let scheduling_proof = crate::SchedulingProof {
			header_chain: vec![make_relay_header(5)],
			internal_scheduling_parent_header: make_relay_header(4),
			signed_scheduling_info: None,
		};

		let v2 = ParachainBlockData::<TestBlock>::V2 {
			blocks: vec![TestBlock::new(
				Header::new_from_number(10),
				vec![
					TestExtrinsic::new_bare(MockCallU64(10)),
					TestExtrinsic::new_bare(MockCallU64(100)),
				],
			)],
			proof: CompactProof { encoded_nodes: vec![vec![10u8; 200], vec![20u8; 30]] },
			scheduling_proof: scheduling_proof.clone(),
		};

		let encoded = v2.encode();
		let decoded = ParachainBlockData::<TestBlock>::decode(&mut &encoded[..]).unwrap();

		assert_eq!(v2.blocks(), decoded.blocks());
		assert_eq!(v2.proof(), decoded.proof());
		assert_eq!(v2.scheduling_proof(), decoded.scheduling_proof());

		// Verify scheduling_proof accessor works
		assert!(decoded.scheduling_proof().is_some());
		assert_eq!(decoded.scheduling_proof().unwrap().header_chain.len(), 1);
	}

	#[test]
	fn v2_scheduling_proof_accessor_returns_none_for_v0_v1() {
		// V0 should return None for scheduling_proof
		let v0 = ParachainBlockData::<TestBlock>::V0 {
			block: [TestBlock::new(Header::new_from_number(1), vec![])],
			proof: CompactProof { encoded_nodes: vec![] },
		};
		assert!(v0.scheduling_proof().is_none());

		// V1 should return None for scheduling_proof
		let v1 = ParachainBlockData::<TestBlock>::V1 {
			blocks: vec![TestBlock::new(Header::new_from_number(1), vec![])],
			proof: CompactProof { encoded_nodes: vec![] },
		};
		assert!(v1.scheduling_proof().is_none());
	}

	#[test]
	fn v2_into_inner_drops_scheduling_proof() {
		let scheduling_proof = crate::SchedulingProof {
			header_chain: vec![make_relay_header(5)],
			internal_scheduling_parent_header: make_relay_header(4),
			signed_scheduling_info: None,
		};

		let v2 = ParachainBlockData::<TestBlock>::V2 {
			blocks: vec![TestBlock::new(Header::new_from_number(10), vec![])],
			proof: CompactProof { encoded_nodes: vec![vec![1u8; 10]] },
			scheduling_proof,
		};

		let (blocks, proof) = v2.into_inner();
		assert_eq!(blocks.len(), 1);
		assert_eq!(proof.encoded_nodes.len(), 1);
	}

	#[test]
	fn v2_as_v0_works_with_single_block() {
		let scheduling_proof = crate::SchedulingProof {
			header_chain: vec![make_relay_header(5)],
			internal_scheduling_parent_header: make_relay_header(4),
			signed_scheduling_info: None,
		};

		// V2 with single block can be converted to V0
		let v2_single = ParachainBlockData::<TestBlock>::V2 {
			blocks: vec![TestBlock::new(Header::new_from_number(10), vec![])],
			proof: CompactProof { encoded_nodes: vec![] },
			scheduling_proof: scheduling_proof.clone(),
		};
		assert!(v2_single.as_v0().is_some());

		// V2 with multiple blocks cannot be converted to V0
		let v2_multi = ParachainBlockData::<TestBlock>::V2 {
			blocks: vec![
				TestBlock::new(Header::new_from_number(10), vec![]),
				TestBlock::new(Header::new_from_number(11), vec![]),
			],
			proof: CompactProof { encoded_nodes: vec![] },
			scheduling_proof,
		};
		assert!(v2_multi.as_v0().is_none());
	}
}
