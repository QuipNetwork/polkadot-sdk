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

//! API trait for the bitswap RPC methods.

use jsonrpsee::{core::RpcResult, proc_macros::rpc};

#[rpc(client, server)]
pub trait BitswapApi {
	/// Retrieve indexed transaction data by CID.
	///
	/// Accepts a CIDv1 (base32 multibase-encoded string), extracts the 32-byte hash
	/// digest, looks up the indexed transaction, and returns hex-encoded data.
	#[method(name = "bitswap_v1_get")]
	fn bitswap_v1_get(&self, cid: String) -> RpcResult<String>;
}
