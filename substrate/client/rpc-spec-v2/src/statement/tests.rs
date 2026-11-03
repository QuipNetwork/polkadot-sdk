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

use super::{
	error::{json_rpc_spec::INVALID_PARAM_ERROR, rpc_spec_v2::INVALID_SUBSCRIPTION},
	StatementSpec, StatementSpecApiServer, SubmitOutcome,
};
use codec::Encode;
use jsonrpsee::{core::server::Subscription as RpcSubscription, MethodsError, RpcModule};
use sc_rpc::testing::TokioTestExecutor;
use sc_statement_store::Store;
use sp_core::Bytes;
use sp_statement_store::{
	NewStatementEntry, Statement, StatementAllowance, SubscribeEvent, TopicFilter,
};
use std::{
	sync::Arc,
	time::{Duration, Instant},
};

type Extrinsic = sp_runtime::OpaqueExtrinsic;
type Hash = sp_core::H256;
type Hashing = sp_runtime::traits::BlakeTwo256;
type BlockNumber = u64;
type Header = sp_runtime::generic::Header<BlockNumber, Hashing>;
type Block = sp_runtime::generic::Block<Header, Extrinsic>;
type MockBackend = sc_client_api::in_mem::Backend<Block>;

#[derive(Clone)]
struct MockClient;

impl sc_client_api::StorageProvider<Block, MockBackend> for MockClient {
	fn storage(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
		let allowance = StatementAllowance { max_count: 10_000, max_size: 100_000_000 };
		Ok(Some(sc_client_api::StorageData(allowance.encode())))
	}

