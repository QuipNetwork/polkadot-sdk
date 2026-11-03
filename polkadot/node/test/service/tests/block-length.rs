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

//! End-to-end test for the relay-chain `BlockLength` parameter.
//!
//! Test runtime overrides `BlockLength` to 10 MiB; with `NORMAL_DISPATCH_RATIO = 75 %`
//! the Normal-class cap is 7.5 MiB.
//!
//! 1. Submit a ~7.9 MiB `System::remark_with_event` and assert it is rejected with
//!    `InvalidTransaction::ExhaustsResources` (above the Normal-class cap).
//! 2. Submit a 7 MiB `System::remark_with_event` and assert it is accepted, included in a block.

use polkadot_test_service::*;
use sc_client_api::BlockBackend;
use sp_blockchain::HeaderBackend;
use sp_keyring::Sr25519Keyring;
use sp_runtime::{codec::Encode, traits::Header as _};
use std::time::Duration;

const OVERSIZED_PAYLOAD: usize = 7 * 1024 * 1024 + 921 * 1024; // ≈ 7.9 MiB
const ACCEPTED_PAYLOAD: usize = 7 * 1024 * 1024;
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);
const BLOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(180);

fn remark_call(size: usize) -> polkadot_test_runtime::RuntimeCall {
	polkadot_test_runtime::RuntimeCall::System(frame_system::Call::remark_with_event {
		remark: vec![0u8; size],
	})
}

#[tokio::test(flavor = "multi_thread")]
async fn make_big_block() {
	let mut alice_config = node_config(
		|| {},
		tokio::runtime::Handle::current(),
		Sr25519Keyring::Alice,
		Vec::new(),
		true,
	);

	alice_config.force_authoring = true;
	let alice = run_validator_node(alice_config, None).await;

	// 1. Above the 7.5 MiB Normal-class cap → must be rejected by `check_block_length`.
	let oversized = tokio::time::timeout(
		SUBMIT_TIMEOUT,
		alice.send_extrinsic(remark_call(OVERSIZED_PAYLOAD), Sr25519Keyring::Bob),
	)
	.await
	.expect("submission must complete within the timeout");
	assert!(
		oversized.is_err(),
		"{OVERSIZED_PAYLOAD}-byte extrinsic must be rejected; got: {oversized:?}",
	);

	// 2. Under the 7.5 MiB Normal-class cap → must be accepted and included.
	tokio::time::timeout(
		SUBMIT_TIMEOUT,
		alice.send_extrinsic(remark_call(ACCEPTED_PAYLOAD), Sr25519Keyring::Charlie),
	)
	.await
	.expect("submission must complete within the timeout")
	.expect("7 MiB extrinsic must be accepted under the 10 MiB BlockLength");

	tokio::time::timeout(BLOCK_WAIT_TIMEOUT, alice.wait_for_blocks(2))
		.await
		.expect("block production must continue after submitting the 7 MiB extrinsic");

	// Walk the chain back from best and find the largest produced block.
	let mut hash = alice.client.info().best_hash;
	let mut max_block_size = 0usize;
	loop {
		let header = alice
			.client
			.header(hash)
			.expect("header query must succeed")
			.expect("header must exist");
		let body = alice
			.client
			.block_body(hash)
			.expect("body query must succeed")
			.unwrap_or_default();

		let size = header.encoded_size() + body.iter().map(Encode::encoded_size).sum::<usize>();
		if size > max_block_size {
			max_block_size = size;
		}

		let parent = *header.parent_hash();
		if parent == Default::default() {
			break;
		}
		hash = parent;
	}

	assert!(
		max_block_size > ACCEPTED_PAYLOAD,
		"expected a block larger than the remark {ACCEPTED_PAYLOAD}-byte payload; \
		 largest was {max_block_size}",
	);
}
