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

//! Parachain specific networking

use sp_consensus::block_validation::{
	BlockAnnounceValidator as BlockAnnounceValidatorT, Validation,
};
use sp_runtime::traits::Block as BlockT;

use polkadot_primitives::{
	CandidateReceiptV2 as CandidateReceipt, CompactStatement, Hash as PHash, UncheckedSigned,
};

use codec::{Decode, DecodeAll, Encode};
use futures::{future::FutureExt, Future};
use std::pin::Pin;

type BoxedError = Box<dyn std::error::Error + Send>;

/// The data that we attach to a block announcement.
///
/// This will be used to prove that a header belongs to a block that is probably being backed by
/// the relay chain.
#[derive(Encode, Debug)]
pub struct BlockAnnounceData {
	/// The receipt identifying the candidate.
	receipt: CandidateReceipt,
	/// The seconded statement issued by a relay chain validator that approves the candidate.
	statement: UncheckedSigned<CompactStatement>,
	/// The relay parent that was used as context to sign the [`Self::statement`].
	relay_parent: PHash,
}

impl Decode for BlockAnnounceData {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let receipt = CandidateReceipt::decode(input)?;
		let statement = UncheckedSigned::<CompactStatement>::decode(input)?;

		let relay_parent = match PHash::decode(input) {
			Ok(p) => p,
			// For being backwards compatible, we support missing relay-chain parent.
			Err(_) => receipt.descriptor.relay_parent(),
		};

		Ok(Self { receipt, statement, relay_parent })
	}
}

/// A [`BlockAnnounceValidatorT`] which accepts all block announcements, as it assumes
/// sybil resistance is handled elsewhere.
#[derive(Debug, Clone)]
pub struct AssumeSybilResistance(bool);

impl AssumeSybilResistance {
	/// Instantiate this block announcement validator while permissively allowing (but ignoring)
	/// announcements which come tagged with seconded messages.
	///
	/// This is useful for backwards compatibility when upgrading nodes: old nodes will continue
	/// to broadcast announcements with seconded messages, so these announcements shouldn't be
	/// rejected and the peers not punished.
	pub fn allow_seconded_messages() -> Self {
		AssumeSybilResistance(true)
	}

	/// Instantiate this block announcement validator while rejecting announcements that come with
	/// data.
	pub fn reject_seconded_messages() -> Self {
		AssumeSybilResistance(false)
	}
}

impl<Block: BlockT> BlockAnnounceValidatorT<Block> for AssumeSybilResistance {
	fn validate(
		&mut self,
		_header: &Block::Header,
		data: &[u8],
	) -> Pin<Box<dyn Future<Output = Result<Validation, BoxedError>> + Send>> {
		let allow_seconded_messages = self.0;
		let data = data.to_vec();

		async move {
			Ok(if data.is_empty() {
				Validation::Success { is_new_best: false }
			} else if !allow_seconded_messages {
				Validation::Failure { disconnect: false }
			} else {
				match BlockAnnounceData::decode_all(&mut data.as_slice()) {
					Ok(_) => Validation::Success { is_new_best: false },
					Err(_) => Validation::Failure { disconnect: true },
				}
			})
		}
		.boxed()
	}
}
