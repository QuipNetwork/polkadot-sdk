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

//! Benchmarks for the multi-filter subscription API (`create_subscription` / `add_filter`)
//! against a full store (~4M statements). Unlike `benches/full_store`, this requires the
//! multi-filter API (#11989), so it only compiles on revisions that include it.
//!
//! `add_filter` collects the filter's replay snapshot from the store index while holding a store
//! lock that concurrent submits also need (in-RAM index: held across a RAM scan; on-disk index:
//! held across a disk scan). Measured here:
//! - `add_filter` latency for a narrow filter (8-hit snapshot) and for `Any` (whole-store
//!   snapshot);
//! - how long a submit stalls while an `add_filter(Any)` snapshot scan is in flight, against the
//!   same submit with no scan running.
//!
//! Measured with manual paced loops rather than criterion: every iteration creates and drops a
//! subscription, and criterion's iteration counts would outrun the matcher task's bounded message
//! channel (`add_filter` then fails with `Stopped` instead of measuring anything).
//!
//! Reuses the fixture at `$STMT_FIXTURE_DIR/db` (see `benches/full_store/common.rs`).

#[allow(dead_code, unused_imports)]
#[path = "full_store/common.rs"]
mod common;

use common::*;
use sc_statement_store::MultiFilterSubscriptionApi;
use sp_core::Pair as _;
use sp_statement_store::{OptimizedTopicFilter, StatementSource, SubmitResult};
use std::{
	collections::HashSet,
	sync::Arc,
	time::{Duration, Instant},
};

/// Times `op` over `n` iterations. `setup` runs untimed before each iteration and its value is
/// dropped untimed after it; `pacing` sleeps between iterations (untimed) let the matcher task
/// keep up with the create/add/drop churn. Returns (mean, min, max) in seconds.
fn timed_loop<S>(
	n: usize,
	pacing: Duration,
	mut setup: impl FnMut() -> S,
	mut op: impl FnMut(&S),
) -> (f64, f64, f64) {
	let mut total = 0.0f64;
	let mut min = f64::MAX;
	let mut max = 0.0f64;
	for _ in 0..n {
		let state = setup();
		let started = Instant::now();
		op(&state);
		let secs = started.elapsed().as_secs_f64();
		total += secs;
		min = min.min(secs);
		max = max.max(secs);
		drop(state);
		std::thread::sleep(pacing);
	}
	(total / n as f64, min, max)
}

fn main() {
	let dir = std::path::PathBuf::from(
		std::env::var("STMT_FIXTURE_DIR")
			.expect("STMT_FIXTURE_DIR must point at the fixture directory"),
	);
	let (store, built) = open_or_build_fixture(&dir);
	if let Some(secs) = built {
		println!("ADDFILTER_META build_secs={:.1}", secs);
	}
	let drained = store.take_recent_statements().expect("take_recent_statements works").len();
	println!("ADDFILTER_META drained_recent={}", drained);
	let store = Arc::new(store);

	// Narrow filter: the snapshot is 8 hashes out of ~4.2M (the realistic "subscribe to my
	// topic" story). Subscription creation and teardown are outside the timed section.
	let narrow = OptimizedTopicFilter::MatchAll(HashSet::from([diverse_topic()]));
	let create = || store.create_subscription();
	// Warmup.
	timed_loop(50, Duration::from_millis(1), create, |(handle, _stream)| {
		handle.add_filter(narrow.clone()).expect("filter attaches");
	});
	let (mean, min, max) =
		timed_loop(500, Duration::from_millis(1), create, |(handle, _stream)| {
			handle.add_filter(narrow.clone()).expect("filter attaches");
		});
	println!(
		"ADDFILTER_META add_filter_diverse_8 mean_us={:.2} min_us={:.2} max_us={:.2}",
		mean * 1e6,
		min * 1e6,
		max * 1e6
	);

	// Whole-store snapshot: `Any` enumerates every statement hash under the store lock. The
	// snapshot Vec (~4.2M hashes) travels to the matcher and is freed on unsubscribe, so pace
	// generously.
	let (mean, min, max) =
		timed_loop(5, Duration::from_millis(500), create, |(handle, _stream)| {
			handle.add_filter(OptimizedTopicFilter::Any).expect("filter attaches");
		});
	println!(
		"ADDFILTER_META add_filter_any_4m mean_s={:.3} min_s={:.3} max_s={:.3}",
		mean, min, max
	);

	// Submit stall: start an `add_filter(Any)` scan on another thread, give it a head start to
	// take the lock, then time a single submit. Compared against the same submit with no scan in
	// flight.
	let sacrifice = sacrifice_keypair();
	for cycle in 0..3 {
		let (handle, stream) = store.create_subscription();
		let started = Instant::now();
		let scan = std::thread::spawn(move || {
			let ok = handle.add_filter(OptimizedTopicFilter::Any).is_ok();
			(ok, started.elapsed().as_secs_f64())
		});
		std::thread::sleep(Duration::from_millis(10));

		let statement = create_statement(
			fresh_id_base(),
			&[],
			None,
			STATEMENT_DATA_SIZE,
			LOW_EXPIRY,
			&sacrifice,
		);
		let submit_started = Instant::now();
		let result = store.submit(statement, StatementSource::Local);
		let submit_during_scan = submit_started.elapsed().as_secs_f64();
		assert!(matches!(result, SubmitResult::New));

		let (filter_ok, addfilter_secs) = scan.join().expect("scan thread joins");
		assert!(filter_ok, "add_filter(Any) must succeed");
		drop(stream);

		let statement = create_statement(
			fresh_id_base() | 1,
			&[],
			None,
			STATEMENT_DATA_SIZE,
			LOW_EXPIRY,
			&sacrifice,
		);
		let submit_started = Instant::now();
		let result = store.submit(statement, StatementSource::Local);
		let submit_no_scan = submit_started.elapsed().as_secs_f64();
		assert!(matches!(result, SubmitResult::New));

		println!(
			"ADDFILTER_META cycle{} addfilter_any_secs={:.3} submit_during_scan_secs={:.4} submit_no_scan_secs={:.5}",
			cycle, addfilter_secs, submit_during_scan, submit_no_scan
		);
	}

	// Remove the handful of sacrificial submissions so the fixture stays pristine for reruns.
	let public = sacrifice_keypair().public();
	let who: [u8; 32] = AsRef::<[u8]>::as_ref(&public)
		.try_into()
		.expect("ed25519 public key is 32 bytes; qed");
	store.remove_by(who).expect("cleanup succeeds");

	// Let the matcher drain outstanding unsubscribes before dropping the store.
	std::thread::sleep(Duration::from_secs(1));
	drop(store);
}
