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

//! Characterises the limit-enforcement pass against the full fixture (~4M statements):
//!
//! - the steady per-tick cost of the allowance sweep over the fixture accounts when nothing is over
//!   allowance and nothing is expired (the background task pays this every
//!   `ENFORCE_LIMITS_PERIOD`);
//! - the cost of reaping a 10,000-statement expiry backlog off the global expiry index (exactly one
//!   bounded pass' statement budget).
//!
//! Requires `Store::enforce_limits` (doc-hidden, added with the on-disk submit index), so unlike
//! `benches/full_store` this only compiles on revisions that include it — it characterises the
//! new enforcement design rather than comparing it against older revisions, which enforce limits
//! through a different, non-invocable-from-outside path.
//!
//! Reuses the fixture at `$STMT_FIXTURE_DIR/db` (see `benches/full_store/common.rs`). Timed with
//! manual loops (the phases mutate store state, so criterion's automatic iteration counts do not
//! fit).

#[allow(dead_code, unused_imports)]
#[path = "full_store/common.rs"]
mod common;

use common::*;
use sp_statement_store::{StatementSource, SubmitResult};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BACKLOG: usize = 10_000;

fn main() {
	let dir = std::path::PathBuf::from(
		std::env::var("STMT_FIXTURE_DIR")
			.expect("STMT_FIXTURE_DIR must point at the fixture directory"),
	);
	let (store, _built) = open_or_build_fixture(&dir);
	// Reopen so the run starts from cold in-memory state whether or not the fixture was just
	// built (a fresh build leaves the per-account caches warm; a reused fixture starts cold).
	drop(store);
	let store = open_store_retry(&dir.join("db"), TestClient::generous());
	let _ = store.take_recent_statements();

	// 1. The steady allowance pass: every fixture account is within its allowance, nothing is
	// expired. Cycle 0 runs against cold OS caches; later cycles show the warm steady state.
	for cycle in 0..3 {
		let started = Instant::now();
		store.enforce_limits();
		println!(
			"ENFORCE_META allowance_pass_cycle{}_secs={:.3}",
			cycle,
			started.elapsed().as_secs_f64()
		);
	}

	// 2. An expiry backlog of exactly one pass' statement budget: submitted a few seconds ahead
	// of expiry, reaped off the global expiry index by a single call.
	let keypair = sacrifice_keypair();
	let base = fresh_id_base();
	let now_secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("system clock after epoch; qed")
		.as_secs();
	let expiry = (now_secs + 4) << 32;
	let started = Instant::now();
	let mut last_hash = None;
	for k in 0..BACKLOG as u64 {
		let statement = create_statement(base + k, &[], None, 64, expiry, &keypair);
		last_hash = Some(statement.hash());
		let result = store.submit(statement, StatementSource::Local);
		assert!(matches!(result, SubmitResult::New), "backlog statement rejected: {:?}", result);
	}
	println!("ENFORCE_META backlog_submit_{}_secs={:.3}", BACKLOG, started.elapsed().as_secs_f64());
	let last_hash = last_hash.expect("backlog is not empty; qed");
	std::thread::sleep(Duration::from_secs(6));

	let started = Instant::now();
	store.enforce_limits();
	println!("ENFORCE_META expiry_sweep_{}_secs={:.3}", BACKLOG, started.elapsed().as_secs_f64());
	assert!(!store.has_statement(&last_hash), "the backlog must have been reaped");

	// 3. The pass right after the sweep: back to the steady state.
	let started = Instant::now();
	store.enforce_limits();
	println!("ENFORCE_META post_sweep_pass_secs={:.3}", started.elapsed().as_secs_f64());
}
