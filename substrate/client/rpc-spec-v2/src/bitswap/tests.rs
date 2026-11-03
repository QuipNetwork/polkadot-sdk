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
use jsonrpsee::{core::client::ClientT, rpc_params, server::ServerBuilder};
use sp_core::H256;
use sp_runtime::traits::Block as BlockT;
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

fn assert_error_code(err: &jsonrpsee::core::ClientError, expected_code: i32) {
	match err {
		jsonrpsee::core::ClientError::Call(obj) => {
			assert_eq!(obj.code(), expected_code, "Unexpected error code: {obj:?}");
		},
		other => panic!("Expected CallError, got: {other:?}"),
	}
}

type Block = substrate_test_runtime::Block;

/// Mock BlockBackend that only implements `indexed_transaction`.
struct MockClient {
	transactions: Mutex<HashMap<H256, Vec<u8>>>,
}

impl MockClient {
	fn new() -> Self {
		Self { transactions: Mutex::new(HashMap::new()) }
	}

	fn insert_transaction(&self, hash: H256, data: Vec<u8>) {
		self.transactions.lock().unwrap().insert(hash, data);
	}
}

impl sc_client_api::BlockBackend<Block> for MockClient {
	fn block_body(
		&self,
		_hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<Vec<<Block as BlockT>::Extrinsic>>> {
		unimplemented!()
	}

	fn block(
		&self,
		_hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<sp_runtime::generic::SignedBlock<Block>>> {
		unimplemented!()
	}

	fn block_status(
		&self,
		_hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<sp_consensus::BlockStatus> {
		unimplemented!()
	}

	fn justifications(
		&self,
		_hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<sp_runtime::Justifications>> {
		unimplemented!()
	}

	fn block_hash(
		&self,
		_number: sp_runtime::traits::NumberFor<Block>,
	) -> sp_blockchain::Result<Option<<Block as BlockT>::Hash>> {
		unimplemented!()
	}

	fn indexed_transaction(&self, hash: H256) -> sp_blockchain::Result<Option<Vec<u8>>> {
		Ok(self.transactions.lock().unwrap().get(&hash).cloned())
	}

	fn has_indexed_transaction(&self, hash: H256) -> sp_blockchain::Result<bool> {
		Ok(self.transactions.lock().unwrap().contains_key(&hash))
	}

	fn block_indexed_body(
		&self,
		_hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<Vec<Vec<u8>>>> {
		unimplemented!()
	}

	fn block_indexed_hashes(
		&self,
		_hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<Vec<H256>>> {
		unimplemented!()
	}

	fn requires_full_sync(&self) -> bool {
		false
	}
}

/// Mock SyncOracle with configurable `is_major_syncing` flag.
struct MockSyncOracle {
	major_syncing: bool,
}

impl MockSyncOracle {
	fn new(major_syncing: bool) -> Self {
		Self { major_syncing }
	}
}

impl sp_consensus::SyncOracle for MockSyncOracle {
	fn is_major_syncing(&self) -> bool {
		self.major_syncing
	}

	fn is_offline(&self) -> bool {
		false
	}
}

// Standard multihash codes.
// See <https://github.com/multiformats/multicodec/blob/master/table.csv>
const SHA2_256: u64 = 0x12;
const BLAKE2B_256: u64 = 0xb220;
const KECCAK_256: u64 = 0x1b;

/// Create a CIDv1 string from a 32-byte hash digest.
fn make_cid_v1(code: u64, digest: &[u8; 32]) -> String {
	let mh = cid::multihash::Multihash::<64>::wrap(code, digest)
		.expect("32 bytes fits in Multihash<32>");
	// codec 0x70 = dag-pb
	let c = cid::Cid::new_v1(0x70, mh);
	c.to_string()
}

/// Create a CIDv0 string.
fn make_cid_v0() -> String {
	// CIDv0 is a bare base58btc-encoded multihash (SHA2-256)
	let digest = [0u8; 32];
	let mh = cid::multihash::Multihash::<64>::wrap(SHA2_256, &digest)
		.expect("32 bytes fits in Multihash<32>");
	let c = cid::Cid::new_v0(mh).expect("SHA2-256 is valid for CIDv0");
	c.to_string()
}

/// Create a CIDv1 with a non-32-byte digest.
fn make_cid_v1_short_digest() -> String {
	let digest = [0u8; 16];
	let mh = cid::multihash::Multihash::<64>::wrap(BLAKE2B_256, &digest)
		.expect("16 bytes fits in Multihash<64>");
	let c = cid::Cid::new_v1(0x70, mh);
	c.to_string()
}

/// Create a CIDv1 string with unsupported multihash code.
fn make_cid_v1_unsupported_hash_function() -> String {
	let digest = [0u8; 32];
	// 0x16 = sha3-256, not in the supported set.
	let mh = cid::multihash::Multihash::<64>::wrap(0x16, &digest)
		.expect("32 bytes fits in Multihash<64>");
	// codec 0x70 = dag-pb
	let c = cid::Cid::new_v1(0x70, mh);
	c.to_string()
}

async fn setup(
	major_syncing: bool,
) -> (jsonrpsee::ws_client::WsClient, jsonrpsee::server::ServerHandle, Arc<MockClient>) {
	let client = Arc::new(MockClient::new());
	let sync_oracle = Arc::new(MockSyncOracle::new(major_syncing));

	let bitswap = Bitswap::<Block, _>::new(client.clone(), sync_oracle);

	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let addr = server.local_addr().unwrap();
	let handle = server.start(bitswap.into_rpc());

	let url = format!("ws://{}", addr);
	let ws_client = jsonrpsee::ws_client::WsClientBuilder::default().build(&url).await.unwrap();

	(ws_client, handle, client)
}

#[tokio::test]
async fn valid_cid_data_found_sha256() {
	let (ws_client, _handle, mock_client) = setup(false).await;

	let data = vec![1u8, 2, 3, 4, 5];
	let digest = sp_crypto_hashing::sha2_256(&data);
	mock_client.insert_transaction(H256::from(digest), data.clone());

	let cid_str = make_cid_v1(SHA2_256, &digest);
	let result: String = ws_client.request("bitswap_v1_get", rpc_params![cid_str]).await.unwrap();

	assert_eq!(result, crate::hex_string(&data));
}

#[tokio::test]
async fn valid_cid_data_found_blake2b() {
	let (ws_client, _handle, mock_client) = setup(false).await;

	let data = vec![1u8, 2, 3, 4, 5];
	let digest = sp_crypto_hashing::blake2_256(&data);
	mock_client.insert_transaction(H256::from(digest), data.clone());

	let cid_str = make_cid_v1(BLAKE2B_256, &digest);
	let result: String = ws_client.request("bitswap_v1_get", rpc_params![cid_str]).await.unwrap();

	assert_eq!(result, crate::hex_string(&data));
}

#[tokio::test]
async fn valid_cid_data_found_keccak256() {
	let (ws_client, _handle, mock_client) = setup(false).await;

	let data = vec![1u8, 2, 3, 4, 5];
	let digest = sp_crypto_hashing::keccak_256(&data);
	mock_client.insert_transaction(H256::from(digest), data.clone());

	let cid_str = make_cid_v1(KECCAK_256, &digest);
	let result: String = ws_client.request("bitswap_v1_get", rpc_params![cid_str]).await.unwrap();

	assert_eq!(result, crate::hex_string(&data));
}

#[tokio::test]
async fn valid_cid_not_found_not_syncing() {
	let (ws_client, _handle, _mock_client) = setup(false).await;

	let digest = [42u8; 32];
	let cid_str = make_cid_v1(BLAKE2B_256, &digest);
	let err = ws_client
		.request::<String, _>("bitswap_v1_get", rpc_params![cid_str])
		.await
		.unwrap_err();

	assert_error_code(&err, -32810);
}

#[tokio::test]
async fn valid_cid_not_found_major_syncing() {
	let (ws_client, _handle, _mock_client) = setup(true).await;

	let digest = [42u8; 32];
	let cid_str = make_cid_v1(BLAKE2B_256, &digest);
	let err = ws_client
		.request::<String, _>("bitswap_v1_get", rpc_params![cid_str])
		.await
		.unwrap_err();

	assert_error_code(&err, -32812);
}

#[tokio::test]
async fn invalid_cid_string() {
	let (ws_client, _handle, _mock_client) = setup(false).await;

	let err = ws_client
		.request::<String, _>("bitswap_v1_get", rpc_params!["not-a-valid-cid"])
		.await
		.unwrap_err();

	assert_error_code(&err, -32602);
}

#[tokio::test]
async fn cid_v0_rejected() {
	let (ws_client, _handle, _mock_client) = setup(false).await;

	let cid_str = make_cid_v0();
	let err = ws_client
		.request::<String, _>("bitswap_v1_get", rpc_params![cid_str])
		.await
		.unwrap_err();

	assert_error_code(&err, -32602);
}

#[tokio::test]
async fn cid_v1_unsupported_hash_function_rejected() {
	let (ws_client, _handle, _mock_client) = setup(false).await;

	let cid_str = make_cid_v1_unsupported_hash_function();
	let err = ws_client
		.request::<String, _>("bitswap_v1_get", rpc_params![cid_str])
		.await
		.unwrap_err();

	assert_error_code(&err, -32602);
}

#[tokio::test]
async fn cid_v1_non_32_byte_digest_rejected() {
	let (ws_client, _handle, _mock_client) = setup(false).await;

	let cid_str = make_cid_v1_short_digest();
	let err = ws_client
		.request::<String, _>("bitswap_v1_get", rpc_params![cid_str])
		.await
		.unwrap_err();

	assert_error_code(&err, -32602);
}
