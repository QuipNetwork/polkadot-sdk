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

//! Integration test that ensures that we can build and include parachain
//! blocks of the `Undying` parachain.

// If this test is failing, make sure to run all tests with the `real-overseer` feature being
// enabled.

use polkadot_node_subsystem::TimeoutExt;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread")]
async fn collating_using_undying_collator() {
	use polkadot_primitives::Id as ParaId;
	use sp_keyring::Sr25519Keyring::*;

	let mut builder = sc_cli::LoggerBuilder::new("");
	builder.with_colors(false);
	builder.init().expect("Set up logger");

	let para_id = ParaId::from(100);

	let alice_config = polkadot_test_service::node_config(
		|| {},
		tokio::runtime::Handle::current(),
		Alice,
		Vec::new(),
		true,
	);

	let mut workers_path = std::env::current_exe().unwrap();
	workers_path.pop();
	workers_path.pop();

	// start alice
	let alice =
		polkadot_test_service::run_validator_node(alice_config, Some(workers_path.clone())).await;

	let bob_config = polkadot_test_service::node_config(
		|| {},
		tokio::runtime::Handle::current(),
		Bob,
		vec![alice.addr.clone()],
		true,
	);

	// start bob
	let bob = polkadot_test_service::run_validator_node(bob_config, Some(workers_path)).await;

	let collator = test_parachain_undying_collator::Collator::new(1_000, 1, false);

	// register parachain
	alice
		.register_parachain(para_id, collator.validation_code().to_vec(), collator.genesis_head())
		.await
		.unwrap();

	// run the collator node
	let mut charlie = polkadot_test_service::run_collator_node(
		tokio::runtime::Handle::current(),
		Charlie,
		|| {},
		vec![alice.addr.clone(), bob.addr.clone()],
		collator.collator_key(),
	)
	.await;

	charlie
		.register_collator(
			collator.collator_key(),
			para_id,
			collator.create_collation_function(charlie.task_manager.spawn_handle()),
		)
		.await;

	// Wait until the parachain has 4 blocks produced.
	collator
		.wait_for_blocks(4)
		.timeout(TIMEOUT)
		.await
		.expect("Timed out waiting for 4 produced blocks");
}
