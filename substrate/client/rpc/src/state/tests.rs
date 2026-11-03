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

use self::error::Error;
use super::*;
use crate::testing::{allow_unsafe, deny_unsafe, test_executor, timeout_secs};
use assert_matches::assert_matches;
use futures::executor;
use jsonrpsee::{core::EmptyServerParams as EmptyParams, MethodsError as RpcError};
use sc_block_builder::BlockBuilderBuilder;
use sp_consensus::BlockOrigin;
use sp_core::{hash::H256, storage::ChildInfo};
use std::sync::Arc;
use substrate_test_runtime_client::{
	prelude::*,
	runtime::{ExtrinsicBuilder, Transfer},
};

const STORAGE_KEY: &[u8] = b"child";

fn prefixed_storage_key() -> PrefixedStorageKey {
	let child_info = ChildInfo::new_default(STORAGE_KEY);
	child_info.prefixed_storage_key()
}

#[tokio::test]
async fn should_return_storage() {
	const KEY: &[u8] = b":mock";
	const VALUE: &[u8] = b"hello world";
	const CHILD_VALUE: &[u8] = b"hello world !";

	let child_info = ChildInfo::new_default(STORAGE_KEY);
	let client = TestClientBuilder::new()
		.add_extra_storage(KEY.to_vec(), VALUE.to_vec())
		.add_extra_child_storage(&child_info, KEY.to_vec(), CHILD_VALUE.to_vec())
		// similar to a map with two keys
		.add_extra_storage(b":map:acc1".to_vec(), vec![1, 2])
		.add_extra_storage(b":map:acc2".to_vec(), vec![1, 2, 3])
		.build();
	let genesis_hash = client.genesis_hash();
	let (client, child) = new_full(Arc::new(client), test_executor(), None);
	let key = StorageKey(KEY.to_vec());
	let ext = allow_unsafe();

	assert_eq!(
		client
			.storage(key.clone(), Some(genesis_hash).into())
			.map(|x| x.map(|x| x.0.len()))
			.unwrap()
			.unwrap() as usize,
		VALUE.len(),
	);
	assert_matches!(
		client.storage_hash(key.clone(), Some(genesis_hash).into()).map(|x| x.is_some()),
		Ok(true)
	);
	assert_eq!(
		client.storage_size(&ext, key.clone(), None).await.unwrap().unwrap() as usize,
		VALUE.len(),
	);
	assert_eq!(
		client
			.storage_size(&ext, StorageKey(b":map".to_vec()), None)
			.await
			.unwrap()
			.unwrap() as usize,
		2 + 3,
	);
	assert_eq!(
		child
			.storage(prefixed_storage_key(), key, Some(genesis_hash).into())
			.map(|x| x.map(|x| x.0.len()))
			.unwrap()
			.unwrap() as usize,
		CHILD_VALUE.len(),
	);
}

#[tokio::test]
async fn should_return_storage_entries() {
	const KEY1: &[u8] = b":mock";
	const KEY2: &[u8] = b":turtle";
	const VALUE: &[u8] = b"hello world";
	const CHILD_VALUE1: &[u8] = b"hello world !";
	const CHILD_VALUE2: &[u8] = b"hello world    !";

	let child_info = ChildInfo::new_default(STORAGE_KEY);
	let client = TestClientBuilder::new()
		.add_extra_storage(KEY1.to_vec(), VALUE.to_vec())
		.add_extra_child_storage(&child_info, KEY1.to_vec(), CHILD_VALUE1.to_vec())
		.add_extra_child_storage(&child_info, KEY2.to_vec(), CHILD_VALUE2.to_vec())
		.build();
	let genesis_hash = client.genesis_hash();
	let (_client, child) = new_full(Arc::new(client), test_executor(), None);

	let keys = &[StorageKey(KEY1.to_vec()), StorageKey(KEY2.to_vec())];
	assert_eq!(
		child
			.storage_entries(prefixed_storage_key(), keys.to_vec(), Some(genesis_hash).into())
			.map(|x| x.into_iter().map(|x| x.map(|x| x.0.len()).unwrap()).sum::<usize>())
			.unwrap(),
		CHILD_VALUE1.len() + CHILD_VALUE2.len()
	);

	// should fail if not all keys exist.
	let mut failing_keys = vec![StorageKey(b":soup".to_vec())];
	failing_keys.extend_from_slice(keys);
	assert_matches!(
		child
			.storage_entries(prefixed_storage_key(), failing_keys, Some(genesis_hash).into())
			.map(|x| x.iter().all(|x| x.is_some())),
		Ok(false)
	);
}

