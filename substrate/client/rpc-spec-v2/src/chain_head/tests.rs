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

use super::*;
use crate::{
	chain_head::{api::ChainHeadApiClient, event::MethodResponse, test_utils::ChainHeadMockClient},
	common::events::{StorageQuery, StorageQueryType, StorageResultType},
	hex_string,
};
use assert_matches::assert_matches;
use codec::{Decode, Encode};
use futures::Future;
use jsonrpsee::{
	core::{
		client::Subscription as RpcClientSubscription, server::Subscription as RpcSubscription,
	},
	rpc_params, MethodsError as Error, RpcModule,
};
use sc_block_builder::BlockBuilderBuilder;
use sc_client_api::ChildInfo;
use sc_rpc::testing::TokioTestExecutor;
use sc_service::client::new_with_backend;
use sp_blockchain::HeaderBackend;
use sp_consensus::BlockOrigin;
use sp_core::{
	storage::well_known_keys::{self, CODE},
	Blake2Hasher, Hasher,
};
use sp_runtime::traits::Block as BlockT;
use sp_version::RuntimeVersion;
use std::{
	collections::{HashMap, HashSet, VecDeque},
	fmt::Debug,
	sync::Arc,
	time::Duration,
};
use substrate_test_runtime::Transfer;
use substrate_test_runtime_client::{
	prelude::*, runtime, runtime::RuntimeApi, Backend, BlockBuilderExt, Client,
	ClientBlockImportExt, GenesisInit,
};

type Header = substrate_test_runtime_client::runtime::Header;
type Block = substrate_test_runtime_client::runtime::Block;
const MAX_PINNED_BLOCKS: usize = 32;
const MAX_PINNED_SECS: u64 = 60;
const MAX_OPERATIONS: usize = 16;
const MAX_LAGGING_DISTANCE: usize = 128;
const MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION: usize = 4;

const INVALID_HASH: [u8; 32] = [1; 32];
const KEY: &[u8] = b":mock";
const VALUE: &[u8] = b"hello world";
const CHILD_STORAGE_KEY: &[u8] = b"child";
const CHILD_VALUE: &[u8] = b"child value";
const DOES_NOT_PRODUCE_EVENTS_SECONDS: u64 = 10;

/// Start an RPC server with the chainHead module.
pub async fn run_server() -> std::net::SocketAddr {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client,
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_follow_subscriptions_per_connection: 1,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let server = jsonrpsee::server::ServerBuilder::default().build("127.0.0.1:0").await.unwrap();

	let addr = server.local_addr().unwrap();
	let handle = server.start(api);

	tokio::spawn(handle.stopped());
	addr
}

async fn get_next_event<T: serde::de::DeserializeOwned>(sub: &mut RpcSubscription) -> T {
	let (event, _sub_id) = tokio::time::timeout(std::time::Duration::from_secs(60), sub.next())
		.await
		.unwrap()
		.unwrap()
		.unwrap();
	event
}

async fn does_not_produce_event<T: serde::de::DeserializeOwned + Debug>(
	sub: &mut RpcSubscription,
	duration: std::time::Duration,
) {
	tokio::time::timeout(duration, sub.next::<T>()).await.unwrap_err();
}

async fn run_with_timeout<F: Future>(future: F) -> <F as Future>::Output {
	tokio::time::timeout(std::time::Duration::from_secs(60 * 10), future)
		.await
		.unwrap()
}

async fn setup_api() -> (
	Arc<Client<Backend>>,
	RpcModule<ChainHead<Backend, Block, Client<Backend>>>,
	RpcSubscription,
	String,
	Block,
) {
	let child_info = ChildInfo::new_default(CHILD_STORAGE_KEY);
	let builder = TestClientBuilder::new().add_extra_child_storage(
		&child_info,
		KEY.to_vec(),
		CHILD_VALUE.to_vec(),
	);
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	(client, api, sub, sub_id, block)
}

async fn import_block(
	client: Arc<Client<Backend>>,
	parent_hash: <Block as BlockT>::Hash,
	parent_number: u64,
) -> Block {
	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(parent_hash)
		.with_parent_block_number(parent_number)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();
	block
}

async fn import_best_block_with_tx(
	client: Arc<Client<Backend>>,
	parent_hash: <Block as BlockT>::Hash,
	parent_number: u64,
	tx: Transfer,
) -> Block {
	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(parent_hash)
		.with_parent_block_number(parent_number)
		.build()
		.unwrap();
	block_builder.push_transfer(tx).unwrap();
	let block = block_builder.build().unwrap().block;
	client.import_as_best(BlockOrigin::Own, block.clone()).await.unwrap();
	block
}

/// Check the subscription produces a new block and a best block event.
///
/// The macro is used instead of a fn to preserve the lines of code in case of panics.
macro_rules! check_new_and_best_block_events {
	($sub:expr, $block_hash:expr, $parent_hash:expr) => {
		let event: FollowEvent<String> = get_next_event($sub).await;
		let expected = FollowEvent::NewBlock(NewBlock {
			block_hash: format!("{:?}", $block_hash),
			parent_block_hash: format!("{:?}", $parent_hash),
			new_runtime: None,
			with_runtime: false,
		});
		assert_eq!(event, expected);

		let event: FollowEvent<String> = get_next_event($sub).await;
		let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
			best_block_hash: format!("{:?}", $block_hash),
		});
		assert_eq!(event, expected);
	};
}

#[tokio::test]
async fn follow_subscription_produces_blocks() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Then the best block
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", finalized_hash),
	});
	assert_eq!(event, expected);

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;

	let best_hash = block.header.hash();
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", best_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", best_hash),
	});
	assert_eq!(event, expected);

	client.finalize_block(best_hash, None).unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", best_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn follow_with_runtime() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;

	// it is basically json-encoded substrate_test_runtime_client::runtime::VERSION
	let runtime_str = "{\"specName\":\"test\",\"implName\":\"parity-test\",\"authoringVersion\":1,\
		\"specVersion\":2,\"implVersion\":2,\"apis\":[[\"0xdf6acb689907609b\",5],\
		[\"0x37e397fc7c91f5e4\",2],[\"0xd2bc9897eed08f15\",3],[\"0x40fe3ad401f8959a\",6],\
		[\"0xbc9d89904f5b923f\",1],[\"0xc6e9a76309f39b09\",2],[\"0xdd718d5cc53262d4\",1],\
		[\"0xcbca25e39f142387\",2],[\"0xf78b278be53f454c\",2],[\"0xab3c0572291feb8b\",2],\
		[\"0xed99c5acb25eedf5\",3],[\"0xfbc577b9d747efd6\",1]],\"transactionVersion\":1,\"systemVersion\":1}";

	let runtime: RuntimeVersion = serde_json::from_str(runtime_str).unwrap();

	let finalized_block_runtime =
		Some(RuntimeEvent::Valid(RuntimeVersionEvent { spec: runtime.clone().into() }));
	// Runtime must always be reported with the first event.
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime,
		with_runtime: false,
	});
	pretty_assertions::assert_eq!(event, expected);

	// Then the best block
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", finalized_hash),
	});
	pretty_assertions::assert_eq!(event, expected);

	// Import a new block without runtime changes.
	// The runtime field must be None in this case.
	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let best_hash = block.hash();
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", best_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", best_hash),
	});
	assert_eq!(event, expected);

	client.finalize_block(best_hash, None).unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", best_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);

	let finalized_hash = best_hash;
	// The `RuntimeVersion` is embedded into the WASM blob at the `runtime_version`
	// section. Modify the `RuntimeVersion` and commit the changes to a new block.
	// The RPC must notify the runtime event change.
	let wasm = sp_maybe_compressed_blob::decompress(
		runtime::wasm_binary_unwrap(),
		sp_maybe_compressed_blob::CODE_BLOB_BOMB_LIMIT,
	)
	.unwrap();
	// Update the runtime spec version.
	let mut runtime = runtime;
	runtime.spec_version += 1;
	let embedded = sp_version::embed::embed_runtime_version(&wasm, runtime.clone()).unwrap();
	let wasm = sp_maybe_compressed_blob::compress_strongly(
		&embedded,
		sp_maybe_compressed_blob::CODE_BLOB_BOMB_LIMIT,
	)
	.unwrap();

	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(best_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap();
	builder.push_storage_change(CODE.to_vec(), Some(wasm)).unwrap();
	let block = builder.build().unwrap().block;
	let best_hash = block.header.hash();
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	let new_runtime =
		Some(RuntimeEvent::Valid(RuntimeVersionEvent { spec: runtime.clone().into() }));
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", best_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime,
		with_runtime: false,
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn get_header() {
	let (_client, api, _sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.header.hash());
	let invalid_hash = hex_string(&INVALID_HASH);

	// Invalid subscription ID must produce no results.
	let res: Option<String> = api
		.call("chainHead_v1_header", ["invalid_sub_id", &invalid_hash])
		.await
		.unwrap();
	assert!(res.is_none());

	// Valid subscription with invalid block hash will error.
	let err = api
		.call::<_, serde_json::Value>("chainHead_v1_header", [&sub_id, &invalid_hash])
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// Obtain the valid header.
	let res: String = api.call("chainHead_v1_header", [&sub_id, &block_hash]).await.unwrap();
	let bytes = array_bytes::hex2bytes(&res).unwrap();
	let header: Header = Decode::decode(&mut &bytes[..]).unwrap();
	assert_eq!(header, block.header);
}

#[tokio::test]
async fn get_body() {
	let (client, api, mut block_sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.header.hash());
	let invalid_hash = hex_string(&INVALID_HASH);

	// Subscription ID is invalid.
	let response: MethodResponse =
		api.call("chainHead_v1_body", ["invalid_sub_id", &invalid_hash]).await.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);

	// Block hash is invalid.
	let err = api
		.call::<_, serde_json::Value>("chainHead_v1_body", [&sub_id, &invalid_hash])
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// Valid call.
	let response: MethodResponse =
		api.call("chainHead_v1_body", [&sub_id, &block_hash]).await.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	// Response propagated to `chainHead_follow`.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationBodyDone(done) if done.operation_id == operation_id && done.value.is_empty()
	);

	// Import a block with extrinsics.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();
	builder
		.push_transfer(runtime::Transfer {
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 42,
			nonce: 0,
		})
		.unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();
	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Valid call to a block with extrinsics.
	let response: MethodResponse =
		api.call("chainHead_v1_body", [&sub_id, &block_hash]).await.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	// Response propagated to `chainHead_follow`.
	let expected_tx = hex_string(&block.extrinsics[0].encode());
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationBodyDone(done) if done.operation_id == operation_id && done.value == vec![expected_tx]
	);
}

