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

//! Legacy validator-side adapter: drives the production `ProtocolSide::Validator` variant of
//! [`polkadot_collator_protocol::CollatorProtocolSubsystem`].

use crate::common::{
	harness::SubsystemUnderTest,
	runtime::{LocalPoolSpawner, MockClock},
};
use futures::{future::BoxFuture, FutureExt};
use polkadot_collator_protocol::{CollatorProtocolSubsystem, ProtocolSide};
use polkadot_node_subsystem::{
	messages::{AllMessages, CollatorProtocolMessage},
	overseer::Subsystem,
	SpawnGlue,
};
use polkadot_node_subsystem_test_helpers::TestSubsystemContext;
use sc_network_types::PeerId;
use sp_keystore::Keystore;
use std::{cell::RefCell, collections::HashSet, sync::Arc, time::Duration};

/// Per-test configuration overrides for the legacy validator. Used by scenarios that need
/// to vary `invulnerables` or `collator_protocol_hold_off` away from the defaults; the
/// trait `SubsystemUnderTest::spawn` signature can't carry these directly, so we stash
/// them in a thread-local that [`set_per_test_config`] populates before `Sim::start` and
/// `LegacyValidator::spawn` reads when it constructs `ProtocolSide::Validator`.
#[derive(Clone, Default)]
pub struct LegacyValidatorConfig {
	/// `ProtocolSide::Validator { invulnerables, .. }`.
	pub invulnerables: HashSet<PeerId>,
	/// `ProtocolSide::Validator { collator_protocol_hold_off, .. }`.
	pub collator_protocol_hold_off: Option<Duration>,
}

thread_local! {
	static PER_TEST_CONFIG: RefCell<LegacyValidatorConfig> = RefCell::new(LegacyValidatorConfig::default());
}

/// Stash a config for the current test. Cleared by [`reset_per_test_config`] (call after
/// the scenario finishes if the test runs more than once on the same thread).
pub fn set_per_test_config(cfg: LegacyValidatorConfig) {
	PER_TEST_CONFIG.with(|c| *c.borrow_mut() = cfg);
}

/// Drop any config previously set by [`set_per_test_config`].
pub fn reset_per_test_config() {
	PER_TEST_CONFIG.with(|c| *c.borrow_mut() = LegacyValidatorConfig::default());
}

/// Adapter for the legacy `ProtocolSide::Validator` variant.
pub struct LegacyValidator;

impl SubsystemUnderTest for LegacyValidator {
	type Message = CollatorProtocolMessage;

	fn spawn(
		ctx: TestSubsystemContext<Self::Message, SpawnGlue<LocalPoolSpawner>>,
		clock: Arc<MockClock>,
	) -> BoxFuture<'static, ()> {
		let keystore: sp_keystore::KeystorePtr = Arc::new(sc_keystore::LocalKeystore::in_memory());
		// Insert a single Sr25519 key so the keystore is non-empty (the production code path
		// expects keys present for sign-on-second).
		Keystore::sr25519_generate_new(
			&*keystore,
			polkadot_primitives::PARACHAIN_KEY_TYPE_ID,
			Some(&sp_keyring::Sr25519Keyring::Alice.to_seed()),
		)
		.expect("keystore accepts inserted key");

		let cfg = PER_TEST_CONFIG.with(|c| c.borrow().clone());

		let side = ProtocolSide::Validator {
			keystore,
			eviction_policy: Default::default(),
			metrics: Default::default(),
			invulnerables: cfg.invulnerables,
			collator_protocol_hold_off: cfg.collator_protocol_hold_off,
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
