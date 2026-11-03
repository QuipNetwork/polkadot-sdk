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

use jsonrpsee::ConnectionId;
use parking_lot::RwLock;
use sc_rpc::utils::Subscription;
use sc_statement_store::{MultiFilterSubscriptionEvent, SubscriptionHandle};
use sp_statement_store::{FilterId, NewStatementEntry, SubscribeEvent};
use std::{collections::HashMap, sync::Arc};

type SubscriptionStateRef = SubscriptionHandle;
type SubscriptionRegistry =
	Arc<RwLock<HashMap<ConnectionId, HashMap<String, SubscriptionStateRef>>>>;

/// Long-lived registry owned by the RPC server
#[derive(Clone, Default)]
pub struct StatementSubscriptions {
	registry: SubscriptionRegistry,
}

impl StatementSubscriptions {
	pub fn new() -> Self {
		Self { registry: Arc::new(RwLock::new(HashMap::new())) }
	}

	/// Registers a subscription owned by the connection
	pub fn register(
		&self,
		conn_id: ConnectionId,
		sub_id: String,
		handle: SubscriptionHandle,
	) -> Option<SubscriptionEntry> {
		let mut registry = self.registry.write();
		let connection = registry.entry(conn_id).or_default();
		if connection.contains_key(&sub_id) {
			return None;
		}

		connection.insert(sub_id.clone(), handle);
		Some(SubscriptionEntry { conn_id, sub_id, registry: self.registry.clone() })
	}

	/// Gets a subscription owned by the connection
	pub fn get(&self, conn_id: ConnectionId, sub_id: &str) -> Option<SubscriptionStateRef> {
		self.registry
			.read()
			.get(&conn_id)
			.and_then(|connection| connection.get(sub_id).cloned())
	}
}

/// Registered subscription entry
pub struct SubscriptionEntry {
	conn_id: ConnectionId,
	sub_id: String,
	registry: SubscriptionRegistry,
}

impl Drop for SubscriptionEntry {
	fn drop(&mut self) {
		let mut registry = self.registry.write();
		if let Some(connection) = registry.get_mut(&self.conn_id) {
			connection.remove(&self.sub_id);
			if connection.is_empty() {
				registry.remove(&self.conn_id);
			}
		}
	}
}

pub(crate) fn filter_id_to_string(id: FilterId) -> String {
	id.as_u64().to_string()
}
pub(crate) fn parse_filter_id(s: &str) -> Option<FilterId> {
	s.parse::<u64>().ok().map(FilterId::new)
}