#[tokio::test]
async fn call_runtime() {
	let (_client, api, mut block_sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.header.hash());
	let invalid_hash = hex_string(&INVALID_HASH);

	// Subscription ID is invalid.
	let response: MethodResponse = api
		.call("chainHead_v1_call", ["invalid_sub_id", &block_hash, "BabeApi_current_epoch", "0x00"])
		.await
		.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);

	// Block hash is invalid.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_call",
			[&sub_id, &invalid_hash, "BabeApi_current_epoch", "0x00"],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// Pass an invalid parameters that cannot be decode.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_call",
			// 0x0 is invalid.
			[&sub_id, &block_hash, "BabeApi_current_epoch", "0x0"],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(err) if err.code() == super::error::json_rpc_spec::INVALID_PARAM_ERROR && err.message().contains("Invalid parameter")
	);

	// Valid call.
	let alice_id = Sr25519Keyring::Alice.to_account_id();
	// Hex encoded scale encoded bytes representing the call parameters.
	let call_parameters = hex_string(&alice_id.encode());
	let response: MethodResponse = api
		.call(
			"chainHead_v1_call",
			[&sub_id, &block_hash, "AccountNonceApi_account_nonce", &call_parameters],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	// Response propagated to `chainHead_follow`.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationCallDone(done) if done.operation_id == operation_id && done.output == "0x0000000000000000"
	);

	// The `current_epoch` takes no parameters and not draining the input buffer
	// will cause the execution to fail.
	let response: MethodResponse = api
		.call("chainHead_v1_call", [&sub_id, &block_hash, "BabeApi_current_epoch", "0x00"])
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	// Error propagated to `chainHead_follow`.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationError(error) if error.operation_id == operation_id && error.error.contains("Execution failed")
	);
}

#[tokio::test]
async fn call_runtime_without_flag() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Valid runtime call on a subscription started with `with_runtime` false.
	let alice_id = Sr25519Keyring::Alice.to_account_id();
	let call_parameters = hex_string(&alice_id.encode());
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_call",
			[&sub_id, &block_hash, "AccountNonceApi_account_nonce", &call_parameters],
		)
		.await
		.unwrap_err();

	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_RUNTIME_CALL && err.message().contains("subscription was started with `withRuntime` set to `false`")
	);
}

#[tokio::test]
async fn get_storage_hash() {
	let (client, api, mut block_sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.header.hash());
	let invalid_hash = hex_string(&INVALID_HASH);
	let key = hex_string(&KEY);

	// Subscription ID is invalid.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				"invalid_sub_id",
				&invalid_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Hash,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);

	// Block hash is invalid.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&invalid_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Hash,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// Valid call without storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Hash,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The `Done` event is generated directly since the key does not have any value associated.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Import a new block with storage changes.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();
	builder.push_storage_change(KEY.to_vec(), Some(VALUE.to_vec())).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Valid call with storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Hash,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	let expected_hash = format!("{:?}", Blake2Hasher::hash(&VALUE));
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
				res.items.len() == 1 &&
				res.items[0].key == key && res.items[0].result == StorageResultType::Hash(expected_hash)
	);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Child value set in `setup_api`.
	let child_info = hex_string(&CHILD_STORAGE_KEY);
	let genesis_hash = format!("{:?}", client.genesis_hash());

	// Valid call with storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&genesis_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Hash,
					pagination_start_key: None
				}],
				&child_info
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	let expected_hash = format!("{:?}", Blake2Hasher::hash(&CHILD_VALUE));
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
				res.items.len() == 1 &&
				res.items[0].key == key && res.items[0].result == StorageResultType::Hash(expected_hash)
	);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);
}

#[tokio::test]
async fn get_storage_multi_query_iter() {
	let (client, api, mut block_sub, sub_id, block) = setup_api().await;
	let key = hex_string(&KEY);

	// Import a new block with storage changes.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();
	builder.push_storage_change(KEY.to_vec(), Some(VALUE.to_vec())).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Valid call with storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![
					StorageQuery {
						key: key.clone(),
						query_type: StorageQueryType::DescendantsHashes,
						pagination_start_key: None
					},
					StorageQuery {
						key: key.clone(),
						query_type: StorageQueryType::DescendantsValues,
						pagination_start_key: None
					}
				]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	let expected_hash = format!("{:?}", Blake2Hasher::hash(&VALUE));
	let expected_value = hex_string(&VALUE);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == key &&
			res.items[0].result == StorageResultType::Hash(expected_hash)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == key &&
			res.items[0].result == StorageResultType::Value(expected_value)
	);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Child value set in `setup_api`.
	let child_info = hex_string(&CHILD_STORAGE_KEY);
	let genesis_hash = format!("{:?}", client.genesis_hash());
	let expected_hash = format!("{:?}", Blake2Hasher::hash(&CHILD_VALUE));
	let expected_value = hex_string(&CHILD_VALUE);
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&genesis_hash,
				vec![
					StorageQuery {
						key: key.clone(),
						query_type: StorageQueryType::DescendantsHashes,
						pagination_start_key: None
					},
					StorageQuery {
						key: key.clone(),
						query_type: StorageQueryType::DescendantsValues,
						pagination_start_key: None
					}
				],
				&child_info
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == key &&
			res.items[0].result == StorageResultType::Hash(expected_hash)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == key &&
			res.items[0].result == StorageResultType::Value(expected_value)
	);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);
}

#[tokio::test]
async fn get_storage_value() {
	let (client, api, mut block_sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.hash());
	let invalid_hash = hex_string(&INVALID_HASH);
	let key = hex_string(&KEY);

	// Subscription ID is invalid.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				"invalid_sub_id",
				&invalid_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);

	// Block hash is invalid.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&invalid_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// Valid call without storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The `Done` event is generated directly since the key does not have any value associated.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Import a new block with storage changes.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();
	builder.push_storage_change(KEY.to_vec(), Some(VALUE.to_vec())).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut block_sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Valid call with storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	let expected_value = hex_string(&VALUE);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
				res.items.len() == 1 &&
				res.items[0].key == key && res.items[0].result == StorageResultType::Value(expected_value)
	);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Child value set in `setup_api`.
	let child_info = hex_string(&CHILD_STORAGE_KEY);
	let genesis_hash = format!("{:?}", client.genesis_hash());

	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&genesis_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}],
				&child_info
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	let expected_value = hex_string(&CHILD_VALUE);

	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
				res.items.len() == 1 &&
				res.items[0].key == key && res.items[0].result == StorageResultType::Value(expected_value)
	);
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);
}

#[tokio::test]
async fn get_storage_non_queryable_key() {
	let (mut _client, api, mut block_sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.header.hash());
	let key = hex_string(&KEY);

	// Key is prefixed by CHILD_STORAGE_KEY_PREFIX.
	let mut prefixed_key = well_known_keys::CHILD_STORAGE_KEY_PREFIX.to_vec();
	prefixed_key.extend_from_slice(&KEY);
	let prefixed_key = hex_string(&prefixed_key);

	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: prefixed_key,
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The `Done` event is generated directly since the key is not queryable.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Key is prefixed by DEFAULT_CHILD_STORAGE_KEY_PREFIX.
	let mut prefixed_key = well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec();
	prefixed_key.extend_from_slice(&KEY);
	let prefixed_key = hex_string(&prefixed_key);
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: prefixed_key,
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The `Done` event is generated directly since the key is not queryable.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Child key is prefixed by CHILD_STORAGE_KEY_PREFIX.
	let mut prefixed_key = well_known_keys::CHILD_STORAGE_KEY_PREFIX.to_vec();
	prefixed_key.extend_from_slice(CHILD_STORAGE_KEY);
	let prefixed_key = hex_string(&prefixed_key);
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: key.clone(),
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}],
				&prefixed_key
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The `Done` event is generated directly since the key is not queryable.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	// Child key is prefixed by DEFAULT_CHILD_STORAGE_KEY_PREFIX.
	let mut prefixed_key = well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec();
	prefixed_key.extend_from_slice(CHILD_STORAGE_KEY);
	let prefixed_key = hex_string(&prefixed_key);
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key,
					query_type: StorageQueryType::Value,
					pagination_start_key: None
				}],
				&prefixed_key
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The `Done` event is generated directly since the key is not queryable.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut block_sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);
}

