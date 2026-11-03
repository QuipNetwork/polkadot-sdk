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

//! Requests and responses as sent over the wire for the individual protocols.

use super::{IsRequest, Protocol};
use codec::{Decode, Encode};
use polkadot_node_primitives::PoV;
use polkadot_primitives::{CandidateReceiptV2 as CandidateReceipt, Hash, HeadData, Id as ParaId};

/// Request the advertised collation at that scheduling parent
#[derive(Debug, Clone, Encode, Decode)]
pub struct CollationFetchingRequest {
	/// Relay parent collation is built on top of.
	pub scheduling_parent: Hash,
	/// The `ParaId` of the collation.
	pub para_id: ParaId,
	/// Output head hash of the candidate
	pub output_head_data_hash: Hash,
}

impl IsRequest for CollationFetchingRequest {
	type Response = CollationFetchingResponse;
	const PROTOCOL: Protocol = Protocol::CollationFetchingV3;
}

/// Response as sent by collator supporting low latency
#[derive(Debug, Clone, Encode, Decode)]
pub enum CollationFetchingResponse {
	/// Deliver requested collation along with parent head data.
	#[codec(index = 0)]
	Collation {
		/// The receipt of the candidate.
		receipt: CandidateReceipt,
		/// Candidate's proof of validity
		pov: PoV,
		/// The head data of the candidate's parent.
		/// This is needed for elastic scaling to work.
		parent_head_data: HeadData,
	},
}
