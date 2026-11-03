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

pub(crate) mod imports {
	pub use cumulus_primitives_core::{ParaId, RelayProofRequest};
	pub use parachains_common_types::{AccountId, Balance, Nonce};
	pub use sp_core::{crypto::KeyTypeId, OpaqueMetadata};
	pub use sp_runtime::{
		traits::Block as BlockT,
		transaction_validity::{TransactionSource, TransactionValidity},
		ApplyExtrinsicResult,
	};
	pub use sp_weights::Weight;
}

macro_rules! impl_node_runtime_apis {
	($runtime: ty, $block: tt, $aura_id: ty) => {
		sp_api::impl_runtime_apis! {
			impl sp_api::Core<$block> for $runtime {
				fn version() -> sp_version::RuntimeVersion {
					unimplemented!()
				}

				fn execute_block(_: <$block as BlockT>::LazyBlock) {
					unimplemented!()
				}

				fn initialize_block(
					_: &<$block as BlockT>::Header
				) -> sp_runtime::ExtrinsicInclusionMode {
					unimplemented!()
				}
			}

			impl sp_api::Metadata<$block> for $runtime {
				fn metadata() -> OpaqueMetadata {
					unimplemented!()
				}

				fn metadata_at_version(_: u32) -> Option<OpaqueMetadata> {
					unimplemented!()
				}

				fn metadata_versions() -> Vec<u32> {
					unimplemented!()
				}
			}

			impl cumulus_primitives_core::RelayParentOffsetApi<$block> for $runtime {
				fn relay_parent_offset() -> u32 {
					unimplemented!()
				}

				fn max_claim_queue_offset() -> u8 {
					unimplemented!()
				}
			}

			impl cumulus_primitives_core::SchedulingV3EnabledApi<$block> for $runtime {
				fn scheduling_v3_enabled() -> bool {
					unimplemented!()
				}
			}

			impl sp_consensus_aura::AuraApi<$block, $aura_id> for $runtime {
				fn slot_duration() -> sp_consensus_aura::SlotDuration {
					unimplemented!()
				}

				fn authorities() -> Vec<$aura_id> {
					unimplemented!()
				}
			}

			impl cumulus_primitives_aura::AuraUnincludedSegmentApi<$block> for $runtime {
				fn can_build_upon(
					_: <$block as BlockT>::Hash,
					_: cumulus_primitives_aura::Slot,
				) -> bool {
					unimplemented!()
				}
			}

			impl sp_block_builder::BlockBuilder<$block> for $runtime {
				fn apply_extrinsic(_: <$block as BlockT>::Extrinsic) -> ApplyExtrinsicResult {
					unimplemented!()
				}

				fn finalize_block() -> <$block as BlockT>::Header {
					unimplemented!()
				}

				fn inherent_extrinsics(
					_: sp_inherents::InherentData
				) -> Vec<<$block as BlockT>::Extrinsic> {
					unimplemented!()
				}

				fn check_inherents(
					_: <$block as BlockT>::LazyBlock,
					_: sp_inherents::InherentData
				) -> sp_inherents::CheckInherentsResult {
					unimplemented!()
				}
			}

			impl sp_transaction_pool::runtime_api::TaggedTransactionQueue<$block> for $runtime {
				fn validate_transaction(
					_: TransactionSource,
					_: <$block as BlockT>::Extrinsic,
					_: <$block as BlockT>::Hash,
				) -> TransactionValidity {
					unimplemented!()
				}
			}

			impl sp_offchain::OffchainWorkerApi<$block> for $runtime {
				fn offchain_worker(_: &<$block as BlockT>::Header) {
					unimplemented!()
				}
			}

			impl sp_session::SessionKeys<$block> for $runtime {
				fn generate_session_keys(_owner: Vec<u8>, _seed: Option<Vec<u8>>) -> sp_session::OpaqueGeneratedSessionKeys {
					unimplemented!()
				}

				fn decode_session_keys(
					_: Vec<u8>,
				) -> Option<Vec<(Vec<u8>, KeyTypeId)>> {
					unimplemented!()
				}

			}

			impl
				pallet_transaction_payment_rpc_runtime_api::TransactionPaymentApi<
					$block,
					Balance,
				> for $runtime
			{
				fn query_info(
					_: <$block as BlockT>::Extrinsic,
					_: u32,
				) -> pallet_transaction_payment_rpc_runtime_api::RuntimeDispatchInfo<Balance> {
					unimplemented!()
				}
				fn query_fee_details(
					_: <$block as BlockT>::Extrinsic,
					_: u32,
				) -> pallet_transaction_payment::FeeDetails<Balance> {
					unimplemented!()
				}
				fn query_weight_to_fee(_: Weight) -> Balance {
					unimplemented!()
				}
				fn query_length_to_fee(_: u32) -> Balance {
					unimplemented!()
				}
			}

			impl cumulus_primitives_core::CollectCollationInfo<$block> for $runtime {
				fn collect_collation_info(
					_: &<$block as BlockT>::Header
				) -> cumulus_primitives_core::CollationInfo {
					unimplemented!()
				}
			}

			impl cumulus_primitives_core::GetParachainInfo<$block> for $runtime {
				fn parachain_id() -> ParaId {
					unimplemented!()
				}
			}

			impl cumulus_primitives_core::KeyToIncludeInRelayProof<$block> for $runtime {
				fn keys_to_prove() -> RelayProofRequest {
					unimplemented!()
				}
			}

			#[cfg(feature = "try-runtime")]
			impl frame_try_runtime::TryRuntime<$block> for $runtime {
				fn on_runtime_upgrade(
					_: frame_try_runtime::UpgradeCheckSelect
				) -> (Weight, Weight) {
					unimplemented!()
				}

				fn execute_block(
					_: <$block as BlockT>::LazyBlock,
					_: bool,
					_: bool,
					_: frame_try_runtime::TryStateSelect,
				) -> Weight {
					unimplemented!()
				}
			}

			impl frame_system_rpc_runtime_api::AccountNonceApi<
				$block,
				AccountId,
				Nonce
			> for $runtime {
				fn account_nonce(_: AccountId) -> Nonce {
					unimplemented!()
				}
			}

			#[cfg(feature = "runtime-benchmarks")]
			impl frame_benchmarking::Benchmark<$block> for $runtime {
				fn benchmark_metadata(_: bool) -> (
					Vec<frame_benchmarking::BenchmarkList>,
					Vec<frame_support::traits::StorageInfo>,
				) {
					unimplemented!()
				}

				#[allow(non_local_definitions)]
				fn dispatch_benchmark(
					_: frame_benchmarking::BenchmarkConfig
				) -> Result<Vec<frame_benchmarking::BenchmarkBatch>, String> {
					unimplemented!()
				}
			}

			impl sp_genesis_builder::GenesisBuilder<$block> for $runtime {
				fn build_state(_: Vec<u8>) -> sp_genesis_builder::Result {
					unimplemented!()
				}

				fn get_preset(_id: &Option<sp_genesis_builder::PresetId>) -> Option<Vec<u8>> {
					unimplemented!()
				}

				fn preset_names() -> Vec<sp_genesis_builder::PresetId> {
					unimplemented!()
				}
			}

			impl cumulus_primitives_core::TargetBlockRate<$block> for $runtime {
				fn target_block_rate() -> u32 {
					unimplemented!()
				}
			}

			impl sp_transaction_storage_proof::runtime_api::TransactionStorageApi<$block> for $runtime {
				fn retention_period() -> sp_runtime::traits::NumberFor<$block> {
					unimplemented!()
				}

				fn indexed_transactions(
					_block: sp_runtime::traits::NumberFor<$block>,
				) -> Vec<sp_transaction_storage_proof::IndexedTransactionInfo> {
					unimplemented!()
				}
			}

			impl sp_authority_discovery::AuthorityDiscoveryApi<$block> for $runtime {
				fn authorities() -> Vec<sp_authority_discovery::AuthorityId> {
					unimplemented!()
				}
			}
		}
	};
}

pub(crate) use impl_node_runtime_apis;