	fn storage_hash(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<Hash>> {
		unimplemented!()
	}

	fn storage_keys(
		&self,
		_hash: Hash,
		_prefix: Option<&sc_client_api::StorageKey>,
		_start_key: Option<&sc_client_api::StorageKey>,
	) -> sp_blockchain::Result<
		sc_client_api::backend::KeysIter<
			<MockBackend as sc_client_api::Backend<Block>>::State,
			Block,
		>,
	> {
		unimplemented!()
	}

	fn storage_pairs(
		&self,
		_hash: Hash,
		_prefix: Option<&sc_client_api::StorageKey>,
		_start_key: Option<&sc_client_api::StorageKey>,
	) -> sp_blockchain::Result<
		sc_client_api::backend::PairsIter<
			<MockBackend as sc_client_api::Backend<Block>>::State,
			Block,
		>,
	> {
		unimplemented!()
	}

	fn child_storage(
		&self,
		_hash: Hash,
		_child_info: &sc_client_api::ChildInfo,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
		unimplemented!()
	}

	fn child_storage_keys(
		&self,
		_hash: Hash,
		_child_info: sc_client_api::ChildInfo,
		_prefix: Option<&sc_client_api::StorageKey>,
		_start_key: Option<&sc_client_api::StorageKey>,
	) -> sp_blockchain::Result<
		sc_client_api::backend::KeysIter<
			<MockBackend as sc_client_api::Backend<Block>>::State,
			Block,
		>,
	> {
		unimplemented!()
	}

	fn child_storage_hash(
		&self,
		_hash: Hash,
		_child_info: &sc_client_api::ChildInfo,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<Hash>> {
		unimplemented!()
	}

	fn closest_merkle_value(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::MerkleValue<Hash>>> {
		unimplemented!()
	}

	fn child_closest_merkle_value(
		&self,
		_hash: Hash,
		_child_info: &sc_client_api::ChildInfo,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::MerkleValue<Hash>>> {
		unimplemented!()
	}
}

impl sp_blockchain::HeaderBackend<Block> for MockClient {
	fn header(&self, _hash: Hash) -> sp_blockchain::Result<Option<Header>> {
		unimplemented!()
	}

	fn info(&self) -> sp_blockchain::Info<Block> {
		let best = sp_core::H256::repeat_byte(1);
		sp_blockchain::Info {
			best_hash: best,
			best_number: 0,
			genesis_hash: Default::default(),
			finalized_hash: best,
			finalized_number: 1,
			finalized_state: None,
			number_leaves: 0,
			block_gap: None,
		}
	}

	fn status(&self, _hash: Hash) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
		unimplemented!()
	}

	fn number(&self, _hash: Hash) -> sp_blockchain::Result<Option<BlockNumber>> {
		unimplemented!()
	}

	fn hash(&self, _number: BlockNumber) -> sp_blockchain::Result<Option<Hash>> {
		unimplemented!()
	}
}

fn make_server() -> (RpcModule<StatementSpec<Store>>, tempfile::TempDir) {
	let executor = Arc::new(TokioTestExecutor::default());
	let client = Arc::new(MockClient);
	let temp_dir = tempfile::tempdir().expect("tempdir");
	let mut db_path: std::path::PathBuf = temp_dir.path().into();
	db_path.push("db");

	let store = Store::new::<Block, MockClient, MockBackend>(
		&db_path,
		Default::default(),
		client,
		Arc::new(sc_keystore::LocalKeystore::in_memory()),
		None,
		Box::new((*executor).clone()),
	)
	.expect("store");

	(StatementSpec::new(Arc::new(store), executor).into_rpc(), temp_dir)
}

fn signed_statement(seed: u8, topics: &[[u8; 32]]) -> Statement {
	use sp_core::Pair as _;
	let kp = sp_core::ed25519::Pair::from_string(&format!("//Seed{seed}"), None)
		.expect("valid derivation path");
	let mut s = Statement::new();

	for (i, t) in topics.iter().enumerate() {
		s.set_topic(i, (*t).into());
	}

	s.set_expiry_from_parts(u32::MAX, seed as u32);
	s.sign_ed25519_private(&kp);
	s
}

fn encoded(s: &Statement) -> Bytes {
	Bytes::from(s.encode())
}

async fn subscribe(rpc: &RpcModule<StatementSpec<Store>>) -> RpcSubscription {
	rpc.subscribe_unbounded("statement_unstable_subscribe", jsonrpsee::rpc_params![])
		.await
		.expect("subscribe")
}

fn sub_id_string(sub: &RpcSubscription) -> String {
	match sub.subscription_id() {
		jsonrpsee::types::SubscriptionId::Num(n) => n.to_string(),
		jsonrpsee::types::SubscriptionId::Str(s) => s.to_string(),
	}
}

async fn next_event(sub: &mut RpcSubscription) -> SubscribeEvent {
	let (event, _) = tokio::time::timeout(Duration::from_secs(5), sub.next::<SubscribeEvent>())
		.await
		.expect("subscribe event timed out")
		.expect("subscription stream ended")
		.expect("decode subscribe event");
	event
}

async fn collect_replay(sub: &mut RpcSubscription, filter_id: &str) -> Vec<Bytes> {
	let deadline = Instant::now() + Duration::from_secs(10);
	let mut statements = Vec::new();
	while Instant::now() < deadline {
		match next_event(sub).await {
			SubscribeEvent::ReplayStatements { filter_id: fid, statements: chunk }
				if fid == filter_id =>
			{
				statements.extend(chunk)
			},
			SubscribeEvent::ReplayDone { filter_id: fid } if fid == filter_id => return statements,
			other => panic!("unexpected event before replayDone: {other:?}"),
		}
	}
	panic!("replayDone for {filter_id} not observed in time")
}

#[tokio::test]
async fn submit_then_subscribe_replays_and_then_lives() {
	let (rpc, _store_dir) = make_server();

	let s_pre = signed_statement(1, &[[7u8; 32]]);
	let outcome: SubmitOutcome = rpc
		.call("statement_unstable_submit", (encoded(&s_pre),))
		.await
		.expect("submit pre");
	assert!(matches!(outcome, SubmitOutcome::New));

	let mut sub = subscribe(&rpc).await;
	let sub_id = sub_id_string(&sub);
	let filter_id: super::AddFilterResponse = rpc
		.call("statement_unstable_add_filter", (sub_id, TopicFilter::Any))
		.await
		.expect("add_filter Any");
	let filter_id = match filter_id {
		super::AddFilterResponse::Ok(id) => id,
		super::AddFilterResponse::LimitReached(_) => panic!("unexpected LimitReached"),
	};

	let replayed = collect_replay(&mut sub, &filter_id).await;
	assert_eq!(replayed.len(), 1);

	let s_post = signed_statement(2, &[[7u8; 32]]);
	let outcome: SubmitOutcome = rpc
		.call("statement_unstable_submit", (encoded(&s_post),))
		.await
		.expect("submit post");
	assert!(matches!(outcome, SubmitOutcome::New));

	match next_event(&mut sub).await {
		SubscribeEvent::NewStatements { statements } => {
			assert_eq!(statements.len(), 1);
			let NewStatementEntry { filter_ids, .. } = &statements[0];
			assert_eq!(filter_ids, &vec![filter_id.clone()]);
		},
		other => panic!("expected NewStatements, got {other:?}"),
	}
}

#[tokio::test]
async fn submit_rejects_trailing_bytes_after_a_statement() {
	let (rpc, _store_dir) = make_server();
	let mut invalid_encoding = signed_statement(1, &[]).encode();
	invalid_encoding.push(0);

	let err = rpc
		.call::<_, SubmitOutcome>("statement_unstable_submit", (Bytes::from(invalid_encoding),))
		.await
		.expect_err("trailing bytes must make the SCALE encoding invalid");
	let object = match err {
		MethodsError::JsonRpc(error) => error,
		other => panic!("expected ErrorObject, got {other:?}"),
	};
	assert_eq!(object.code(), INVALID_PARAM_ERROR);
}

#[tokio::test]
async fn add_filter_rejects_match_any_topic_filter() {
	use sp_runtime::BoundedVec;

	let (rpc, _store_dir) = make_server();
	let sub = subscribe(&rpc).await;
	let sub_id = sub_id_string(&sub);

	let topic = sp_statement_store::Topic::from([1u8; 32]);
	let filter = TopicFilter::MatchAny(BoundedVec::truncate_from(vec![topic]));
	let err = rpc
		.call::<_, super::AddFilterResponse>("statement_unstable_add_filter", (sub_id, filter))
		.await
		.expect_err("matchAny must be rejected");

	let s = err.to_string();
	assert!(s.contains("matchAny"));
	drop(sub);
}

#[tokio::test]
async fn add_filter_rejects_empty_match_all_topic_filter() {
	let (rpc, _store_dir) = make_server();
	let sub = subscribe(&rpc).await;
	let sub_id = sub_id_string(&sub);
	let filter = TopicFilter::MatchAll(Default::default());

	let err = rpc
		.call::<_, super::AddFilterResponse>("statement_unstable_add_filter", (sub_id, filter))
		.await
		.expect_err("empty matchAll must be rejected");
	let object = match err {
		MethodsError::JsonRpc(error) => error,
		other => panic!("expected ErrorObject, got {other:?}"),
	};
	assert_eq!(object.code(), INVALID_PARAM_ERROR);
	drop(sub);
}

#[tokio::test]
async fn remove_filter_frees_rpc_filter_capacity() {
	let (rpc, _store_dir) = make_server();
	let sub = subscribe(&rpc).await;
	let sub_id = sub_id_string(&sub);
	let mut filter_ids = Vec::new();

	for i in 0..sc_statement_store::MAX_FILTERS_PER_SUBSCRIPTION {
		let resp: super::AddFilterResponse = rpc
			.call("statement_unstable_add_filter", (sub_id.clone(), TopicFilter::Any))
			.await
			.unwrap_or_else(|e| panic!("filter {i} should be accepted: {e:?}"));
		let filter_id = match resp {
			super::AddFilterResponse::Ok(id) => id,
			super::AddFilterResponse::LimitReached(_) => {
				panic!("filter {i} unexpectedly reached the cap")
			},
		};
		filter_ids.push(filter_id);
	}

	let resp: super::AddFilterResponse = rpc
		.call("statement_unstable_add_filter", (sub_id.clone(), TopicFilter::Any))
		.await
		.expect("filter beyond cap returns a successful RPC response");
	assert_eq!(resp, super::AddFilterResponse::limit_reached());

	let _: () = rpc
		.call("statement_unstable_remove_filter", (sub_id.clone(), filter_ids[0].clone()))
		.await
		.expect("remove_filter frees capacity");

	let resp: super::AddFilterResponse = rpc
		.call("statement_unstable_add_filter", (sub_id, TopicFilter::Any))
		.await
		.expect("replacement filter should be accepted");
	assert!(
		matches!(resp, super::AddFilterResponse::Ok(_)),
		"replacement filter should return Ok, got {resp:?}"
	);

	drop(sub);
}

#[tokio::test]
async fn add_filter_for_unknown_subscription_yields_invalid_subscription() {
	let (rpc, _store_dir) = make_server();
	let err = rpc
		.call::<_, super::AddFilterResponse>(
			"statement_unstable_add_filter",
			("does-not-exist".to_string(), TopicFilter::Any),
		)
		.await
		.expect_err("unknown subscription must error");

	let object = match err {
		MethodsError::JsonRpc(e) => e,
		other => panic!("expected ErrorObject, got {other:?}"),
	};
	assert_eq!(object.code(), INVALID_SUBSCRIPTION);
}

#[tokio::test]
async fn remove_filter_is_silent_for_unknown_subscription_and_filter() {
	let (rpc, _store_dir) = make_server();

	let _: () = rpc
		.call("statement_unstable_remove_filter", ("does-not-exist".to_string(), "0".to_string()))
		.await
		.expect("remove_filter on unknown sub is no-op");

	let sub = subscribe(&rpc).await;
	let sub_id = sub_id_string(&sub);
	let _: () = rpc
		.call("statement_unstable_remove_filter", (sub_id, "999".to_string()))
		.await
		.expect("remove_filter on unknown filter is no-op");
	drop(sub);
}
