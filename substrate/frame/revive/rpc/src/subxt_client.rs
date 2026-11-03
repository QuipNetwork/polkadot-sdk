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
//! The generated subxt client.
//! Generated against a substrate chain configured with [`pallet_revive`] using:
//! subxt metadata  --url ws://localhost:9944 -o rpc/revive_chain.scale
pub use subxt::config::PolkadotConfig as SrcChainConfig;

#[subxt::subxt(
	runtime_metadata_path = "$OUT_DIR/revive_chain.scale",
	// TODO remove once subxt use the same U256 type
	substitute_type(
		path = "primitive_types::U256",
		with = "::subxt::utils::Static<::sp_core::U256>"
	),

	substitute_type(
		path = "sp_runtime::generic::block::Block<A, B, C, D, E>",
		with = "::subxt::utils::Static<::sp_runtime::generic::Block<
		::sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
		::sp_runtime::OpaqueExtrinsic
		>>"
	),

	substitute_type(
		path = "pallet_revive::primitives::EthTransactError",
		with = "::subxt::utils::Static<::pallet_revive::EthTransactError>"
	),
	substitute_type(
		path = "sp_weights::weight_v2::Weight",
		with = "::subxt::utils::Static<::sp_weights::Weight>"
	),
	substitute_type(
		path = "pallet_revive::evm::api::block::Block",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockV1>"
	),
	substitute_type(
		path = "pallet_revive::evm::block_hash::ReceiptGasInfo",
		with = "::subxt::utils::Static<::pallet_revive::evm::ReceiptGasInfo>"
	),

	// Versioning replacements
	substitute_type(
		path = "pallet_revive_types::runtime_api::declaration::ReviveRuntimeApiVersionDeclarations",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ReviveRuntimeApiVersionDeclarations>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::tracer::TracerTypeV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TracerTypeV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::receipt::ReceiptGasInfoV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ReceiptGasInfoV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::tracer::CallTracerConfigV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallTracerConfigV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::tracer::PrestateTracerConfigV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PrestateTracerConfigV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::tracer::ExecutionTracerConfigV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ExecutionTracerConfigV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::TraceV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::TraceV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::TraceEntryV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceEntryV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::CallTraceV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallTraceV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::CallTraceV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallTraceV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::PrestateTraceV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PrestateTraceV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::ExecutionTraceV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ExecutionTraceV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::CallLogV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallLogV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::CallLogV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallLogV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::CallTypeV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallTypeV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::PrestateTraceInfoV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PrestateTraceInfoV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::ExecutionStepV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ExecutionStepV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::traces::ExecutionStepKindV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ExecutionStepKindV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::GenericTransactionV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GenericTransactionV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::InputOrDataV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::InputOrDataV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::AccessListEntryV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AccessListEntryV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::AuthorizationListEntryV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AuthorizationListEntryV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::state_overrides::StateOverrideSetV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::StateOverrideSetV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::state_overrides::StateOverrideV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::StateOverrideV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::state_overrides::StorageOverrideV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::StorageOverrideV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::state_overrides::TracingConfigV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TracingConfigV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::dry_run::EthTransactInfoV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::EthTransactInfoV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::dry_run::DryRunConfigV1<Moment>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::DryRunConfigV1<Moment>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::block::BlockV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::block::WithdrawalV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::WithdrawalV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::HashesOrTransactionInfosV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::HashesOrTransactionInfosV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::TransactionInfoV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactionInfoV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::TransactionSignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactionSignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction1559UnsignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction1559UnsignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction2930UnsignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction2930UnsignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction4844UnsignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction4844UnsignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::TransactionLegacyUnsignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactionLegacyUnsignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction7702UnsignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction7702UnsignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction7702SignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction7702SignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction1559SignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction1559SignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction2930SignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction2930SignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::Transaction4844SignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::Transaction4844SignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::transaction::TransactionLegacySignedV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactionLegacySignedV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::contract::ContractResultV1<R, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ContractResultV1<R, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::contract::StorageDepositV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::StorageDepositV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::contract::ExecReturnValueV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ExecReturnValueV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::contract::InstantiateReturnValueV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::InstantiateReturnValueV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::contract::CodeV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CodeV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_block::BlockInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_block::BlockVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_block::BlockOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_block::BlockVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_hash::BlockHashInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockHashInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_hash::BlockHashVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockHashVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_hash::BlockHashOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockHashOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_hash::BlockHashVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockHashVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::receipt_data::ReceiptDataInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ReceiptDataInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::receipt_data::ReceiptDataVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ReceiptDataVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::receipt_data::ReceiptDataOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ReceiptDataOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::receipt_data::ReceiptDataVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::ReceiptDataVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::balance::BalanceInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BalanceInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::balance::BalanceVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BalanceVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::balance::BalanceOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BalanceOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::balance::BalanceVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BalanceVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::gas_price::GasPriceInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GasPriceInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::gas_price::GasPriceVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GasPriceVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::gas_price::GasPriceOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GasPriceOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::gas_price::GasPriceVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GasPriceVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::nonce::NonceInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NonceInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::nonce::NonceVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NonceVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::nonce::NonceOutputPayloadV1<Nonce>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NonceOutputPayloadV1<Nonce>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::nonce::NonceVersionedOutputPayload<Nonce>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NonceVersionedOutputPayload<Nonce>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::call::CallInputPayloadV1<AccountId, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallInputPayloadV1<AccountId, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::call::CallVersionedInputPayload<AccountId, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallVersionedInputPayload<AccountId, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::call::CallOutputPayloadV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallOutputPayloadV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::call::CallVersionedOutputPayload<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CallVersionedOutputPayload<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::instantiate::InstantiateInputPayloadV1<AccountId, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::InstantiateInputPayloadV1<AccountId, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::instantiate::InstantiateVersionedInputPayload<AccountId, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::InstantiateVersionedInputPayload<AccountId, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::instantiate::InstantiateOutputPayloadV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::InstantiateOutputPayloadV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::instantiate::InstantiateVersionedOutputPayload<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::InstantiateVersionedOutputPayload<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_transact::TransactInputPayloadV1<Moment>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactInputPayloadV1<Moment>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_transact::TransactVersionedInputPayload<Moment>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactVersionedInputPayload<Moment>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_transact::TransactOutputPayloadV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactOutputPayloadV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_transact::TransactVersionedOutputPayload<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TransactVersionedOutputPayload<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_estimate_gas::EstimateGasInputPayloadV1<Moment>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::EstimateGasInputPayloadV1<Moment>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_estimate_gas::EstimateGasVersionedInputPayload<Moment>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::EstimateGasVersionedInputPayload<Moment>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_estimate_gas::EstimateGasOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::EstimateGasOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_estimate_gas::EstimateGasVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::EstimateGasVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_pre_dispatch_weight::PreDispatchWeightInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PreDispatchWeightInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_pre_dispatch_weight::PreDispatchWeightVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PreDispatchWeightVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_pre_dispatch_weight::PreDispatchWeightOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PreDispatchWeightOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::eth_pre_dispatch_weight::PreDispatchWeightVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::PreDispatchWeightVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::upload::CodeUploadReturnValueV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CodeUploadReturnValueV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::upload_code::UploadCodeInputPayloadV1<AccountId, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::UploadCodeInputPayloadV1<AccountId, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::upload_code::UploadCodeVersionedInputPayload<AccountId, Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::UploadCodeVersionedInputPayload<AccountId, Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::upload_code::UploadCodeOutputPayloadV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::UploadCodeOutputPayloadV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::upload_code::UploadCodeVersionedOutputPayload<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::UploadCodeVersionedOutputPayload<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::types::storage::StorageKeyV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::StorageKeyV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::get_storage::GetStorageInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GetStorageInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::get_storage::GetStorageVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GetStorageVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::get_storage::GetStorageOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GetStorageOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::get_storage::GetStorageVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::GetStorageVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::runtime_pallets_address::RuntimePalletsAddressInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::RuntimePalletsAddressInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::runtime_pallets_address::RuntimePalletsAddressVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::RuntimePalletsAddressVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::runtime_pallets_address::RuntimePalletsAddressOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::RuntimePalletsAddressOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::runtime_pallets_address::RuntimePalletsAddressVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::RuntimePalletsAddressVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::code::CodeInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CodeInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::code::CodeVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CodeVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::code::CodeOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CodeOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::code::CodeVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::CodeVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::account_id::AccountIdInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AccountIdInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::account_id::AccountIdVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AccountIdVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::account_id::AccountIdOutputPayloadV1<AccountId>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AccountIdOutputPayloadV1<AccountId>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::account_id::AccountIdVersionedOutputPayload<AccountId>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AccountIdVersionedOutputPayload<AccountId>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::new_balance_with_dust::NewBalanceWithDustInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NewBalanceWithDustInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::new_balance_with_dust::NewBalanceWithDustVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NewBalanceWithDustVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::new_balance_with_dust::NewBalanceWithDustOutputPayloadV1<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NewBalanceWithDustOutputPayloadV1<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::new_balance_with_dust::NewBalanceWithDustVersionedOutputPayload<Balance>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::NewBalanceWithDustVersionedOutputPayload<Balance>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_author::BlockAuthorInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockAuthorInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_author::BlockAuthorVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockAuthorVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_author::BlockAuthorOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockAuthorOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_author::BlockAuthorVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockAuthorVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::address::AddressInputPayloadV1<AccountId>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AddressInputPayloadV1<AccountId>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::address::AddressVersionedInputPayload<AccountId>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AddressVersionedInputPayload<AccountId>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::address::AddressOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AddressOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::address::AddressVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::AddressVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_gas_limit::BlockGasLimitInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockGasLimitInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_gas_limit::BlockGasLimitVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockGasLimitVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_gas_limit::BlockGasLimitOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockGasLimitOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::block_gas_limit::BlockGasLimitVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::BlockGasLimitVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::max_extrinsic_weight_in_gas::MaxExtrinsicWeightInGasInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::MaxExtrinsicWeightInGasInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::max_extrinsic_weight_in_gas::MaxExtrinsicWeightInGasVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::MaxExtrinsicWeightInGasVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::max_extrinsic_weight_in_gas::MaxExtrinsicWeightInGasOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::MaxExtrinsicWeightInGasOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::max_extrinsic_weight_in_gas::MaxExtrinsicWeightInGasVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::MaxExtrinsicWeightInGasVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_block::TraceBlockInputPayloadV1<Block>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceBlockInputPayloadV1<Block>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_block::TraceBlockInputPayloadV2<Block>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceBlockInputPayloadV2<Block>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_block::TraceBlockVersionedInputPayload<Block>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceBlockVersionedInputPayload<Block>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_block::TraceBlockOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceBlockOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_block::TraceBlockOutputPayloadV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceBlockOutputPayloadV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_block::TraceBlockVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceBlockVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_tx::TraceTxInputPayloadV1<Block>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceTxInputPayloadV1<Block>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_tx::TraceTxInputPayloadV2<Block>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceTxInputPayloadV2<Block>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_tx::TraceTxVersionedInputPayload<Block>",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceTxVersionedInputPayload<Block>>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_tx::TraceTxOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceTxOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_tx::TraceTxOutputPayloadV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceTxOutputPayloadV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_tx::TraceTxVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceTxVersionedOutputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_call::TraceCallInputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceCallInputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_call::TraceCallInputPayloadV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceCallInputPayloadV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_call::TraceCallVersionedInputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceCallVersionedInputPayload>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_call::TraceCallOutputPayloadV1",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceCallOutputPayloadV1>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_call::TraceCallOutputPayloadV2",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceCallOutputPayloadV2>"
	),
	substitute_type(
		path = "pallet_revive_types::runtime_api::payloads::trace_call::TraceCallVersionedOutputPayload",
		with = "::subxt::utils::Static<::pallet_revive_types::runtime_api::TraceCallVersionedOutputPayload>"
	),

	derive_for_all_types = "codec::Encode, codec::Decode"
)]
mod src_chain {}
pub use src_chain::*;