#[tokio::test]
async fn unique_operation_ids() {
	let (mut _client, api, mut block_sub, sub_id, block) = setup_api().await;
	let block_hash = format!("{:?}", block.header.hash());

	let mut op_ids = HashSet::new();

	// Ensure that operation IDs are unique for multiple method calls.
	for _ in 0..5 {
		// Valid `chainHead_v1_body` call.
		let response: MethodResponse =
			api.call("chainHead_v1_body", [&sub_id, &block_hash]).await.unwrap();
		let operation_id = match response {
			MethodResponse::Started(started) => started.operation_id,
			MethodResponse::LimitReached => panic!("Expected started response"),
		};
		assert_matches!(
				get_next_event::<FollowEvent<String>>(&mut block_sub).await,
				FollowEvent::OperationBodyDone(done) if done.operation_id == operation_id && done.value.is_empty()
		);
		// Ensure uniqueness.
		assert!(op_ids.insert(operation_id));

		// Valid `chainHead_v1_storage` call.
		let key = hex_string(&KEY);
		let response: MethodResponse = api
			.call(
				"chainHead_v1_storage",
				rpc_params![
					&sub_id,
					&block_hash,
					vec![StorageQuery {
						key: key.clone(),
						query_type: StorageQueryType::Value,
						pagination_start_key: None
					}]
				],
			)
			.await
			.unwrap();
		let operation_id = match response {
			MethodResponse::Started(started) => started.operation_id,
			MethodResponse::LimitReached => panic!("Expected started response"),
		};
		// The `Done` event is generated directly since the key does not have any value associated.
		assert_matches!(
				get_next_event::<FollowEvent<String>>(&mut block_sub).await,
				FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
		);
		// Ensure uniqueness.
		assert!(op_ids.insert(operation_id));

		// Valid `chainHead_v1_call` call.
		let alice_id = Sr25519Keyring::Alice.to_account_id();
		let call_parameters = hex_string(&alice_id.encode());
		let response: MethodResponse = api
			.call(
				"chainHead_v1_call",
				[&sub_id, &block_hash, "AccountNonceApi_account_nonce", &call_parameters],
			)
			.await
			.unwrap();
		let operation_id = match response {
			MethodResponse::Started(started) => started.operation_id,
			MethodResponse::LimitReached => panic!("Expected started response"),
		};
		// Response propagated to `chainHead_follow`.
		assert_matches!(
				get_next_event::<FollowEvent<String>>(&mut block_sub).await,
				FollowEvent::OperationCallDone(done) if done.operation_id == operation_id && done.output == "0x0000000000000000"
		);
		// Ensure uniqueness.
		assert!(op_ids.insert(operation_id));
	}
}

#[tokio::test]
async fn separate_operation_ids_for_subscriptions() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	// Create two separate subscriptions.
	let mut sub_first = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	let sub_id_first = sub_first.subscription_id();
	let sub_id_first = serde_json::to_string(&sub_id_first).unwrap();

	let mut sub_second = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	let sub_id_second = sub_second.subscription_id();
	let sub_id_second = serde_json::to_string(&sub_id_second).unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();
	let block_hash = format!("{:?}", block.header.hash());

	// Ensure the imported block is propagated and pinned.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_first).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_first).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_first).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_first).await,
		FollowEvent::BestBlockChanged(_)
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_second).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_second).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_second).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub_second).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Each `chainHead_follow` subscription receives a separate operation ID.
	let response: MethodResponse =
		api.call("chainHead_v1_body", [&sub_id_first, &block_hash]).await.unwrap();
	let operation_id: String = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	assert_eq!(operation_id, "0");

	let response: MethodResponse =
		api.call("chainHead_v1_body", [&sub_id_second, &block_hash]).await.unwrap();
	let operation_id_second: String = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};
	// The second subscription does not increment the operation ID of the first one.
	assert_eq!(operation_id_second, "0");
}

#[tokio::test]
async fn follow_generates_initial_blocks() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;

	// Block tree:
	// finalized -> block 1 -> block 2 -> block 3
	//           -> block 1 -> block 2_f
	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.header.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let block_2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_hash = block_2.header.hash();
	client.import(BlockOrigin::Own, block_2.clone()).await.unwrap();

	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap();
	// This push is required as otherwise block 3 has the same hash as block 2 and won't get
	// imported
	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_2_f = block_builder.build().unwrap().block;
	let block_2_f_hash = block_2_f.header.hash();
	client.import(BlockOrigin::Own, block_2_f.clone()).await.unwrap();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Check block 1.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_1_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Check block 2.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	// Check block 3.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_f_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_hash),
	});
	assert_eq!(event, expected);

	// Import block 3.
	let block_3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2_hash)
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_3_hash = block_3.hash();
	client.import(BlockOrigin::Own, block_3.clone()).await.unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_3_hash),
		parent_block_hash: format!("{:?}", block_2_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_3_hash),
	});
	assert_eq!(event, expected);

	// Check the finalized event:
	//  - blocks 1, 2, 4 from canonical chain are finalized
	//  - block 3 from the fork is pruned
	client.finalize_block(block_3_hash, None).unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![
			format!("{:?}", block_1_hash),
			format!("{:?}", block_2_hash),
			format!("{:?}", block_3_hash),
		],
		pruned_block_hashes: vec![format!("{:?}", block_2_f_hash)],
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn follow_exceeding_pinned_blocks() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: 2,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;

	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Block tree:
	//   finalized_block -> block -> block2
	// The first 2 blocks are pinned into the subscription, but the block2 will exceed the limit (2
	// blocks).
	let block2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block2.clone()).await.unwrap();

	assert_matches!(get_next_event::<FollowEvent<String>>(&mut sub).await, FollowEvent::Stop);

	// Subscription will not produce any more event for further blocks.
	let block3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block2.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block3.clone()).await.unwrap();

	assert!(sub.next::<FollowEvent<String>>().await.is_none());
}

#[tokio::test]
async fn follow_with_unpin() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: 2,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Unpin an invalid subscription ID must return Ok(()).
	let invalid_hash = hex_string(&INVALID_HASH);
	let _res: () = api
		.call("chainHead_v1_unpin", rpc_params!["invalid_sub_id", &invalid_hash])
		.await
		.unwrap();

	// Valid subscription with invalid block hash.
	let invalid_hash = hex_string(&INVALID_HASH);
	let err = api
		.call::<_, serde_json::Value>("chainHead_v1_unpin", rpc_params![&sub_id, &invalid_hash])
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// To not exceed the number of pinned blocks, we need to unpin before the next import.
	let _res: () = api.call("chainHead_v1_unpin", rpc_params![&sub_id, &block_hash]).await.unwrap();

	// Block tree:
	//   finalized_block -> block -> block2
	//                      ^ has been unpinned
	let block2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block2.clone()).await.unwrap();

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	let block3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block2.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block3.clone()).await.unwrap();

	assert_matches!(get_next_event::<FollowEvent<String>>(&mut sub).await, FollowEvent::Stop);
	assert!(sub.next::<FollowEvent<String>>().await.is_none());
}

#[tokio::test]
async fn unpin_duplicate_hashes() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: 3,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Try to unpin duplicate hashes.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_unpin",
			rpc_params![&sub_id, vec![&block_hash, &block_hash]],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(err) if err.code() == super::error::rpc_spec_v2::INVALID_DUPLICATE_HASHES && err.message() == "Received duplicate hashes for the `chainHead_unpin` method"
	);

	// Block tree:
	//   finalized_block -> block -> block2
	let block2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_hash_2 = format!("{:?}", block2.header.hash());
	client.import(BlockOrigin::Own, block2.clone()).await.unwrap();

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Try to unpin duplicate hashes.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_unpin",
			rpc_params![&sub_id, vec![&block_hash, &block_hash_2, &block_hash]],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(err) if err.code() == super::error::rpc_spec_v2::INVALID_DUPLICATE_HASHES && err.message() == "Received duplicate hashes for the `chainHead_unpin` method"
	);

	// Can unpin blocks.
	let _res: () = api
		.call("chainHead_v1_unpin", rpc_params![&sub_id, vec![&block_hash, &block_hash_2]])
		.await
		.unwrap();
}