#[tokio::test]
async fn should_return_child_storage() {
	let child_info = ChildInfo::new_default(STORAGE_KEY);
	let client = Arc::new(
		substrate_test_runtime_client::TestClientBuilder::new()
			.add_child_storage(&child_info, "key", vec![42_u8])
			.build(),
	);
	let genesis_hash = client.genesis_hash();
	let (_client, child) = new_full(client, test_executor(), None);
	let child_key = prefixed_storage_key();
	let key = StorageKey(b"key".to_vec());

	assert_matches!(
		child.storage(
			child_key.clone(),
			key.clone(),
			Some(genesis_hash).into(),
		),
		Ok(Some(StorageData(ref d))) if d[0] == 42 && d.len() == 1
	);
	assert_matches!(
		child
			.storage_hash(child_key.clone(), key.clone(), Some(genesis_hash).into(),)
			.map(|x| x.is_some()),
		Ok(true)
	);
	assert_matches!(child.storage_size(child_key.clone(), key.clone(), None), Ok(Some(1)));
}

#[tokio::test]
async fn should_return_child_storage_entries() {
	let child_info = ChildInfo::new_default(STORAGE_KEY);
	let client = Arc::new(
		substrate_test_runtime_client::TestClientBuilder::new()
			.add_child_storage(&child_info, "key1", vec![42_u8])
			.add_child_storage(&child_info, "key2", vec![43_u8, 44])
			.build(),
	);
	let genesis_hash = client.genesis_hash();
	let (_client, child) = new_full(client, test_executor(), None);
	let child_key = prefixed_storage_key();
	let keys = vec![StorageKey(b"key1".to_vec()), StorageKey(b"key2".to_vec())];

	let res = child
		.storage_entries(child_key.clone(), keys.clone(), Some(genesis_hash).into())
		.unwrap();

	assert_matches!(
		res[0],
		Some(StorageData(ref d))
			if d[0] == 42 && d.len() == 1
	);
	assert_matches!(
		res[1],
		Some(StorageData(ref d))
			if d[0] == 43 && d[1] == 44 && d.len() == 2
	);
	assert_matches!(
		child
			.storage_hash(child_key.clone(), keys[0].clone(), Some(genesis_hash).into())
			.map(|x| x.is_some()),
		Ok(true)
	);
	assert_matches!(child.storage_size(child_key.clone(), keys[0].clone(), None), Ok(Some(1)));
}

#[tokio::test]
async fn should_call_contract() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let genesis_hash = client.genesis_hash();
	let (client, _child) = new_full(client, test_executor(), None);

	assert_matches!(
		client.call("balanceOf".into(), Bytes(vec![1, 2, 3]), Some(genesis_hash).into()),
		Err(Error::Client(_))
	)
}

#[tokio::test]
async fn call_recorded_is_unsupported_without_recorder() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let block_hash = client.chain_info().best_hash;
	let (state, _child) = new_full(client, test_executor(), None);

	let err = state
		.call_recorded(&allow_unsafe(), "Core_version".into(), Bytes(vec![]), block_hash)
		.unwrap_err();

	assert_matches!(err, Error::CallRecordedUnsupported);
	let object = jsonrpsee::types::ErrorObjectOwned::from(err);
	assert_eq!(object.code(), error::CALL_RECORDED_UNSUPPORTED_ERROR_CODE);
}

#[tokio::test]
async fn call_recorded_denied_when_unsafe() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let block_hash = client.chain_info().best_hash;
	let (state, _child) = new_full(client, test_executor(), None);

	let err = state
		.call_recorded(&deny_unsafe(), "Core_version".into(), Bytes(vec![]), block_hash)
		.unwrap_err();

	assert_matches!(err, Error::CallRecordedDenied);
	let object = jsonrpsee::types::ErrorObjectOwned::from(err);
	assert_eq!(object.code(), error::CALL_RECORDED_DENIED_ERROR_CODE);
}

