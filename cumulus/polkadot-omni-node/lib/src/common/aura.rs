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

//! Aura-related primitives for cumulus parachain collators.

use codec::Codec;
use cumulus_primitives_aura::AuraUnincludedSegmentApi;
use cumulus_primitives_core::KeyToIncludeInRelayProof;
use sp_consensus_aura::AuraApi;
use sp_runtime::{
	app_crypto::{AppCrypto, AppPair, AppSignature, Pair},
	traits::Block as BlockT,
};

/// Convenience trait for defining the basic bounds of an `AuraId`.
pub trait AuraIdT: AppCrypto<Pair = Self::BoundedPair> + Codec + Send {
	/// Extra bounds for the `Pair`.
	type BoundedPair: AppPair + AppCrypto<Signature = Self::BoundedSignature>;

	/// Extra bounds for the `Signature`.
	type BoundedSignature: AppSignature
		+ TryFrom<Vec<u8>>
		+ std::hash::Hash
		+ sp_runtime::traits::Member
		+ Codec;
}

impl<T> AuraIdT for T
where
	T: AppCrypto + Codec + Send + Sync,
	<<T as AppCrypto>::Pair as AppCrypto>::Signature:
		TryFrom<Vec<u8>> + std::hash::Hash + sp_runtime::traits::Member + Codec,
{
	type BoundedPair = <T as AppCrypto>::Pair;
	type BoundedSignature = <<T as AppCrypto>::Pair as AppCrypto>::Signature;
}

/// Convenience trait for defining the basic bounds of a parachain runtime that supports
/// the Aura consensus.
pub trait AuraRuntimeApi<Block: BlockT, AuraId: AuraIdT>:
	sp_api::ApiExt<Block>
	+ AuraApi<Block, <AuraId::BoundedPair as Pair>::Public>
	+ AuraUnincludedSegmentApi<Block>
	+ KeyToIncludeInRelayProof<Block>
	+ Sized
where
	<AuraId::BoundedPair as Pair>::Public: std::fmt::Debug,
{
	/// Check if the runtime has the Aura API.
	fn has_aura_api(&self, at: Block::Hash) -> bool {
		self.has_api::<dyn AuraApi<Block, <AuraId::BoundedPair as Pair>::Public>>(at)
			.unwrap_or(false)
	}
}

impl<T, Block: BlockT, AuraId: AuraIdT> AuraRuntimeApi<Block, AuraId> for T
where
	T: sp_api::ApiExt<Block>
		+ AuraApi<Block, <AuraId::BoundedPair as Pair>::Public>
		+ AuraUnincludedSegmentApi<Block>
		+ KeyToIncludeInRelayProof<Block>,
	<AuraId::BoundedPair as Pair>::Public: std::fmt::Debug,
{
}
