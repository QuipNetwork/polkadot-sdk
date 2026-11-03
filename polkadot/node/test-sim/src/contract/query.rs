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

//! `Query` enum: information-gathering queries the subsystem makes to other subsystems.
//!
//! The harness's responder answers these. Tests **never** assert on `Query` values — they
//! represent implementation detail (how the subsystem gathers the information it needs).

use polkadot_node_subsystem::messages::{
	CandidateBackingMessage, ChainApiMessage, ProspectiveParachainsMessage, RuntimeApiMessage,
};

/// An information-gathering query the subsystem made. Variants wrap the original message
/// (including its `oneshot` reply channel) so the responder can answer authoritatively.
#[derive(Debug)]
pub enum Query {
	/// `RuntimeApi` query (validator groups, claim queue, async backing params, ...).
	Runtime(RuntimeApiMessage),
	/// `ChainApi` query (block headers, ancestry, finalized hashes/numbers).
	ChainApi(ChainApiMessage),
	/// `ProspectiveParachains` query (validation data, fragment chain membership).
	Prospective(ProspectiveParachainsMessage),
	/// `CandidateBacking::CanSecond(...)` request — strictly a query (the subsystem only
	/// proceeds with the fetch if backing OKs it).
	CanSecond(CandidateBackingMessage),
}
