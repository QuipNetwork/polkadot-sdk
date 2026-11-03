// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

//! HOP (Hand-Off Protocol) primitives.
//!
//! Contains the runtime API trait for HOP — authorization checks and promotion
//! of ephemeral pool data to on-chain storage.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

sp_api::decl_runtime_apis! {
	/// Runtime API for HOP.
	///
	/// Runtimes that support HOP implement this API so the node can check
	/// authorization and promote near-expiry pool entries to on-chain storage.
	#[api_version(1)]
	pub trait HopRuntimeApi<AccountId> where AccountId: codec::Codec {
		/// Maximum blob size (in bytes) the runtime will accept for promotion.
		///
		/// Authoritative — the node rejects oversized submissions at the RPC
		/// boundary using this value, before any per-account authorization lookup
		/// or signature verification.
		fn max_promotion_size() -> u32;
		/// Whether `who` may submit a HOP blob of `data_len` bytes for promotion.
		///
		/// Returns `false` for any per-account "not allowed" reason — unknown
		/// account, exhausted quota, size outside a per-account tier, etc. The
		/// absolute per-submission size cap is the responsibility of
		/// [`Self::max_promotion_size`]; this hook is for per-account policy.
		fn can_account_promote(who: AccountId, data_len: u32) -> bool;
		/// Construct an unsigned promotion extrinsic carrying the user's submit-time
		/// (in milliseconds from the Unix epoch), signer, signature, and timestamp
		/// so the runtime pallet can verify consent on-chain.
		///
		/// `submit_timestamp` is bound into the signed payload. Implementing
		/// runtimes **must** reject promotions whose timestamp is outside a
		/// tolerance window around the current on-chain clock — otherwise the
		/// same `(data, signer, signature)` tuple can be replayed indefinitely
		/// from the collator's persisted metadata. The width of the window is a
		/// runtime policy decision (clock skew + max acceptable promotion
		/// latency); a few hours is a reasonable upper bound.
		fn create_promotion_extrinsic(
			data: alloc::vec::Vec<u8>,
			signer: sp_runtime::MultiSigner,
			signature: sp_runtime::MultiSignature,
			submit_timestamp: u64,
		) -> Block::Extrinsic;
		/// Whether the content with `hash` is already stored on-chain.
		///
		/// Used by HOP's maintenance task to confirm that a previously submitted
		/// promotion extrinsic actually made it into a block.
		fn is_promoted_on_chain(hash: [u8; 32]) -> bool;
	}
}
