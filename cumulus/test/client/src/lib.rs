// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! A Cumulus test client.

mod block_builder;
pub use block_builder::*;
use codec::{Decode, Encode};
use cumulus_pallet_parachain_system::block_weight::DynamicMaxBlockWeight;
pub use cumulus_test_runtime as runtime;
use cumulus_test_runtime::AuraId;
pub use polkadot_parachain_primitives::primitives::{
	BlockData, HeadData, ValidationParams, ValidationResult,
};
use runtime::{
	test_pallet, Balance, Block, BlockHashCount, Runtime, RuntimeCall, Signature, SignedPayload,
	TxExtension, UncheckedExtrinsic, VERSION,
};
use sc_consensus_aura::{
	find_pre_digest,
	standalone::{seal, slot_author},
};
pub use sc_executor::error::Result as ExecutorResult;
use sc_executor::HeapAllocStrategy;
use sc_executor_common::runtime_blob::RuntimeBlob;
use sp_api::ProvideRuntimeApi;
use sp_application_crypto::AppCrypto;
use sp_blockchain::HeaderBackend;
use sp_consensus_aura::AuraApi;
use sp_core::Pair;
use sp_io::TestExternalities;
use sp_keystore::testing::MemoryKeystore;
use sp_runtime::{generic::Era, traits::Header, BuildStorage, MultiAddress, SaturatedConversion};
use std::sync::Arc;
pub use substrate_test_client::*;

pub type ParachainBlockData = cumulus_primitives_core::ParachainBlockData<Block>;

/// Test client database backend.
pub type Backend = substrate_test_client::Backend<Block>;

/// Test client executor.
pub type Executor = client::LocalCallExecutor<
	Block,
	Backend,
	WasmExecutor<(
		sp_io::SubstrateHostFunctions,
		cumulus_primitives_proof_size_hostfunction::storage_proof_size::HostFunctions,
	)>,
>;

/// Test client builder for Cumulus
pub type TestClientBuilder =
	substrate_test_client::TestClientBuilder<Block, Executor, Backend, GenesisParameters>;

/// LongestChain type for the test runtime/client.
pub type LongestChain = sc_consensus::LongestChain<Backend, Block>;

/// Test client type with `LocalExecutor` and generic Backend.
pub type Client = client::Client<Backend, Executor, Block, runtime::RuntimeApi>;

/// Parameters of test-client builder with test-runtime.
#[derive(Default)]
pub struct GenesisParameters {
	pub endowed_accounts: Vec<cumulus_test_runtime::AccountId>,
	pub wasm: Option<Vec<u8>>,
}

impl substrate_test_client::GenesisInit for GenesisParameters {
	fn genesis_storage(&self) -> Storage {
		cumulus_test_service::chain_spec::get_chain_spec_with_extra_endowed(
			None,
			self.endowed_accounts.clone(),
			self.wasm.as_deref().unwrap_or_else(|| {
				cumulus_test_runtime::WASM_BINARY.expect("WASM binary not compiled!")
			}),
		)
		.build_storage()
		.expect("Builds test runtime genesis storage")
	}
}

/// A `test-runtime` extensions to [`TestClientBuilder`].
pub trait TestClientBuilderExt: Sized {
	/// Build the test client.
	fn build(self) -> Client {
		self.build_with_longest_chain().0
	}

	/// Build the test client and longest chain selector.
	fn build_with_longest_chain(self) -> (Client, LongestChain);
}

impl TestClientBuilderExt for TestClientBuilder {
	fn build_with_longest_chain(self) -> (Client, LongestChain) {
		self.build_with_native_executor(None)
	}
}

/// A `TestClientBuilder` with default backend and executor.
pub trait DefaultTestClientBuilderExt: Sized {
	/// Create new `TestClientBuilder`
	fn new() -> Self;
}

impl DefaultTestClientBuilderExt for TestClientBuilder {
	fn new() -> Self {
		Self::with_default_backend()
	}
}

/// Create an unsigned extrinsic from a runtime call.
pub fn generate_unsigned(function: impl Into<RuntimeCall>) -> UncheckedExtrinsic {
	UncheckedExtrinsic::new_bare(function.into())
}

