// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Experimental validator-side adapter: drives the production
//! `ProtocolSide::ValidatorExperimental` variant of
//! [`polkadot_collator_protocol::CollatorProtocolSubsystem`].

use crate::common::{
	harness::SubsystemUnderTest,
	runtime::{LocalPoolSpawner, MockClock},
};
use futures::{future::BoxFuture, FutureExt};
use polkadot_collator_protocol::{CollatorProtocolSubsystem, ProtocolSide, ReputationConfig};
use polkadot_node_subsystem::{
	messages::{AllMessages, CollatorProtocolMessage},
	overseer::Subsystem,
	SpawnGlue,
};
use polkadot_node_subsystem_test_helpers::TestSubsystemContext;
use polkadot_node_subsystem_util::database::{kvdb_impl::DbAdapter, Database};
use sp_keystore::Keystore;
use std::sync::Arc;

/// Adapter for the experimental `ProtocolSide::ValidatorExperimental` variant.
pub struct ExperimentalValidator;

impl SubsystemUnderTest for ExperimentalValidator {
	type Message = CollatorProtocolMessage;

	fn spawn(
		ctx: TestSubsystemContext<Self::Message, SpawnGlue<LocalPoolSpawner>>,
		clock: Arc<MockClock>,
	) -> BoxFuture<'static, ()> {
		let keystore: sp_keystore::KeystorePtr = Arc::new(sc_keystore::LocalKeystore::in_memory());
		Keystore::sr25519_generate_new(
			&*keystore,
			polkadot_primitives::PARACHAIN_KEY_TYPE_ID,
			Some(&sp_keyring::Sr25519Keyring::Alice.to_seed()),
		)
		.expect("keystore accepts inserted key");

		const NUM_COLUMNS: u32 = 1;
		const REPUTATION_COL: u32 = 0;
		let db = kvdb_memorydb::create(NUM_COLUMNS);
		let db: Arc<dyn Database> = Arc::new(DbAdapter::new(db, &[]));

		let reputation_config =
			ReputationConfig { col_reputation_data: REPUTATION_COL, persist_interval: None };

		let side = ProtocolSide::ValidatorExperimental {
			keystore,
			metrics: Default::default(),
			db,
			reputation_config,
			clock,
		};
		let subsystem = CollatorProtocolSubsystem::new(side);
		let spawned = subsystem.start(ctx);
		spawned.future.map(|_| ()).boxed()
	}

	fn try_extract_inbound(msg: AllMessages) -> Result<Self::Message, AllMessages> {
		match msg {
			AllMessages::CollatorProtocol(inner) => Ok(inner),
			other => Err(other),
		}
	}

	fn our_view_change(view: polkadot_node_network_protocol::OurView) -> Option<Self::Message> {
		Some(CollatorProtocolMessage::NetworkBridgeUpdate(
			polkadot_node_subsystem::messages::NetworkBridgeEvent::OurViewChange(view),
		))
	}
}
