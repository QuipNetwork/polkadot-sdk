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

//! Builds the full-store fixture (~4M statements, see `benches/full_store/common.rs`) and
//! measures the *benefit* side of moving the statement-store index to disk:
//!
//! - live heap (tracking global allocator) after population, after draining `recent`, after a drop
//!   + reopen (the steady-state index footprint a node restart would pay), and after a read
//!   workload (whether reads retain memory);
//! - process RSS at the same points (coarse: includes mmapped DB pages and allocator retention);
//! - wall-clock population and reopen (index load) times;
//! - on-disk DB size.
//!
//! The fixture is left in place at `$STMT_FIXTURE_DIR/db` so `benches/full_store` can reuse it.
//! The directory must not contain a previous fixture (remove it first): a clean build is required
//! for meaningful heap deltas.
//!
//! Uses only the public `StatementStore` API, so the same file runs against any revision.

#[allow(dead_code, unused_imports)]
#[path = "../benches/full_store/common.rs"]
mod common;

use common::*;
use std::{
	alloc::{GlobalAlloc, Layout, System},
	sync::atomic::{AtomicUsize, Ordering},
	time::Instant,
};

// --- Tracking global allocator: counts live (current) and peak heap bytes. ----------------------

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		let ptr = System.alloc(layout);
		if !ptr.is_null() {
			let current = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
			PEAK.fetch_max(current, Ordering::Relaxed);
		}
		ptr
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		System.dealloc(ptr, layout);
		CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
	}
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

/// Resident set size of this process, in bytes (Linux; assumes a 4 KiB page).
fn rss_bytes() -> usize {
	let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
	let resident_pages: usize =
		statm.split_whitespace().nth(1).and_then(|f| f.parse().ok()).unwrap_or(0);
	resident_pages * 4096
}

/// Waits until the live-heap figure stabilises (parity-db's background worker flushes its commit
/// overlay asynchronously), then returns it. Stability: three consecutive 2s samples within 1 MiB,
/// or a 60s cap.
fn settled_heap(label: &str) -> usize {
	let mut last = CURRENT.load(Ordering::Relaxed);
	let mut stable = 0;
	for _ in 0..30 {
		std::thread::sleep(std::time::Duration::from_secs(2));
		let now = CURRENT.load(Ordering::Relaxed);
		if now.abs_diff(last) < 1024 * 1024 {
			stable += 1;
			if stable >= 3 {
				break;
			}
		} else {
			stable = 0;
		}
		last = now;
	}
	let heap = CURRENT.load(Ordering::Relaxed);
	eprintln!("heap settled after {}: {:.1} MiB", label, heap as f64 / (1024.0 * 1024.0));
	heap
}

fn main() {
	let dir = std::path::PathBuf::from(
		std::env::var("STMT_FIXTURE_DIR")
			.expect("STMT_FIXTURE_DIR must point at the fixture directory"),
	);
	let db_path = dir.join("db");
	assert!(
		!db_path.exists(),
		"{} already exists; remove it first (a clean build is required for heap deltas)",
		db_path.display()
	);
	std::fs::create_dir_all(&dir).expect("fixture dir is creatable");

	// Baseline: an empty store plus the harness. Index heap figures are growth from here.
	let store = open_store(&db_path, TestClient::generous());
	let baseline = settled_heap("empty store");
	PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);

	let build_secs = populate_fixture(&store);
	let heap_populated = settled_heap("population");
	let rss_populated = rss_bytes();

	let drained = store.take_recent_statements().expect("take_recent_statements works").len();
	let heap_drained = settled_heap("draining `recent`");
	let peak = PEAK.load(Ordering::Relaxed);

	drop(store);
	let heap_dropped = settled_heap("dropping the store");

	// Reopen: the index-load cost a node restart pays, and the steady-state index heap.
	let reopen_started = Instant::now();
	let store = open_store_retry(&db_path, TestClient::generous());
	let reopen_secs = reopen_started.elapsed().as_secs_f64();
	let heap_reopened = settled_heap("reopen");
	let rss_reopened = rss_bytes();

	// Read workload: bounded, diverse and scaled queries; checks whether reads retain memory.
	let mut matched = 0usize;
	for _ in 0..100 {
		matched += store.broadcasts(&t01()).unwrap().len();
		matched += store.posted(&[], dk42()).unwrap().len();
	}
	let group_base = diverse_topic_group().saturating_sub(1000);
	for k in 0..1000u64 {
		matched += store.broadcasts(&[topic(1000 + group_base + k)]).unwrap().len();
	}
	matched += store.broadcasts(&t23()).unwrap().len();
	let heap_after_reads = settled_heap("read workload");
	let rss_after_reads = rss_bytes();

	let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
	println!("FULL4M_MEM n={}", n_statements());
	println!("FULL4M_MEM build_secs={:.1}", build_secs);
	println!("FULL4M_MEM reopen_secs={:.2}", reopen_secs);
	println!("FULL4M_MEM drained_recent={}", drained);
	println!("FULL4M_MEM read_workload_matches={}", matched);
	println!("FULL4M_MEM heap_baseline_mib={:.1}", mib(baseline));
	println!("FULL4M_MEM heap_populated_mib={:.1}", mib(heap_populated.saturating_sub(baseline)));
	println!("FULL4M_MEM heap_drained_mib={:.1}", mib(heap_drained.saturating_sub(baseline)));
	println!("FULL4M_MEM heap_peak_mib={:.1}", mib(peak.saturating_sub(baseline)));
	println!("FULL4M_MEM heap_dropped_mib={:.1}", mib(heap_dropped.saturating_sub(baseline)));
	println!("FULL4M_MEM heap_reopened_mib={:.1}", mib(heap_reopened.saturating_sub(baseline)));
	println!(
		"FULL4M_MEM heap_after_reads_mib={:.1}",
		mib(heap_after_reads.saturating_sub(baseline))
	);
	println!("FULL4M_MEM rss_populated_mib={:.1}", mib(rss_populated));
	println!("FULL4M_MEM rss_reopened_mib={:.1}", mib(rss_reopened));
	println!("FULL4M_MEM rss_after_reads_mib={:.1}", mib(rss_after_reads));
	println!("FULL4M_MEM db_size_mib={:.1}", db_size_bytes(&dir) as f64 / (1024.0 * 1024.0));

	drop(store);
}