pub(super) async fn send_subscription_event(
	sink: &Subscription,
	event: MultiFilterSubscriptionEvent,
) -> bool {
	match event {
		MultiFilterSubscriptionEvent::ReplayStatements { filter_id, statements } => {
			let statements = statements.into_iter().map(sp_core::Bytes).collect();
			sink.send(&SubscribeEvent::ReplayStatements {
				filter_id: filter_id_to_string(filter_id),
				statements,
			})
			.await
			.is_ok()
		},
		MultiFilterSubscriptionEvent::ReplayDone { filter_id } => sink
			.send(&SubscribeEvent::ReplayDone { filter_id: filter_id_to_string(filter_id) })
			.await
			.is_ok(),
		MultiFilterSubscriptionEvent::NewStatement(event) => {
			let filter_ids =
				event.matched_filter_ids.into_iter().map(filter_id_to_string).collect();
			sink.send(&SubscribeEvent::NewStatements {
				statements: vec![NewStatementEntry {
					statement: sp_core::Bytes(event.encoded),
					filter_ids,
				}],
			})
			.await
			.is_ok()
		},
		MultiFilterSubscriptionEvent::Stop => {
			let _ = sink.send(&SubscribeEvent::Stop).await;
			false
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sc_statement_store::{MultiFilterSubscriptionApi, Store};
	use std::sync::Arc;

	fn empty_subscription_state() -> (SubscriptionHandle, tempfile::TempDir) {
		let dir = tempfile::tempdir().expect("tempdir");
		let mut db_path: std::path::PathBuf = dir.path().into();
		db_path.push("db");

		type Extrinsic = sp_runtime::OpaqueExtrinsic;
		type Hash = sp_core::H256;
		type Hashing = sp_runtime::traits::BlakeTwo256;
		type BlockNumber = u64;
		type Header = sp_runtime::generic::Header<BlockNumber, Hashing>;
		type Block = sp_runtime::generic::Block<Header, Extrinsic>;
		type MockBackend = sc_client_api::in_mem::Backend<Block>;

		#[derive(Clone)]
		struct TestClient;

		impl sc_client_api::StorageProvider<Block, MockBackend> for TestClient {
			fn storage(
				&self,
				_hash: Hash,
				_key: &sc_client_api::StorageKey,
			) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
				use codec::Encode;
				let allowance =
					sp_statement_store::StatementAllowance { max_count: 1000, max_size: 1_000_000 };
				Ok(Some(sc_client_api::StorageData(allowance.encode())))
			}
			fn storage_hash(
				&self,
				_: Hash,
				_: &sc_client_api::StorageKey,
			) -> sp_blockchain::Result<Option<Hash>> {
				unimplemented!()
			}
			fn storage_keys(
				&self,
				_: Hash,
				_: Option<&sc_client_api::StorageKey>,
				_: Option<&sc_client_api::StorageKey>,
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
				_: Hash,
				_: Option<&sc_client_api::StorageKey>,
				_: Option<&sc_client_api::StorageKey>,
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
				_: Hash,
				_: &sc_client_api::ChildInfo,
				_: &sc_client_api::StorageKey,
			) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
				unimplemented!()
			}
			fn child_storage_keys(
				&self,
				_: Hash,
				_: sc_client_api::ChildInfo,
				_: Option<&sc_client_api::StorageKey>,
				_: Option<&sc_client_api::StorageKey>,
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
				_: Hash,
				_: &sc_client_api::ChildInfo,
				_: &sc_client_api::StorageKey,
			) -> sp_blockchain::Result<Option<Hash>> {
				unimplemented!()
			}
			fn closest_merkle_value(
				&self,
				_: Hash,
				_: &sc_client_api::StorageKey,
			) -> sp_blockchain::Result<Option<sc_client_api::MerkleValue<Hash>>> {
				unimplemented!()
			}
			fn child_closest_merkle_value(
				&self,
				_: Hash,
				_: &sc_client_api::ChildInfo,
				_: &sc_client_api::StorageKey,
			) -> sp_blockchain::Result<Option<sc_client_api::MerkleValue<Hash>>> {
				unimplemented!()
			}
		}

		impl sp_blockchain::HeaderBackend<Block> for TestClient {
			fn header(&self, _: Hash) -> sp_blockchain::Result<Option<Header>> {
				unimplemented!()
			}
			fn info(&self) -> sp_blockchain::Info<Block> {
				let h = sp_core::H256::repeat_byte(1);
				sp_blockchain::Info {
					best_hash: h,
					best_number: 0,
					genesis_hash: Default::default(),
					finalized_hash: h,
					finalized_number: 1,
					finalized_state: None,
					number_leaves: 0,
					block_gap: None,
				}
			}
			fn status(&self, _: Hash) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
				unimplemented!()
			}
			fn number(&self, _: Hash) -> sp_blockchain::Result<Option<BlockNumber>> {
				unimplemented!()
			}
			fn hash(&self, _: BlockNumber) -> sp_blockchain::Result<Option<Hash>> {
				unimplemented!()
			}
		}

		let store = Arc::new(
			Store::new::<Block, TestClient, MockBackend>(
				&db_path,
				Default::default(),
				Arc::new(TestClient),
				Arc::new(sc_keystore::LocalKeystore::in_memory()),
				None,
				Box::new(sp_core::testing::TaskExecutor::new()),
			)
			.expect("store"),
		);

		let (handle, _live) = store.create_subscription();
		(handle, dir)
	}

	#[test]
	fn subscription_registry_is_scoped_by_connection_id() {
		let subscriptions = StatementSubscriptions::new();
		let conn_a = ConnectionId(1);
		let conn_b = ConnectionId(2);
		let sub_id = "same-subscription-id".to_string();

		let (handle_a, _dir_a) = empty_subscription_state();
		let (handle_b, _dir_b) = empty_subscription_state();

		let entry_a = subscriptions.register(conn_a, sub_id.clone(), handle_a).unwrap();
		let _entry_b = subscriptions.register(conn_b, sub_id.clone(), handle_b).unwrap();

		assert!(subscriptions.get(conn_a, &sub_id).is_some());
		assert!(subscriptions.get(conn_b, &sub_id).is_some());

		// Dropping conn_a's entry must only remove conn_a's registration, proving the
		// registry is scoped by connection despite the shared subscription id.
		drop(entry_a);
		assert!(subscriptions.get(conn_a, &sub_id).is_none());
		assert!(subscriptions.get(conn_b, &sub_id).is_some());
	}

	#[test]
	fn duplicate_subscription_id_is_rejected_per_connection() {
		let subscriptions = StatementSubscriptions::new();
		let conn_a = ConnectionId(1);
		let conn_b = ConnectionId(2);
		let sub_id = "same-subscription-id".to_string();

		let (handle_a, _dir_a) = empty_subscription_state();
		let (duplicate_handle, _dir_dup) = empty_subscription_state();
		let (handle_b, _dir_b) = empty_subscription_state();

		let _entry = subscriptions.register(conn_a, sub_id.clone(), handle_a).unwrap();
		assert!(subscriptions.register(conn_a, sub_id.clone(), duplicate_handle).is_none());
		assert!(subscriptions.register(conn_b, sub_id, handle_b).is_some());
	}
}