#[tokio::test]
async fn follow_with_multiple_unpin_hashes() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	// Import 3 blocks.
	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.header.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let block_2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_hash = block_2.header.hash();
	client.import(BlockOrigin::Own, block_2.clone()).await.unwrap();

	let block_3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_3_hash = block_3.header.hash();
	client.import(BlockOrigin::Own, block_3.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Unpin an invalid subscription ID must return Ok(()).
	let invalid_hash = hex_string(&INVALID_HASH);
	let _res: () = api
		.call("chainHead_v1_unpin", rpc_params!["invalid_sub_id", &invalid_hash])
		.await
		.unwrap();

	// Valid subscription with invalid block hash.
	let err = api
		.call::<_, serde_json::Value>("chainHead_v1_unpin", rpc_params![&sub_id, &invalid_hash])
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	let _res: () = api
		.call("chainHead_v1_unpin", rpc_params![&sub_id, &block_1_hash])
		.await
		.unwrap();

	// One block hash is invalid. Block 1 is already unpinned.
	let err = api
		.call::<_, serde_json::Value>(
			"chainHead_v1_unpin",
			rpc_params![&sub_id, vec![&block_1_hash, &block_2_hash, &block_3_hash]],
		)
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	// Unpin multiple blocks.
	let _res: () = api
		.call("chainHead_v1_unpin", rpc_params![&sub_id, vec![&block_2_hash, &block_3_hash]])
		.await
		.unwrap();

	// Check block 2 and 3 are unpinned.
	let err = api
		.call::<_, serde_json::Value>("chainHead_v1_unpin", rpc_params![&sub_id, &block_2_hash])
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);

	let err = api
		.call::<_, serde_json::Value>("chainHead_v1_unpin", rpc_params![&sub_id, &block_3_hash])
		.await
		.unwrap_err();
	assert_matches!(err,
		Error::JsonRpc(ref err) if err.code() == super::error::rpc_spec_v2::INVALID_BLOCK_ERROR && err.message() == "Invalid block hash"
	);
}

#[tokio::test]
async fn follow_prune_best_block() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Then the best block
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", finalized_hash),
	});
	assert_eq!(event, expected);

	// Block tree:
	//
	// finalized -> block 1 -> block 2
	//                         ^^^ best block reported
	//
	//           -> block 1 -> block 2_f -> block 4
	//                                    ^^^ finalized
	//
	// The block 4 is needed on the longest chain because we want the
	// best block 2 to be reported as pruned. Pruning is happening at
	// height (N - 1), where N is the finalized block number.

	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let block_2_f = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_f_hash = block_2_f.hash();
	client.import(BlockOrigin::Own, block_2_f.clone()).await.unwrap();

	let block_4 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2_f_hash)
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_4_hash = block_4.hash();
	client.import(BlockOrigin::Own, block_4.clone()).await.unwrap();

	// Import block 2 as best on the fork.
	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap();
	// This push is required as otherwise block 3 has the same hash as block 2 and won't get
	// imported
	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_2 = block_builder.build().unwrap().block;
	let block_2_hash = block_2.header.hash();
	client.import_as_best(BlockOrigin::Own, block_2.clone()).await.unwrap();

	// Check block 1.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_1_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_1_hash),
	});
	assert_eq!(event, expected);

	// Check block 3.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_f_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_f_hash),
	});
	assert_eq!(event, expected);

	// Check block 4.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_4_hash),
		parent_block_hash: format!("{:?}", block_2_f_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_4_hash),
	});
	assert_eq!(event, expected);

	// Check block 2, that we imported as custom best.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_hash),
	});
	assert_eq!(event, expected);

	// Finalize the block 4 from the fork.
	client.finalize_block(block_4_hash, None).unwrap();

	// Expect to report the best block changed before the finalized event.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_4_hash),
	});
	assert_eq!(event, expected);

	// Block 2 must be reported as pruned, even if it was the previous best.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![
			format!("{:?}", block_1_hash),
			format!("{:?}", block_2_f_hash),
			format!("{:?}", block_4_hash),
		],
		pruned_block_hashes: vec![format!("{:?}", block_2_hash)],
	});
	assert_eq!(event, expected);

	// Pruned hash can be unpinned.
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();
	let hash = format!("{:?}", block_2_hash);
	let _res: () = api.call("chainHead_v1_unpin", rpc_params![&sub_id, &hash]).await.unwrap();
}

#[tokio::test]
async fn follow_forks_pruned_block() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	// Block tree before the subscription:
	//
	// finalized -> block 1 -> block 2 -> block 3
	//                                        ^^^ finalized
	//           -> block 1 -> block 2_f -> block 3_f
	//
	let finalized_hash = client.info().finalized_hash;

	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.header.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let block_2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_hash = block_2.header.hash();
	client.import(BlockOrigin::Own, block_2.clone()).await.unwrap();

	let block_3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_3_hash = block_3.header.hash();
	client.import(BlockOrigin::Own, block_3.clone()).await.unwrap();

	// Block 4 with parent Block 1 is not the best imported.
	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();
	// This push is required as otherwise block 4 has the same hash as block 2 and won't get
	// imported
	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_2_f = block_builder.build().unwrap().block;
	client.import(BlockOrigin::Own, block_2_f.clone()).await.unwrap();

	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2_f.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap();
	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Bob.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_3_f = block_builder.build().unwrap().block;
	client.import(BlockOrigin::Own, block_3_f.clone()).await.unwrap();

	// Block 2_f and 3_f are not pruned, pruning happens at height (N - 1).
	client.finalize_block(block_3_hash, None).unwrap();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![
			format!("{:?}", finalized_hash),
			format!("{:?}", block_1_hash),
			format!("{:?}", block_2_hash),
			format!("{:?}", block_3_hash),
		],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_3_hash),
	});
	assert_eq!(event, expected);

	// Block tree:
	//
	// finalized -> block 1 -> block 2 -> block 3 -> block 4
	//                                                  ^^^ finalized
	//           -> block 1 -> block 2_f -> block 3_f
	//
	// Mark block 4 as finalized to force block 2_f and 3_f to get pruned.

	let block_4 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_3.hash())
		.with_parent_block_number(3)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_4_hash = block_4.hash();
	client.import(BlockOrigin::Own, block_4.clone()).await.unwrap();

	client.finalize_block(block_4_hash, None).unwrap();

	// Check block 6.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_4_hash),
		parent_block_hash: format!("{:?}", block_3_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_4_hash),
	});
	assert_eq!(event, expected);

	// Block 4 and 5 must not be reported as pruned.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", block_4_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn follow_report_multiple_pruned_block() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	// Block tree:
	//
	// finalized -> block 1 -> block 2 -> block 3
	//                                        ^^^ finalized after subscription
	//           -> block 1 -> block 2_f -> block 3_f

	let finalized_hash = client.info().finalized_hash;

	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let block_2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_hash = block_2.hash();
	client.import(BlockOrigin::Own, block_2.clone()).await.unwrap();

	let block_3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_3_hash = block_3.hash();
	client.import(BlockOrigin::Own, block_3.clone()).await.unwrap();

	// Block 2_f with parent Block 1 is not the best imported.
	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();

	// This push is required as otherwise block 2_f has the same hash as block 2 and won't get
	// imported
	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_2_f = block_builder.build().unwrap().block;
	let block_2_f_hash = block_2_f.hash();
	client.import(BlockOrigin::Own, block_2_f.clone()).await.unwrap();

	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2_f.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap();

	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Bob.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_3_f = block_builder.build().unwrap().block;
	let block_3_f_hash = block_3_f.hash();
	client.import(BlockOrigin::Own, block_3_f.clone()).await.unwrap();
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_1_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_3_hash),
		parent_block_hash: format!("{:?}", block_2_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// The fork must also be reported.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_f_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_3_f_hash),
		parent_block_hash: format!("{:?}", block_2_f_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// The best block of the chain must also be reported.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_3_hash),
	});
	assert_eq!(event, expected);

	// Block 4 and 5 are not pruned, pruning happens at height (N - 1).
	client.finalize_block(block_3_hash, None).unwrap();

	// Finalizing block 3 directly will also result in block 1 and 2 being finalized.
	// It will also mark block 2 and block 3 from the fork as pruned.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;

	// Sort the hashes to prevent flaky tests.
	let mut pruned_block_hashes =
		vec![format!("{:?}", block_2_f_hash), format!("{:?}", block_3_f_hash)];
	pruned_block_hashes.sort();

	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![
			format!("{:?}", block_1_hash),
			format!("{:?}", block_2_hash),
			format!("{:?}", block_3_hash),
		],
		pruned_block_hashes,
	});
	assert_eq!(event, expected);

	// Block tree:
	//
	// finalized -> block 1 -> block 2 -> block 3 -> block 4
	//                                                  ^^^ finalized
	//           -> block 1 -> block 2_f -> block 3_f
	//

	let block_4 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_3.hash())
		.with_parent_block_number(3)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;

	let block_4_hash = block_4.hash();
	client.import(BlockOrigin::Own, block_4.clone()).await.unwrap();

	client.finalize_block(block_4_hash, None).unwrap();

	// Check block 6.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_4_hash),
		parent_block_hash: format!("{:?}", block_3_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_4_hash),
	});
	assert_eq!(event, expected);

	// Blocks from the fork were pruned earlier.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", block_4_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn pin_block_references() {
	// Manually construct an in-memory backend and client.
	let backend = Arc::new(sc_client_api::in_mem::Backend::new());
	let executor = substrate_test_runtime_client::WasmExecutor::default();
	let client_config = sc_service::ClientConfig::default();

	let genesis_block_builder = sc_service::GenesisBlockBuilder::new(
		&substrate_test_runtime_client::GenesisParameters::default().genesis_storage(),
		!client_config.no_genesis,
		backend.clone(),
		executor.clone(),
	)
	.unwrap();

	let client = Arc::new(
		new_with_backend::<_, _, Block, _, RuntimeApi>(
			backend.clone(),
			executor,
			genesis_block_builder,
			Box::new(TokioTestExecutor::default()),
			None,
			None,
			client_config,
		)
		.unwrap(),
	);

	let api = ChainHead::new(
		client.clone(),
		backend.clone(),
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: 3,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	async fn wait_pinned_references<Block: BlockT>(
		backend: &Arc<sc_client_api::in_mem::Backend<Block>>,
		hash: &Block::Hash,
		target: i64,
	) {
		// Retry for at most 2 minutes.
		let mut retries = 120;
		while backend.pin_refs(hash).unwrap() != target {
			if retries == 0 {
				panic!("Expected target={} pinned references for hash={:?}", target, hash);
			}
			retries -= 1;

			tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
		}
	}

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let hash = block.header.hash();
	let block_hash = format!("{:?}", hash);
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// We need to wait a bit for:
	// 1. `NewBlock` and `BestBlockChanged` notifications to propagate to the chainHead
	// subscription. (pin_refs == 2)
	// 2. The chainHead to call `pin_blocks` only once for the `NewBlock`
	// notification (pin_refs == 3)
	// 3. Both notifications to go out of scope (pin_refs ==  1 (total 3 - dropped 2)).
	wait_pinned_references(&backend, &hash, 1).await;

	// To not exceed the number of pinned blocks, we need to unpin before the next import.
	let _res: () = api.call("chainHead_v1_unpin", rpc_params![&sub_id, &block_hash]).await.unwrap();

	// Make sure unpin clears out the reference.
	let refs = backend.pin_refs(&hash).unwrap();
	assert_eq!(refs, 0);

	// Add another 2 blocks and make sure we drop the subscription with the blocks pinned.
	let mut hashes = Vec::new();
	for _ in 0..2 {
		let block = BlockBuilderBuilder::new(&*client)
			.on_parent_block(client.chain_info().best_hash)
			.with_parent_block_number(client.chain_info().best_number)
			.build()
			.unwrap()
			.build()
			.unwrap()
			.block;
		let hash = block.hash();
		client.import(BlockOrigin::Own, block.clone()).await.unwrap();

		// Ensure the imported block is propagated for this subscription.
		assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut sub).await,
			FollowEvent::NewBlock(_)
		);
		assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut sub).await,
			FollowEvent::BestBlockChanged(_)
		);

		hashes.push(hash);
	}

	// Make sure the pin was propagated.
	for hash in &hashes {
		wait_pinned_references(&backend, hash, 1).await;
	}

	// Drop the subscription and expect the pinned blocks to be released.
	drop(sub);
	// The `chainHead` detects the subscription was terminated when it tries
	// to send another block.
	let block = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().best_hash)
		.with_parent_block_number(client.chain_info().best_number)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	for hash in &hashes {
		wait_pinned_references(&backend, &hash, 0).await;
	}
}

