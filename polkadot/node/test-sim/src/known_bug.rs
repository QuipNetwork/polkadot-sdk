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

//! Known-bug test support.
//!
//! Lets a test assert the *correct* (not-yet-implemented) behavior while a tracked bug is still
//! open: the body is expected to fail, so the failure is swallowed and the test stays green. The
//! moment the body stops failing — the bug got fixed — the test fails loudly, telling the author
//! to remove the marker.
//!
//! This is the backbone of a test-first workflow: land the scenarios as known-bugs first, then
//! the fix PR's diff is just the removal of the markers, highlighting exactly what it fixed.
//!
//! Most call sites use the `#[known_bug]` attribute from `polkadot-subsystem-test-sim-macros`
//! rather than calling [`run_known_bug`] directly; the attribute keeps the fix-PR diff to a
//! single deleted line (the body and its indentation are untouched). [`run_known_bug`] is the
//! shared runtime the attribute expands to, and is also called directly by per-subsystem
//! fan-out macros (e.g. collator-protocol's `sim_test`) that generate the test functions
//! themselves.

use std::{
	cell::Cell,
	panic::{self, AssertUnwindSafe},
	sync::Once,
};

thread_local! {
	/// Set for the duration of a [`run_known_bug`] body. While set, the installed panic
	/// hook suppresses output *on this thread only*. Read by the hook in [`install_hook`].
	static SUPPRESS_PANIC_OUTPUT: Cell<bool> = const { Cell::new(false) };
}

/// Install — exactly once for the whole process — a panic hook that delegates to the hook
/// present at install time, except on threads that have opted into suppression via
/// [`SUPPRESS_PANIC_OUTPUT`].
///
/// Doing this once and gating per-thread is what makes [`run_known_bug`] safe under cargo's
/// default parallel test execution. The naive alternative — `set_hook(empty)` around the
/// body and `set_hook(prev)` after — mutates a *process-global* slot, so two known-bug tests
/// (or a known-bug test and an unrelated failing test) running concurrently race: one can
/// restore the other's temporary empty hook permanently, silently swallowing later genuine
/// failures. A per-thread flag keeps each `run_known_bug` isolated and never hides panics
/// raised on any other thread.
fn install_hook() {
	static INSTALL: Once = Once::new();
	INSTALL.call_once(|| {
		let previous = panic::take_hook();
		panic::set_hook(Box::new(move |info| {
			let suppress = SUPPRESS_PANIC_OUTPUT.with(|s| s.get());
			if !suppress {
				previous(info);
			}
		}));
	});
}

/// Run `body` with this thread's panic output suppressed, restoring the previous value
/// afterwards (so nested or sequential calls compose). Returns whether `body` ran to
/// completion without panicking.
fn run_suppressed<F: FnOnce()>(body: F) -> bool {
	install_hook();
	let was_suppressed = SUPPRESS_PANIC_OUTPUT.with(|s| s.replace(true));
	let result = panic::catch_unwind(AssertUnwindSafe(body));
	SUPPRESS_PANIC_OUTPUT.with(|s| s.set(was_suppressed));
	result.is_ok()
}

/// Run a test body that is expected to fail because of a known, tracked bug.
///
/// While the bug is open the body panics; the panic is caught and the test passes. When the body
/// no longer panics — the bug has been fixed — this panics with a message naming the test and the
/// tracking URL (if any), instructing the author to remove the known-bug marker so the test
/// asserts the fixed behavior going forward.
///
/// The body's own panic output (backtrace, the failed assertion) is suppressed *on the running
/// thread only* while the bug is open, so CI is not littered with the expected failure on the
/// normal still-buggy path. Panics raised on other threads — including unrelated tests running
/// concurrently — are unaffected.
pub fn run_known_bug<F>(test_name: &str, tracking_url: Option<&str>, body: F)
where
	F: FnOnce(),
{
	let completed_without_panic = run_suppressed(body);

	if completed_without_panic {
		let tracking = match tracking_url {
			Some(url) => format!(" Tracking: {url}"),
			None => String::new(),
		};
		panic!(
			"KNOWN-BUG TEST PASSED: `{test_name}` no longer fails — the tracked bug appears \
			 FIXED. Remove the known-bug marker so this test asserts the fixed behavior going \
			 forward.{tracking}"
		);
	}

	// Bug still open: the body failed as expected and we swallowed it so the suite stays
	// green. Emit a single machine-greppable marker line on stdout so known bugs are
	// visible in (and countable from) the test output rather than masquerading as plain
	// passes. Captured by libtest unless `--nocapture` is passed; grep for `KNOWN-BUG` to
	// list them, or `grep -c` for the count, e.g.
	//   cargo test ... -- --nocapture 2>&1 | grep -c 'KNOWN-BUG (open)'
	let url = tracking_url.unwrap_or("(no tracking url)");
	println!("KNOWN-BUG (open) {test_name} — {url}");
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn swallows_failure_while_bug_open() {
		// Body panics (bug still open) → swallowed → this test passes.
		run_known_bug("swallows_failure_while_bug_open", Some("example#1"), || {
			panic!("the known bug");
		});
	}

	#[test]
	#[should_panic(expected = "no longer fails")]
	fn flips_loud_when_bug_fixed() {
		// Body does NOT panic (bug fixed) → run_known_bug panics, telling us to remove the marker.
		run_known_bug("flips_loud_when_bug_fixed", Some("example#1"), || {
			// fixed behavior: no panic
		});
	}

	#[test]
	#[should_panic(expected = "Tracking: example#42")]
	fn includes_tracking_url() {
		run_known_bug("includes_tracking_url", Some("example#42"), || {});
	}

	#[test]
	#[should_panic(expected = "no longer fails")]
	fn url_optional() {
		run_known_bug("url_optional", None, || {});
	}
}
