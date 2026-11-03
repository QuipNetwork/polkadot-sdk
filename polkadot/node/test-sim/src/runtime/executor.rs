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

//! Single-threaded deterministic executor harness.
//!
//! Built on `futures::executor::LocalPool` so task scheduling is FIFO and bounded by the test
//! thread. Combined with a [`crate::runtime::MockClock`], scenario execution becomes
//! reproducible from a seed.
//!
//! Two primitives the harness exposes:
//! - [`Executor::poll_until_pending`] — drive every spawned task forward until each is `Pending`
//!   (i.e., parked on a channel / clock wakeup / external event). The "settle" step that every
//!   stimulus injection ends with.
//! - [`Executor::run_until`] — block until a single future completes, draining auxiliary tasks as
//!   needed.

use crate::runtime::local_spawner::LocalPoolSpawnDrain;
use futures::{
	executor::{LocalPool, LocalSpawner},
	future::Future,
	task::{LocalSpawnExt, SpawnExt},
};

/// Single-threaded executor wrapper. Holds a `LocalPool`; tests drive it via
/// [`Self::poll_until_pending`] / [`Self::run_until`].
pub struct Executor {
	pool: LocalPool,
	/// Drain handle for the `LocalPoolSpawner` — futures spawned via `Spawner::spawn`
	/// land in a queue, and we pull them onto the LocalPool between settle passes.
	spawn_drain: Option<LocalPoolSpawnDrain>,
}

impl Default for Executor {
	fn default() -> Self {
		Self::new()
	}
}

impl Executor {
	/// Create a fresh executor without a `LocalPoolSpawner` drain. Background tasks the
	/// harness spawns via `ctx.spawn(...)` will not be driven on this `LocalPool` —
	/// suitable only for tests that don't trigger such spawns.
	pub fn new() -> Self {
		Self { pool: LocalPool::new(), spawn_drain: None }
	}

	/// Attach a [`LocalPoolSpawnDrain`] so futures spawned via the matching
	/// [`crate::runtime::local_spawner::LocalPoolSpawner`] are pulled onto this `LocalPool`
	/// inside `poll_until_pending`.
	pub fn set_spawn_drain(&mut self, drain: LocalPoolSpawnDrain) {
		self.spawn_drain = Some(drain);
	}

	/// Spawn a `!Send` future on the pool. Useful when the future captures non-`Send` test
	/// state.
	pub fn spawn_local<Fut>(&self, fut: Fut)
	where
		Fut: Future<Output = ()> + 'static,
	{
		self.pool
			.spawner()
			.spawn_local(fut)
			.expect("LocalPool spawner accepts spawns; qed");
	}

	/// Spawn a `Send` future on the pool. Subsystem futures returned by
	/// `polkadot-overseer` typically require `Send`; this is the path they take.
	pub fn spawn<Fut>(&self, fut: Fut)
	where
		Fut: Future<Output = ()> + Send + 'static,
	{
		self.pool.spawner().spawn(fut).expect("LocalPool spawner accepts spawns; qed");
	}

	/// Poll every spawned task until none can make further progress. Returns when every task
	/// is `Pending`. Combined with a `MockClock`, this is the standard "settle" primitive: feed
	/// a stimulus, settle, observe.
	///
	/// Before each pass, drains any queued spawns from the `LocalPoolSpawner` (if attached) so
	/// subsystem-spawned background tasks land on this `LocalPool`. Drains-then-polls
	/// repeatedly until both the queue is empty and the pool is stalled.
	pub fn poll_until_pending(&mut self) {
		loop {
			let pulled = self.pull_pending_spawns();
			self.pool.run_until_stalled();
			if !pulled {
				break;
			}
		}
	}

	fn pull_pending_spawns(&mut self) -> bool {
		let Some(drain) = self.spawn_drain.as_ref() else { return false };
		let pending = drain.drain();
		if pending.is_empty() {
			return false;
		}
		for fut in pending {
			self.pool.spawner().spawn(fut).expect("LocalPool spawner accepts spawns; qed");
		}
		true
	}

	/// Run the pool until `fut` completes. Auxiliary spawned tasks are polled as part of the
	/// pool's normal scheduling.
	pub fn run_until<F: Future>(&mut self, fut: F) -> F::Output {
		self.pool.run_until(fut)
	}

	/// A handle to spawn additional tasks. Useful when test code needs to inject side-channel
	/// futures (e.g., a mock-peer driver).
	pub fn spawner(&self) -> LocalSpawner {
		self.pool.spawner()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{cell::Cell, rc::Rc};

	#[test]
	fn run_until_completes_a_future() {
		let mut exec = Executor::new();
		let result = exec.run_until(async { 42 });
		assert_eq!(result, 42);
	}

	#[test]
	fn poll_until_pending_drains_ready_work() {
		let mut exec = Executor::new();
		let counter = Rc::new(Cell::new(0));
		let counter_in_task = counter.clone();
		exec.spawn_local(async move {
			counter_in_task.set(counter_in_task.get() + 1);
		});
		assert_eq!(counter.get(), 0);
		exec.poll_until_pending();
		assert_eq!(counter.get(), 1);
	}

	#[test]
	fn poll_until_pending_stops_at_pending_task() {
		use futures::channel::oneshot;

		let mut exec = Executor::new();
		let (tx, rx) = oneshot::channel::<()>();
		let counter = Rc::new(Cell::new(0));
		let counter_in_task = counter.clone();
		exec.spawn_local(async move {
			let _ = rx.await;
			counter_in_task.set(counter_in_task.get() + 1);
		});

		// Task is parked on the oneshot. poll_until_pending returns without completing.
		exec.poll_until_pending();
		assert_eq!(counter.get(), 0);

		// Sending the value lets the task complete on the next poll.
		tx.send(()).unwrap();
		exec.poll_until_pending();
		assert_eq!(counter.get(), 1);
	}
}