#[tokio::test]
async fn call_recorded_reports_dispatch_failures_as_client_errors() {
	struct FailingExecuteBlock;
	impl<Block: BlockT> sc_tracing::block::TracingExecuteBlock<Block> for FailingExecuteBlock {
		fn execute_block(&self, _: Block::Hash, _: Block) -> sp_blockchain::Result<()> {
			unreachable!("not exercised by this test")
		}

		fn call_recorded(
			&self,
			_: Block::Hash,
			_: &str,
			_: &[u8],
		) -> sp_blockchain::Result<Vec<u8>> {
			Err(sp_blockchain::Error::Backend("dispatch failed".into()))
		}
	}

	let client = Arc::new(substrate_test_runtime_client::new());
	let block_hash = client.chain_info().best_hash;
	let (state, _child) = new_full(client, test_executor(), Some(Arc::new(FailingExecuteBlock)));

	assert_matches!(
		state.call_recorded(&allow_unsafe(), "Core_version".into(), Bytes(vec![]), block_hash),
		Err(Error::Client(e)) if e.to_string().contains("dispatch failed")
	);
}

#[tokio::test]
async fn should_notify_about_storage_changes() {
	let mut sub = {
		let client = Arc::new(substrate_test_runtime_client::new());
		let (api, _child) = new_full(client.clone(), test_executor(), None);
		let mut api_rpc = api.into_rpc();
		api_rpc.extensions_mut().insert(DenyUnsafe::No);

		let sub = api_rpc
			.subscribe_unbounded("state_subscribeStorage", EmptyParams::new())
			.await
			.unwrap();

		// Cause a change:
		let mut builder = BlockBuilderBuilder::new(&*client)
			.on_parent_block(client.chain_info().best_hash)
			.with_parent_block_number(client.chain_info().best_number)
			.build()
			.unwrap();
		builder
			.push_transfer(Transfer {
				from: Sr25519Keyring::Alice.into(),
				to: Sr25519Keyring::Ferdie.into(),
				amount: 42,
				nonce: 0,
			})
			.unwrap();
		let block = builder.build().unwrap().block;
		client.import(BlockOrigin::Own, block).await.unwrap();

		sub
	};

	// We should get a message back on our subscription about the storage change:
	// NOTE: previous versions of the subscription code used to return an empty value for the
	// "initial" storage change here
	assert_matches!(timeout_secs(1, sub.next::<StorageChangeSet<H256>>()).await, Ok(Some(_)));
}

#[tokio::test]
async fn should_send_initial_storage_changes_and_notifications() {
	let mut sub = {
		let client = Arc::new(substrate_test_runtime_client::new());
		let (api, _child) = new_full(client.clone(), test_executor(), None);

		let alice_balance_key = [
			sp_crypto_hashing::twox_128(b"System"),
			sp_crypto_hashing::twox_128(b"Account"),
			sp_crypto_hashing::blake2_128(&Sr25519Keyring::Alice.public()),
		]
		.concat()
		.iter()
		.chain(Sr25519Keyring::Alice.public().0.iter())
		.cloned()
		.collect::<Vec<u8>>();

		let mut api_rpc = api.into_rpc();
		api_rpc.extensions_mut().insert(DenyUnsafe::No);

		let sub = api_rpc
			.subscribe_unbounded(
				"state_subscribeStorage",
				[[StorageKey(alice_balance_key.to_vec())]],
			)
			.await
			.unwrap();

		let mut builder = BlockBuilderBuilder::new(&*client)
			.on_parent_block(client.chain_info().best_hash)
			.with_parent_block_number(client.chain_info().best_number)
			.build()
			.unwrap();
		builder
			.push_transfer(Transfer {
				from: Sr25519Keyring::Alice.into(),
				to: Sr25519Keyring::Ferdie.into(),
				amount: 42,
				nonce: 0,
			})
			.unwrap();
		let block = builder.build().unwrap().block;
		client.import(BlockOrigin::Own, block).await.unwrap();

		sub
	};

	assert_matches!(timeout_secs(1, sub.next::<StorageChangeSet<H256>>()).await, Ok(Some(_)));
	assert_matches!(timeout_secs(1, sub.next::<StorageChangeSet<H256>>()).await, Ok(Some(_)));
}