/// Create a signed extrinsic from a runtime call and sign
/// with the given key pair.
pub fn generate_extrinsic_with_pair(
	client: &Client,
	origin: sp_core::sr25519::Pair,
	function: impl Into<RuntimeCall>,
	nonce: Option<u32>,
) -> UncheckedExtrinsic {
	let current_block_hash = client.info().best_hash;
	let current_block = client.info().best_number.saturated_into();
	let genesis_block = client.hash(0).unwrap().unwrap();
	let nonce = nonce.unwrap_or_default();
	let period =
		BlockHashCount::get().checked_next_power_of_two().map(|c| c / 2).unwrap_or(2) as u64;
	let tip = 0;
	let tx_ext: TxExtension = DynamicMaxBlockWeight::new(
		(
			frame_system::AuthorizeCall::<Runtime>::new(),
			frame_system::CheckNonZeroSender::<Runtime>::new(),
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckEra::<Runtime>::from(Era::mortal(period, current_block)),
			frame_system::CheckNonce::<Runtime>::from(nonce),
			frame_system::CheckWeight::<Runtime>::new(),
			pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(tip),
			test_pallet::TestTransactionExtension::<Runtime>::default(),
		)
			.into(),
	);

	let function = function.into();

	let raw_payload = SignedPayload::from_raw(
		function.clone(),
		tx_ext.clone(),
		((), (), VERSION.spec_version, genesis_block, current_block_hash, (), (), (), ()),
	);
	let signature = raw_payload.using_encoded(|e| origin.sign(e));

	UncheckedExtrinsic::new_signed(
		function,
		MultiAddress::Id(origin.public().into()),
		Signature::Sr25519(signature),
		tx_ext,
	)
}

/// Generate an extrinsic from the provided function call, origin and [`Client`].
pub fn generate_extrinsic(
	client: &Client,
	origin: sp_keyring::Sr25519Keyring,
	function: impl Into<RuntimeCall>,
) -> UncheckedExtrinsic {
	generate_extrinsic_with_pair(client, origin.into(), function, None)
}

/// Transfer some token from one account to another using a provided test [`Client`].
pub fn transfer(
	client: &Client,
	origin: sp_keyring::Sr25519Keyring,
	dest: sp_keyring::Sr25519Keyring,
	value: Balance,
) -> UncheckedExtrinsic {
	let function = RuntimeCall::Balances(pallet_balances::Call::transfer_allow_death {
		dest: MultiAddress::Id(dest.public().into()),
		value,
	});

	generate_extrinsic(client, origin, function)
}

/// Call `validate_block` in the given `wasm_blob`.
pub fn validate_block(
	validation_params: ValidationParams,
	wasm_blob: &[u8],
) -> ExecutorResult<ValidationResult> {
	let mut ext = TestExternalities::default();
	let mut ext_ext = ext.ext();

	let heap_pages = HeapAllocStrategy::Static { extra_pages: 2048 };
	let executor = WasmExecutor::<(
		sp_io::SubstrateHostFunctions,
		cumulus_primitives_proof_size_hostfunction::storage_proof_size::HostFunctions,
	)>::builder()
	.with_execution_method(WasmExecutionMethod::default())
	.with_max_runtime_instances(1)
	.with_runtime_cache_size(2)
	.with_onchain_heap_alloc_strategy(heap_pages)
	.with_offchain_heap_alloc_strategy(heap_pages)
	.build();

	executor
		.uncached_call(
			RuntimeBlob::uncompress_if_needed(wasm_blob).expect("RuntimeBlob uncompress & parse"),
			&mut ext_ext,
			false,
			"validate_block",
			&validation_params.encode(),
		)
		.map(|v| ValidationResult::decode(&mut &v[..]).expect("Decode `ValidationResult`."))
}

fn get_keystore() -> sp_keystore::KeystorePtr {
	let keystore = MemoryKeystore::new();
	sp_keyring::Sr25519Keyring::iter().for_each(|key| {
		keystore
			.sr25519_generate_new(
				sp_consensus_aura::sr25519::AuthorityPair::ID,
				Some(&key.to_seed()),
			)
			.expect("Key should be created");
	});
	Arc::new(keystore)
}

/// Seals the given block with an AURA seal.
///
/// Assumes that the authorities of the test runtime are present in the keyring.
pub fn seal_block(mut block: Block, client: &Client) -> Block {
	let parachain_slot =
		find_pre_digest::<Block, <AuraId as AppCrypto>::Signature>(&block.header).unwrap();
	let parent_hash = block.header.parent_hash;
	let authorities = client.runtime_api().authorities(parent_hash).unwrap();
	let expected_author = slot_author::<<AuraId as AppCrypto>::Pair>(parachain_slot, &authorities)
		.expect("Should be able to find author");

	let keystore = get_keystore();
	let seal_digest = seal::<_, sp_consensus_aura::sr25519::AuthorityPair>(
		&block.header.hash(),
		expected_author,
		&keystore,
	)
	.expect("Should be able to create seal");
	block.header.digest_mut().push(seal_digest);

	block
}