#[tokio::test]
async fn follow_finalized_before_new_block() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let client_mock = Arc::new(ChainHeadMockClient::new(client.clone()));

	let api = ChainHead::new(
		client_mock.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	// Make sure the block is imported for it to be pinned.
	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.header.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Trigger the `FinalizedNotification` for block 1 before the `BlockImportNotification`, and
	// expect for the `chainHead` to generate `NewBlock`, `BestBlock` and `Finalized` events.

	// Trigger the Finalized notification before the NewBlock one.
	run_with_timeout(client_mock.trigger_finality_stream(block_1.header.clone(), vec![])).await;

	// Initialized must always be reported first.
	let finalized_hash = client.info().finalized_hash;
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Block 1 must be reported because we triggered the finalized notification.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_1_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_1_hash),
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", block_1_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);

	let block_2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_hash = block_2.hash();
	client.import(BlockOrigin::Own, block_2.clone()).await.unwrap();

	// Triggering the `BlockImportNotification` notification for block 1 should have no effect
	// on the notification because the events were handled by the `FinalizedNotification`.
	// Also trigger the `BlockImportNotification` notification for block 2 to ensure
	// `NewBlock and `BestBlock` events are generated.

	// Trigger NewBlock notification for block 1 and block 2.
	run_with_timeout(client_mock.trigger_import_stream(block_1.header)).await;
	run_with_timeout(client_mock.trigger_import_stream(block_2.header)).await;

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_hash),
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn operation_limit_bounds_concurrency_not_items() {
	let child_info = ChildInfo::new_default(CHILD_STORAGE_KEY);
	let builder = TestClientBuilder::new().add_extra_child_storage(
		&child_info,
		KEY.to_vec(),
		CHILD_VALUE.to_vec(),
	);
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	// A single permit, against a four-item query below. `reserve_at_most` takes what is
	// available so a request larger than the remaining capacity is
	// still admitted, and it still delivers every item: the limit bounds how many operations
	// run concurrently, never the size or the result of any one of them.
	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: 1,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	// Import a block with a value at `KEY`, so every query item has a result to deliver.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap();
	builder.push_storage_change(KEY.to_vec(), Some(VALUE.to_vec())).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	let key = hex_string(&KEY);
	let items = vec![
		StorageQuery {
			key: key.clone(),
			query_type: StorageQueryType::DescendantsHashes,
			pagination_start_key: None,
		},
		StorageQuery {
			key: key.clone(),
			query_type: StorageQueryType::DescendantsHashes,
			pagination_start_key: None,
		},
		StorageQuery {
			key: key.clone(),
			query_type: StorageQueryType::DescendantsValues,
			pagination_start_key: None,
		},
		StorageQuery {
			key: key.clone(),
			query_type: StorageQueryType::DescendantsValues,
			pagination_start_key: None,
		},
	];

	let response: MethodResponse = api
		.call("chainHead_v1_storage", rpc_params![&sub_id, &block_hash, items])
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => {
			// Admitted on one permit, and nothing is dropped to fit.
			assert_eq!(started.discarded_items, Some(0));
			started.operation_id
		},
		MethodResponse::LimitReached => {
			panic!("a query larger than the remaining capacity must still be admitted")
		},
	};

	// All four items are delivered, in query order, then the operation completes.
	let expected_hash = format!("{:?}", Blake2Hasher::hash(&VALUE));
	let expected_value = hex_string(&VALUE);
	for expected in [
		StorageResultType::Hash(expected_hash.clone()),
		StorageResultType::Hash(expected_hash),
		StorageResultType::Value(expected_value.clone()),
		StorageResultType::Value(expected_value),
	] {
		assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut sub).await,
			FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
				res.items.len() == 1 &&
				res.items[0].key == key &&
				res.items[0].result == expected
		);
	}
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);
}

#[tokio::test]
async fn storage_is_backpressured() {
	let child_info = ChildInfo::new_default(CHILD_STORAGE_KEY);
	let builder = TestClientBuilder::new().add_extra_child_storage(
		&child_info,
		KEY.to_vec(),
		CHILD_VALUE.to_vec(),
	);
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	// Configure the chainHead with maximum 1 item before asking for pagination.
	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	// Import a new block with storage changes.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap();
	builder.push_storage_change(b":m".to_vec(), Some(b"a".to_vec())).unwrap();
	builder.push_storage_change(b":mo".to_vec(), Some(b"ab".to_vec())).unwrap();
	builder.push_storage_change(b":moc".to_vec(), Some(b"abc".to_vec())).unwrap();
	builder.push_storage_change(b":moD".to_vec(), Some(b"abcmoD".to_vec())).unwrap();
	builder.push_storage_change(b":mock".to_vec(), Some(b"abcd".to_vec())).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	let invalid_hash = hex_string(&INVALID_HASH);

	// Invalid subscription ID must produce no results.
	let _res: () = api
		.call("chainHead_v1_continue", ["invalid_sub_id", &invalid_hash])
		.await
		.unwrap();

	// Invalid operation ID must produce no results.
	let _res: () = api.call("chainHead_v1_continue", [&sub_id, &invalid_hash]).await.unwrap();

	// Valid call with storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: hex_string(b":m"),
					query_type: StorageQueryType::DescendantsValues,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":m") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"a"))
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":mo") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"ab"))
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":moD") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"abcmoD"))
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":moc") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"abc"))
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":mock") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"abcd"))
	);

	// Finished.
	assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut sub).await,
			FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);
}