#[tokio::test]
async fn should_query_storage() {
	async fn run_tests(client: Arc<TestClient>) {
		let (api, _child) = new_full(client.clone(), test_executor(), None);

		let add_block = |index| {
			let mut builder = BlockBuilderBuilder::new(&*client)
				.on_parent_block(client.chain_info().best_hash)
				.with_parent_block_number(client.chain_info().best_number)
				.build()
				.unwrap();
			// fake change: None -> None -> None
			builder
				.push(ExtrinsicBuilder::new_storage_change(vec![1], None).build())
				.unwrap();
			// fake change: None -> Some(value) -> Some(value)
			builder
				.push(ExtrinsicBuilder::new_storage_change(vec![2], Some(vec![2])).build())
				.unwrap();
			// actual change: None -> Some(value) -> None
			builder
				.push(
					ExtrinsicBuilder::new_storage_change(
						vec![3],
						if index == 0 { Some(vec![3]) } else { None },
					)
					.build(),
				)
				.unwrap();
			// actual change: None -> Some(value)
			builder
				.push(
					ExtrinsicBuilder::new_storage_change(
						vec![4],
						if index == 0 { None } else { Some(vec![4]) },
					)
					.build(),
				)
				.unwrap();
			// actual change: Some(value1) -> Some(value2)
			builder
				.push(
					ExtrinsicBuilder::new_storage_change(vec![5], Some(vec![index as u8])).build(),
				)
				.unwrap();
			let block = builder.build().unwrap().block;
			let hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			hash
		};
		let block1_hash = add_block(0);
		let block2_hash = add_block(1);
		let genesis_hash = client.genesis_hash();

		let mut expected = vec![
			StorageChangeSet {
				block: genesis_hash,
				changes: vec![
					(StorageKey(vec![1]), None),
					(StorageKey(vec![2]), None),
					(StorageKey(vec![3]), None),
					(StorageKey(vec![4]), None),
					(StorageKey(vec![5]), None),
				],
			},
			StorageChangeSet {
				block: block1_hash,
				changes: vec![
					(StorageKey(vec![2]), Some(StorageData(vec![2]))),
					(StorageKey(vec![3]), Some(StorageData(vec![3]))),
					(StorageKey(vec![5]), Some(StorageData(vec![0]))),
				],
			},
		];

		let ext = allow_unsafe();

		// Query changes only up to block1
		let keys = (1..6).map(|k| StorageKey(vec![k])).collect::<Vec<_>>();
		let result = api.query_storage(&ext, keys.clone(), genesis_hash, Some(block1_hash).into());

		assert_eq!(result.unwrap(), expected);

		// Query all changes
		let result = api.query_storage(&ext, keys.clone(), genesis_hash, None.into());

		expected.push(StorageChangeSet {
			block: block2_hash,
			changes: vec![
				(StorageKey(vec![3]), None),
				(StorageKey(vec![4]), Some(StorageData(vec![4]))),
				(StorageKey(vec![5]), Some(StorageData(vec![1]))),
			],
		});
		assert_eq!(result.unwrap(), expected);

		// Query changes up to block2.
		let result = api.query_storage(&ext, keys.clone(), genesis_hash, Some(block2_hash));

		assert_eq!(result.unwrap(), expected);

		// Inverted range.
		assert_matches!(
			api.query_storage(&ext, keys.clone(), block1_hash, Some(genesis_hash)),
			Err(Error::InvalidBlockRange { from, to, details }) if from == format!("1 ({:?})", block1_hash) && to == format!("0 ({:?})", genesis_hash) && details == "from number > to number".to_owned()
		);

		let random_hash1 = H256::random();
		let random_hash2 = H256::random();

		// Invalid second hash.
		assert_matches!(
			api.query_storage(&ext, keys.clone(), genesis_hash, Some(random_hash1)),
			Err(Error::InvalidBlockRange { from, to, details }) if from == format!("{:?}", genesis_hash) && to == format!("{:?}", Some(random_hash1)) && details == format!(
				"UnknownBlock: Header was not found in the database: {:?}",
				random_hash1
			)
		);

		// Invalid first hash with Some other hash.
		assert_matches!(
			api.query_storage(&ext, keys.clone(), random_hash1, Some(genesis_hash)),
			Err(Error::InvalidBlockRange { from, to, details }) if from == format!("{:?}", random_hash1) && to == format!("{:?}", Some(genesis_hash)) && details == format!(
				"UnknownBlock: Header was not found in the database: {:?}",
				random_hash1
			)
		);

		// Invalid first hash with None.
		assert_matches!(
			api.query_storage(&ext, keys.clone(), random_hash1, None),
			Err(Error::InvalidBlockRange { from, to, details }) if from == format!("{:?}", random_hash1) && to == format!("{:?}", Some(block2_hash)) && details == format!(
				"UnknownBlock: Header was not found in the database: {:?}",
				random_hash1
			)
		);

		// Both hashes invalid.
		assert_matches!(
			api.query_storage(&ext, keys.clone(), random_hash1, Some(random_hash2)),
			Err(Error::InvalidBlockRange { from, to, details }) if from == format!("{:?}", random_hash1) && to == format!("{:?}", Some(random_hash2)) && details == format!(
				"UnknownBlock: Header was not found in the database: {:?}",
				random_hash1
			)
		);

		// single block range
		let result = api.query_storage_at(keys.clone(), Some(block1_hash));

		assert_eq!(
			result.unwrap(),
			vec![StorageChangeSet {
				block: block1_hash,
				changes: vec![
					(StorageKey(vec![1_u8]), None),
					(StorageKey(vec![2_u8]), Some(StorageData(vec![2_u8]))),
					(StorageKey(vec![3_u8]), Some(StorageData(vec![3_u8]))),
					(StorageKey(vec![4_u8]), None),
					(StorageKey(vec![5_u8]), Some(StorageData(vec![0_u8]))),
				]
			}]
		);
	}

	run_tests(Arc::new(substrate_test_runtime_client::new())).await;
	run_tests(Arc::new(TestClientBuilder::new().build())).await;
}

