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

//! Dynamic-dispatch wrappers for the HOP runtime API.
//!
//! Calling the API by SCALE-encoded bytes lets the node interact with any
//! runtime that exposes the named methods, without imposing a static
//! `HopRuntimeApi` bound on the client's `RuntimeApi` type. Detection happens
//! via `ApiExt::has_api_with` at startup; from there the node either carries
//! a promoter (which uses these wrappers) or runs cleanup-only.

use codec::{Decode, Encode};
use sp_api::{ApiError, CallApiAt, CallApiAtParams, CallContext};
use sp_runtime::{traits::Block as BlockT, AccountId32, MultiSignature, MultiSigner};

fn call<Block, C, Args, R>(
	client: &C,
	at: Block::Hash,
	method: &'static str,
	args: Args,
) -> Result<R, ApiError>
where
	Block: BlockT,
	C: CallApiAt<Block>,
	Args: Encode,
	R: Decode,
{
	let raw = client.call_api_at(CallApiAtParams {
		at,
		function: method,
		arguments: args.encode(),
		overlayed_changes: &Default::default(),
		call_context: CallContext::Offchain,
		recorder: &None,
		extensions: &Default::default(),
	})?;
	R::decode(&mut &*raw).map_err(|error| ApiError::FailedToDecodeReturnValue {
		function: method,
		error,
		raw,
	})
}

/// `HopRuntimeApi::max_promotion_size`.
pub fn max_promotion_size<Block, C>(client: &C, at: Block::Hash) -> Result<u32, ApiError>
where
	Block: BlockT,
	C: CallApiAt<Block>,
{
	call::<Block, _, _, _>(client, at, "HopRuntimeApi_max_promotion_size", ())
}

/// `HopRuntimeApi::can_account_promote`.
pub fn can_account_promote<Block, C>(
	client: &C,
	at: Block::Hash,
	who: AccountId32,
	data_len: u32,
) -> Result<bool, ApiError>
where
	Block: BlockT,
	C: CallApiAt<Block>,
{
	call::<Block, _, _, _>(client, at, "HopRuntimeApi_can_account_promote", (who, data_len))
}

/// `HopRuntimeApi::create_promotion_extrinsic`.
pub fn create_promotion_extrinsic<Block, C>(
	client: &C,
	at: Block::Hash,
	data: Vec<u8>,
	signer: MultiSigner,
	signature: MultiSignature,
	submit_timestamp: u64,
) -> Result<<Block as BlockT>::Extrinsic, ApiError>
where
	Block: BlockT,
	C: CallApiAt<Block>,
{
	call::<Block, _, _, _>(
		client,
		at,
		"HopRuntimeApi_create_promotion_extrinsic",
		(data, signer, signature, submit_timestamp),
	)
}

/// `HopRuntimeApi::is_promoted_on_chain`.
pub fn is_promoted_on_chain<Block, C>(
	client: &C,
	at: Block::Hash,
	hash: [u8; 32],
) -> Result<bool, ApiError>
where
	Block: BlockT,
	C: CallApiAt<Block>,
{
	call::<Block, _, _, _>(client, at, "HopRuntimeApi_is_promoted_on_chain", hash)
}