#[tokio::test]
async fn stop_storage_operation() {
	let child_info = ChildInfo::new_default(CHILD_STORAGE_KEY);
	let builder = TestClientBuilder::new().add_extra_child_storage(
		&child_info,
		KEY.to_vec(),
		CHILD_VALUE.to_vec(),
	);
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	// Configure the chainHead with maximum 1 item before asking for pagination.
	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();

	// Import a new block with storage changes.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap();
	builder.push_storage_change(b":m".to_vec(), Some(b"a".to_vec())).unwrap();
	builder.push_storage_change(b":mo".to_vec(), Some(b"ab".to_vec())).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	let invalid_hash = hex_string(&INVALID_HASH);

	// Invalid subscription ID must produce no results.
	let _res: () = api
		.call("chainHead_v1_stopOperation", ["invalid_sub_id", &invalid_hash])
		.await
		.unwrap();

	// Invalid operation ID must produce no results.
	let _res: () = api.call("chainHead_v1_stopOperation", [&sub_id, &invalid_hash]).await.unwrap();

	// Valid call with storage at the key.
	let response: MethodResponse = api
		.call(
			"chainHead_v1_storage",
			rpc_params![
				&sub_id,
				&block_hash,
				vec![StorageQuery {
					key: hex_string(b":m"),
					query_type: StorageQueryType::DescendantsValues,
					pagination_start_key: None
				}]
			],
		)
		.await
		.unwrap();
	let operation_id = match response {
		MethodResponse::Started(started) => started.operation_id,
		MethodResponse::LimitReached => panic!("Expected started response"),
	};

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":m") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"a"))
	);

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id &&
			res.items.len() == 1 &&
			res.items[0].key == hex_string(b":mo") &&
			res.items[0].result == StorageResultType::Value(hex_string(b"ab"))
	);

	// Stop the operation.
	let _res: () = api.call("chainHead_v1_stopOperation", [&sub_id, &operation_id]).await.unwrap();

	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id
	);

	does_not_produce_event::<FollowEvent<String>>(
		&mut sub,
		std::time::Duration::from_secs(DOES_NOT_PRODUCE_EVENTS_SECONDS),
	)
	.await;
}