#[tokio::test]
async fn should_return_runtime_version() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let (api, _child) = new_full(client.clone(), test_executor(), None);

	// it is basically json-encoded substrate_test_runtime_client::runtime::VERSION
	let result = "{\"specName\":\"test\",\"implName\":\"parity-test\",\"authoringVersion\":1,\
		\"specVersion\":2,\"implVersion\":2,\"apis\":[[\"0xdf6acb689907609b\",5],\
		[\"0x37e397fc7c91f5e4\",2],[\"0xd2bc9897eed08f15\",3],[\"0x40fe3ad401f8959a\",6],\
		[\"0xbc9d89904f5b923f\",1],[\"0xc6e9a76309f39b09\",2],[\"0xdd718d5cc53262d4\",1],\
		[\"0xcbca25e39f142387\",2],[\"0xf78b278be53f454c\",2],[\"0xab3c0572291feb8b\",2],\
		[\"0xed99c5acb25eedf5\",3],[\"0xfbc577b9d747efd6\",1]],\"transactionVersion\":1,\"systemVersion\":1,\
		\"stateVersion\":1}";

	let runtime_version = api.runtime_version(None.into()).unwrap();
	let serialized = serde_json::to_string(&runtime_version).unwrap();
	pretty_assertions::assert_eq!(serialized, result);

	let deserialized: RuntimeVersion = serde_json::from_str(result).unwrap();
	assert_eq!(deserialized, runtime_version);
}

#[tokio::test]
async fn should_notify_on_runtime_version_initially() {
	let mut sub = {
		let client = Arc::new(substrate_test_runtime_client::new());
		let (api, _child) = new_full(client, test_executor(), None);
		let mut api_rpc = api.into_rpc();
		api_rpc.extensions_mut().insert(DenyUnsafe::No);

		let sub = api_rpc
			.subscribe_unbounded("state_subscribeRuntimeVersion", EmptyParams::new())
			.await
			.unwrap();

		sub
	};

	// assert initial version sent.
	assert_matches!(timeout_secs(10, sub.next::<RuntimeVersion>()).await, Ok(Some(_)));
}

#[test]
fn should_deserialize_storage_key() {
	let k = "\"0x7f864e18e3dd8b58386310d2fe0919eef27c6e558564b7f67f22d99d20f587b\"";
	let k: StorageKey = serde_json::from_str(k).unwrap();

	assert_eq!(k.0.len(), 32);
}

#[tokio::test]
async fn wildcard_storage_subscriptions_are_rpc_unsafe() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let (api, _child) = new_full(client, test_executor(), None);
	let mut api_rpc = api.into_rpc();
	api_rpc.extensions_mut().insert(DenyUnsafe::Yes);

	let err = api_rpc.subscribe_unbounded("state_subscribeStorage", EmptyParams::new()).await;
	assert_matches!(err, Err(RpcError::JsonRpc(e)) if e.message() == "RPC call is unsafe to be called externally");
}

#[tokio::test]
async fn concrete_storage_subscriptions_are_rpc_safe() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let (api, _child) = new_full(client, test_executor(), None);
	let mut api_rpc = api.into_rpc();
	api_rpc.extensions_mut().insert(DenyUnsafe::Yes);

	let key = StorageKey(STORAGE_KEY.to_vec());
	let sub = api_rpc.subscribe_unbounded("state_subscribeStorage", [[key]]).await;

	assert!(sub.is_ok());
}