#[tokio::test]
async fn storage_closest_merkle_value() {
	let (client, api, mut sub, sub_id, block) = setup_api().await;

	/// The core of this test.
	///
	/// Checks keys that are exact match, keys with descendant and keys that should not return
	/// values.
	///
	/// Returns (key, merkle value) pairs.
	async fn expect_merkle_request(
		api: &RpcModule<ChainHead<Backend, Block, Client<Backend>>>,
		mut sub: &mut RpcSubscription,
		sub_id: String,
		block_hash: String,
	) -> HashMap<String, String> {
		// Valid call with storage at the keys.
		let response: MethodResponse = api
			.call(
				"chainHead_v1_storage",
				rpc_params![
					&sub_id,
					&block_hash,
					vec![
						StorageQuery {
							key: hex_string(b":AAAA"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						StorageQuery {
							key: hex_string(b":AAAB"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						// Key with descendant.
						StorageQuery {
							key: hex_string(b":A"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						StorageQuery {
							key: hex_string(b":AA"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						// Keys below this comment do not produce a result.
						// Key that exceed the keyspace of the trie.
						StorageQuery {
							key: hex_string(b":AAAAX"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						StorageQuery {
							key: hex_string(b":AAABX"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						// Key that are not part of the trie.
						StorageQuery {
							key: hex_string(b":AAX"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
						StorageQuery {
							key: hex_string(b":AAAX"),
							query_type: StorageQueryType::ClosestDescendantMerkleValue,
							pagination_start_key: None
						},
					]
				],
			)
			.await
			.unwrap();
		let operation_id = match response {
			MethodResponse::Started(started) => {
				assert_eq!(started.discarded_items, Some(0));
				started.operation_id
			},
			MethodResponse::LimitReached => panic!("Expected started response"),
		};

		let mut merkle_values = HashMap::new();

		loop {
			match get_next_event::<FollowEvent<String>>(&mut sub).await {
				FollowEvent::OperationStorageItems(res) if res.operation_id == operation_id => {
					for res in res.items {
						let value = match res.result {
							StorageResultType::ClosestDescendantMerkleValue(value) => value,
							_ => panic!("Unexpected StorageResultType"),
						};
						merkle_values.insert(res.key, value);
					}
				},
				FollowEvent::OperationStorageDone(done) if done.operation_id == operation_id => {
					break
				},
				_ => panic!("Unexpected event"),
			}
		}

		// Response for AAAA, AAAB, A and AA.
		assert_eq!(merkle_values.len(), 4);

		// While checking for expected merkle values to align,
		// the following will check that the returned keys are
		// expected.

		// Values for AAAA and AAAB are different.
		assert_ne!(
			merkle_values.get(&hex_string(b":AAAA")).unwrap(),
			merkle_values.get(&hex_string(b":AAAB")).unwrap()
		);

		// Values for A and AA should be on the same branch node.
		assert_eq!(
			merkle_values.get(&hex_string(b":A")).unwrap(),
			merkle_values.get(&hex_string(b":AA")).unwrap()
		);
		// The branch node value must be different than the leaf of either
		// AAAA and AAAB.
		assert_ne!(
			merkle_values.get(&hex_string(b":A")).unwrap(),
			merkle_values.get(&hex_string(b":AAAA")).unwrap()
		);
		assert_ne!(
			merkle_values.get(&hex_string(b":A")).unwrap(),
			merkle_values.get(&hex_string(b":AAAB")).unwrap()
		);

		merkle_values
	}

	// Import a new block with storage changes.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(1)
		.build()
		.unwrap();
	builder.push_storage_change(b":AAAA".to_vec(), Some(vec![1; 64])).unwrap();
	builder.push_storage_change(b":AAAB".to_vec(), Some(vec![2; 64])).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	let merkle_values_lhs = expect_merkle_request(&api, &mut sub, sub_id.clone(), block_hash).await;

	// Import a new block with and change AAAB value.
	let mut builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block.hash())
		.with_parent_block_number(2)
		.build()
		.unwrap();
	builder.push_storage_change(b":AAAA".to_vec(), Some(vec![1; 64])).unwrap();
	builder.push_storage_change(b":AAAB".to_vec(), Some(vec![3; 64])).unwrap();
	let block = builder.build().unwrap().block;
	let block_hash = format!("{:?}", block.header.hash());
	client.import(BlockOrigin::Own, block.clone()).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::NewBlock(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	let merkle_values_rhs = expect_merkle_request(&api, &mut sub, sub_id.clone(), block_hash).await;

	// Change propagated to the root.
	assert_ne!(
		merkle_values_lhs.get(&hex_string(b":A")).unwrap(),
		merkle_values_rhs.get(&hex_string(b":A")).unwrap()
	);
	assert_ne!(
		merkle_values_lhs.get(&hex_string(b":AAAB")).unwrap(),
		merkle_values_rhs.get(&hex_string(b":AAAB")).unwrap()
	);
	// However the AAAA branch leaf remains unchanged.
	assert_eq!(
		merkle_values_lhs.get(&hex_string(b":AAAA")).unwrap(),
		merkle_values_rhs.get(&hex_string(b":AAAA")).unwrap()
	);
}

#[tokio::test]
async fn chain_head_stop_all_subscriptions() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	// Configure the chainHead to stop all subscriptions on lagging distance of 5 blocks.
	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: 5,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();

	// Ensure the imported block is propagated and pinned for this subscription.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::BestBlockChanged(_)
	);

	// Import 6 blocks in total to trigger the suspension distance.
	let mut parent_hash = client.chain_info().genesis_hash;
	for i in 0..6 {
		let block = BlockBuilderBuilder::new(&*client)
			.on_parent_block(parent_hash)
			.with_parent_block_number(i)
			.build()
			.unwrap()
			.build()
			.unwrap()
			.block;

		let hash = block.hash();
		parent_hash = hash;
		client.import(BlockOrigin::Own, block.clone()).await.unwrap();

		assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut sub).await,
			FollowEvent::NewBlock(_)
		);
		assert_matches!(
			get_next_event::<FollowEvent<String>>(&mut sub).await,
			FollowEvent::BestBlockChanged(_)
		);
	}

	let mut second_sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	// Lagging detected, the stop event is delivered immediately.
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut second_sub).await,
		FollowEvent::Stop
	);

	// Ensure that all subscriptions are stopped.
	assert_matches!(get_next_event::<FollowEvent<String>>(&mut sub).await, FollowEvent::Stop);

	// Other subscriptions cannot be started until the suspension period is over.
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	// Should receive the stop event immediately.
	assert_matches!(get_next_event::<FollowEvent<String>>(&mut sub).await, FollowEvent::Stop);

	// For the next subscription, lagging distance must be smaller.
	client.finalize_block(parent_hash, None).unwrap();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	assert_matches!(
		get_next_event::<FollowEvent<String>>(&mut sub).await,
		FollowEvent::Initialized(_)
	);
}

#[tokio::test]
async fn chain_head_single_connection_context() {
	let server_addr = run_server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = jsonrpsee::ws_client::WsClientBuilder::default()
		.build(&server_url)
		.await
		.unwrap();
	// Calls cannot be made from a different connection context.
	let second_client = jsonrpsee::ws_client::WsClientBuilder::default()
		.build(&server_url)
		.await
		.unwrap();

	let mut sub: RpcClientSubscription<FollowEvent<String>> =
		ChainHeadApiClient::<String>::chain_head_unstable_follow(&client, true)
			.await
			.unwrap();

	let event = tokio::time::timeout(std::time::Duration::from_secs(60), sub.next())
		.await
		.unwrap()
		.unwrap()
		.unwrap();
	let finalized_hash = match event {
		FollowEvent::Initialized(init) => init.finalized_block_hashes.into_iter().last().unwrap(),
		_ => panic!("Expected FollowEvent::Initialized"),
	};

	let first_sub_id = match sub.kind() {
		jsonrpsee::core::client::SubscriptionKind::Subscription(id) => match id {
			jsonrpsee::types::SubscriptionId::Num(num) => num.to_string(),
			jsonrpsee::types::SubscriptionId::Str(s) => s.to_string(),
		},
		_ => panic!("Unexpected subscription ID"),
	};

	// Trying to unpin from a different connection will have no effect.
	let _response = ChainHeadApiClient::<String>::chain_head_unstable_unpin(
		&second_client,
		first_sub_id.clone(),
		crate::chain_head::api::ListOrValue::Value(finalized_hash.clone()),
	)
	.await
	.unwrap();

	// Body can still be fetched from the first subscription.
	let response: MethodResponse = ChainHeadApiClient::<String>::chain_head_unstable_body(
		&client,
		first_sub_id.clone(),
		finalized_hash.clone(),
	)
	.await
	.unwrap();
	assert_matches!(response, MethodResponse::Started(_started));

	// Cannot make a call from a different connection context.
	let response: MethodResponse = ChainHeadApiClient::<String>::chain_head_unstable_body(
		&second_client,
		first_sub_id.clone(),
		finalized_hash.clone(),
	)
	.await
	.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);

	let response: Option<String> = ChainHeadApiClient::<String>::chain_head_unstable_header(
		&client,
		first_sub_id.clone(),
		finalized_hash.clone(),
	)
	.await
	.unwrap();
	assert!(response.is_some());
	// Cannot make a call from a different connection context.
	let response: Option<String> = ChainHeadApiClient::<String>::chain_head_unstable_header(
		&second_client,
		first_sub_id.clone(),
		finalized_hash.clone(),
	)
	.await
	.unwrap();
	assert!(response.is_none());

	let key = hex_string(&KEY);
	let response: MethodResponse = ChainHeadApiClient::<String>::chain_head_unstable_storage(
		&client,
		first_sub_id.clone(),
		finalized_hash.clone(),
		vec![StorageQuery {
			key: key.clone(),
			query_type: StorageQueryType::Hash,
			pagination_start_key: None,
		}],
		None,
	)
	.await
	.unwrap();
	assert_matches!(response, MethodResponse::Started(_started));
	// Cannot make a call from a different connection context.
	let response: MethodResponse = ChainHeadApiClient::<String>::chain_head_unstable_storage(
		&second_client,
		first_sub_id.clone(),
		finalized_hash.clone(),
		vec![StorageQuery {
			key: key.clone(),
			query_type: StorageQueryType::Hash,
			pagination_start_key: None,
		}],
		None,
	)
	.await
	.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);

	let alice_id = Sr25519Keyring::Alice.to_account_id();
	// Hex encoded scale encoded bytes representing the call parameters.
	let call_parameters = hex_string(&alice_id.encode());
	let response: MethodResponse = ChainHeadApiClient::<String>::chain_head_unstable_call(
		&client,
		first_sub_id.clone(),
		finalized_hash.clone(),
		"AccountNonceApi_account_nonce".into(),
		call_parameters.clone(),
	)
	.await
	.unwrap();
	assert_matches!(response, MethodResponse::Started(_started));
	// Cannot make a call from a different connection context.
	let response: MethodResponse = ChainHeadApiClient::<String>::chain_head_unstable_call(
		&second_client,
		first_sub_id.clone(),
		finalized_hash.clone(),
		"AccountNonceApi_account_nonce".into(),
		call_parameters.clone(),
	)
	.await
	.unwrap();
	assert_matches!(response, MethodResponse::LimitReached);
}

#[tokio::test]
async fn chain_head_limit_reached() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	// Maximum of 1 chainHead_follow subscription.
	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: 1,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();

	// Initialized must always be reported first.
	let _event: FollowEvent<String> = get_next_event(&mut sub).await;

	let error = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap_err();
	assert!(error
		.to_string()
		.contains("Maximum number of chainHead_follow has been reached"));

	// After dropping the subscription, other subscriptions are allowed to be created.
	drop(sub);
	// Ensure the `chainHead_unfollow` is propagated to the server.
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;

	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [true]).await.unwrap();
	// Initialized must always be reported first.
	let _event: FollowEvent<String> = get_next_event(&mut sub).await;
}

#[tokio::test]
async fn follow_unique_pruned_blocks() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();

	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Then the best block
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", finalized_hash),
	});
	assert_eq!(event, expected);

	// Block tree:
	//
	// finalized -> block 1 -> block 2 -> block 3
	//
	//                      -> block 2 -> block 4 -> block 5
	//
	//           -> block 1 -> block 2_f -> block 6
	//                                    ^^^ finalized
	//                                                 -> block 7
	//                                                    ^^^ finalized
	//                                                            -> block 8
	//                                                               ^^^ finalized
	// The chainHead will see block 5 as the best block. However, the
	// client will finalize the block 6, which is on another fork.
	//
	// When the block 6 is finalized all blocks from the stale forks (2, 3, 4, 5) are pruned.
	//

	// Initial setup steps:
	let block_1_hash =
		import_block(client.clone(), client.chain_info().genesis_hash, 0).await.hash();
	let block_2_f_hash = import_block(client.clone(), block_1_hash, 1).await.hash();
	let block_6_hash = import_block(client.clone(), block_2_f_hash, 2).await.hash();
	// Import block 2 as best on the fork.
	let mut tx_alice_ferdie = Transfer {
		from: Sr25519Keyring::Alice.into(),
		to: Sr25519Keyring::Ferdie.into(),
		amount: 41,
		nonce: 0,
	};
	let block_2_hash =
		import_best_block_with_tx(client.clone(), block_1_hash, 1, tx_alice_ferdie.clone())
			.await
			.hash();

	let block_3_hash = import_block(client.clone(), block_2_hash, 2).await.hash();
	// Fork block 4.
	tx_alice_ferdie.nonce = 1;
	let block_4_hash = import_best_block_with_tx(client.clone(), block_2_hash, 2, tx_alice_ferdie)
		.await
		.hash();
	let block_5_hash = import_block(client.clone(), block_4_hash, 3).await.hash();

	// Check expected events generated by the setup.
	{
		// Check block 1 -> block 2f -> block 6.
		check_new_and_best_block_events!(&mut sub, block_1_hash, finalized_hash);
		check_new_and_best_block_events!(&mut sub, block_2_f_hash, block_1_hash);
		check_new_and_best_block_events!(&mut sub, block_6_hash, block_2_f_hash);

		// Check (block 1 ->) block 2 -> block 3.
		check_new_and_best_block_events!(&mut sub, block_2_hash, block_1_hash);
		check_new_and_best_block_events!(&mut sub, block_3_hash, block_2_hash);

		// Check (block 1 -> block 2 ->) block 4 -> block 5.
		check_new_and_best_block_events!(&mut sub, block_4_hash, block_2_hash);
		check_new_and_best_block_events!(&mut sub, block_5_hash, block_4_hash);
	}

	// Finalize the block 6 from the fork.
	client.finalize_block(block_6_hash, None).unwrap();

	// Expect to report the best block changed before the finalized event.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_6_hash),
	});
	assert_eq!(event, expected);

	// All blocks from stale forks are pruned when we finalize block 6.
	let mut event: FollowEvent<String> = get_next_event(&mut sub).await;

	// Sort pruned block hashes to counter flaky test caused by event generation (get_pruned_hashes)
	if let FollowEvent::Finalized(Finalized { pruned_block_hashes, .. }) = &mut event {
		pruned_block_hashes.sort();
	}
	let expected_pruned_block_hashes = {
		let mut hashes = vec![
			format!("{:?}", block_2_hash),
			format!("{:?}", block_3_hash),
			format!("{:?}", block_4_hash),
			format!("{:?}", block_5_hash),
		];
		hashes.sort();
		hashes
	};

	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![
			format!("{:?}", block_1_hash),
			format!("{:?}", block_2_f_hash),
			format!("{:?}", block_6_hash),
		],
		pruned_block_hashes: expected_pruned_block_hashes,
	});

	assert_eq!(event, expected);

	// Pruned hash can be unpinned.
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();
	let hash = format!("{:?}", block_2_hash);
	let _res: () = api.call("chainHead_v1_unpin", rpc_params![&sub_id, &hash]).await.unwrap();

	// Import block 7 and check it.
	let block_7_hash = import_block(client.clone(), block_6_hash, 3).await.hash();
	check_new_and_best_block_events!(&mut sub, block_7_hash, block_6_hash);

	// Finalize the block 7.
	client.finalize_block(block_7_hash, None).unwrap();

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	// All necessary blocks were pruned on block 6 finalization.
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", block_7_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);

	// Check block 8.
	let block_8_hash = import_block(client.clone(), block_7_hash, 4).await.hash();
	check_new_and_best_block_events!(&mut sub, block_8_hash, block_7_hash);

	// Finalize the block 8.
	client.finalize_block(block_8_hash, None).unwrap();

	// All necessary blocks were pruned on block 6 finalization.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", block_8_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn follow_report_best_block_of_a_known_block() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let client_mock = Arc::new(ChainHeadMockClient::new(client.clone()));

	let api = ChainHead::new(
		client_mock.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Then the best block
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", finalized_hash),
	});
	assert_eq!(event, expected);

	// Block tree:
	//
	// finalized -> block 1 -> block 2
	//                         ^^^ best block reported
	//
	//           -> block 1 -> block 2_f -> block 3 (best)
	//                          ^^^ finalized

	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();
	let block_2_f = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_2_f_hash = block_2_f.hash();
	client.import(BlockOrigin::Own, block_2_f.clone()).await.unwrap();

	// Import block 2 as best on the fork.
	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap();
	// This push is required as otherwise block 3 has the same hash as block 2 and won't get
	// imported
	block_builder
		.push_transfer(Transfer {
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Ferdie.into(),
			amount: 41,
			nonce: 0,
		})
		.unwrap();
	let block_2 = block_builder.build().unwrap().block;
	let block_2_hash = block_2.header.hash();
	client.import_as_best(BlockOrigin::Own, block_2.clone()).await.unwrap();

	run_with_timeout(client_mock.trigger_import_stream(block_1.header.clone())).await;
	run_with_timeout(client_mock.trigger_import_stream(block_2_f.header.clone())).await;
	run_with_timeout(client_mock.trigger_import_stream(block_2.header.clone())).await;

	// Check block 1.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_1_hash),
		parent_block_hash: format!("{:?}", finalized_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_1_hash),
	});
	assert_eq!(event, expected);

	// Check block 2.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_f_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_f_hash),
	});
	assert_eq!(event, expected);

	// Check block 2, that we imported as custom best.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_2_hash),
		parent_block_hash: format!("{:?}", block_1_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_hash),
	});
	assert_eq!(event, expected);

	// Craft block 3 and import it later to simulate a race condition.
	let block_3 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_2_f_hash)
		.with_parent_block_number(2)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_3_hash = block_3.hash();

	// Set best block info to block 3, that is not announced yet.
	//
	// This simulates the following edge-case:
	// - The client imports a new block as best block.
	// - The finality stream is triggered before the block is announced.
	//
	// This generated in the past a `BestBlock` event for the block that was not announced
	// by `NewBlock` events.
	//
	// This happened because the chainHead was using the `client.info()` without verifying
	// if the block was announced or not. This was fixed by using the latest finalized
	// block instead as fallback. For more info see: https://github.com/paritytech/polkadot-sdk/issues/5512.
	client_mock.set_best_block(block_3_hash, 3);

	// Finalize the block 2 from the fork.
	client.finalize_block(block_2_f_hash, None).unwrap();
	run_with_timeout(
		client_mock.trigger_finality_stream(block_2_f.header.clone(), vec![block_2_hash]),
	)
	.await;

	// Block 2f is now the best block, not the block 3 that is not announced yet.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_2_f_hash),
	});
	assert_eq!(event, expected);
	// Block 2 must be reported as pruned, even if it was the previous best.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![
			// Note: the client mock is only reporting one block at a time.
			// format!("{:?}", block_1_hash),
			format!("{:?}", block_2_f_hash),
		],
		pruned_block_hashes: vec![format!("{:?}", block_2_hash)],
	});
	assert_eq!(event, expected);

	// Block 3 is now imported as best.
	client.import_as_best(BlockOrigin::Own, block_3.clone()).await.unwrap();
	run_with_timeout(client_mock.trigger_import_stream(block_3.header.clone())).await;

	// Check block 3.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::NewBlock(NewBlock {
		block_hash: format!("{:?}", block_3_hash),
		parent_block_hash: format!("{:?}", block_2_f_hash),
		new_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", block_3_hash),
	});
	assert_eq!(event, expected);

	// Pruned hash can be unpinned.
	let sub_id = sub.subscription_id();
	let sub_id = serde_json::to_string(&sub_id).unwrap();
	let hash = format!("{:?}", block_2_hash);
	let _res: () = api.call("chainHead_v1_unpin", rpc_params![&sub_id, &hash]).await.unwrap();

	// Finalize the block 3.
	client.finalize_block(block_3_hash, None).unwrap();
	run_with_timeout(client_mock.trigger_finality_stream(block_3.header.clone(), vec![])).await;

	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Finalized(Finalized {
		finalized_block_hashes: vec![format!("{:?}", block_3_hash)],
		pruned_block_hashes: vec![],
	});
	assert_eq!(event, expected);
}

#[tokio::test]
async fn follow_event_with_unknown_parent() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let client_mock = Arc::new(ChainHeadMockClient::new(client.clone()));

	let api = ChainHead::new(
		client_mock.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			subscription_buffer_cap: MAX_PINNED_BLOCKS,
		},
	)
	.into_rpc();

	let finalized_hash = client.info().finalized_hash;
	let mut sub = api.subscribe_unbounded("chainHead_v1_follow", [false]).await.unwrap();
	// Initialized must always be reported first.
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::Initialized(Initialized {
		finalized_block_hashes: vec![format!("{:?}", finalized_hash)],
		finalized_block_runtime: None,
		with_runtime: false,
	});
	assert_eq!(event, expected);

	// Then the best block
	let event: FollowEvent<String> = get_next_event(&mut sub).await;
	let expected = FollowEvent::BestBlockChanged(BestBlockChanged {
		best_block_hash: format!("{:?}", finalized_hash),
	});
	assert_eq!(event, expected);

	// Block tree:
	//
	// finalized -> (gap: block 1) -> block 2
	//
	// Block 1 is not announced yet. ChainHead should report the stop
	// event when encountering an unknown parent of block 2.

	// Note: `client` is used just for constructing the blocks.
	// The blocks are imported to chainHead using the `client_mock`.
	let block_1 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	let block_1_hash = block_1.hash();
	client.import(BlockOrigin::Own, block_1.clone()).await.unwrap();

	let block_2 = BlockBuilderBuilder::new(&*client)
		.on_parent_block(block_1_hash)
		.with_parent_block_number(1)
		.build()
		.unwrap()
		.build()
		.unwrap()
		.block;
	client.import(BlockOrigin::Own, block_2.clone()).await.unwrap();

	run_with_timeout(client_mock.trigger_import_stream(block_2.header)).await;
	// When importing the block 2, chainHead detects a gap in our blocks and stops.
	assert_matches!(get_next_event::<FollowEvent<String>>(&mut sub).await, FollowEvent::Stop);
}

#[tokio::test]
async fn events_are_backpressured() {
	let builder = TestClientBuilder::new();
	let backend = builder.backend();
	let client = Arc::new(builder.build());

	let api = ChainHead::new(
		client.clone(),
		backend,
		Arc::new(TokioTestExecutor::default()),
		ChainHeadConfig {
			global_max_pinned_blocks: MAX_PINNED_BLOCKS,
			subscription_max_pinned_duration: Duration::from_secs(MAX_PINNED_SECS),
			subscription_max_ongoing_operations: MAX_OPERATIONS,
			max_lagging_distance: MAX_LAGGING_DISTANCE,
			max_follow_subscriptions_per_connection: MAX_FOLLOW_SUBSCRIPTIONS_PER_CONNECTION,
			subscription_buffer_cap: 10,
		},
	)
	.into_rpc();

	let mut parent_hash = client.chain_info().genesis_hash;
	let mut header = VecDeque::new();
	let mut sub = api.subscribe("chainHead_v1_follow", [false], 1).await.unwrap();

	// insert more events than the user can consume
	for i in 0..=5 {
		let block = BlockBuilderBuilder::new(&*client)
			.on_parent_block(parent_hash)
			.with_parent_block_number(i)
			.build()
			.unwrap()
			.build()
			.unwrap()
			.block;
		header.push_front(block.header().clone());

		parent_hash = block.hash();
		client.import(BlockOrigin::Own, block.clone()).await.unwrap();
	}

	let mut events = Vec::new();

	while let Some(event) = sub.next::<FollowEvent<String>>().await {
		events.push(event);
	}

	assert_eq!(events.len(), 2);
	assert_matches!(events.pop().unwrap().map(|x| x.0), Ok(FollowEvent::Stop));
}
