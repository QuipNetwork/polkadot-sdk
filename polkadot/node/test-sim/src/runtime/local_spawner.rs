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

//! Single-threaded spawner that funnels every spawned task into the harness's
//! [`crate::runtime::Executor`] (`LocalPool`). Required for `Spawner: Send + Sync` traits,
//! which `LocalSpawner` itself does not satisfy.
//!
//! Why this matters: real subsystems (e.g. `candidate-backing`) call `ctx.spawn(...)` for
//! background work like candidate validation. The default `sp_core::testing::TaskExecutor`
//! is a `ThreadPool`-backed spawner — those tasks run on OS threads outside the
//! `LocalPool`. The harness's `Executor::poll_until_pending()` doesn't drive them, so
//! their results don't propagate deterministically and tests time out waiting for
//! cross-task notifications.
//!
//! `LocalPoolSpawner` works around this by stashing every spawned future into a global
//! `Mutex<Vec<BoxFuture>>`, then `Executor::drain_pending_spawns()` is called between
//! `run_until_stalled` passes to spawn them locally. End result: all subsystem
//! background tasks run on the same `LocalPool`, polled deterministically.

use futures::future::BoxFuture;
use std::sync::{Arc, Mutex};

/// Shared queue of pending spawns. The harness owns one instance; every subsystem context
/// receives a clone so spawn calls land in the same queue. The `Executor` drains the queue
/// inside `poll_until_pending` to spawn futures on its `LocalPool`.
#[derive(Clone)]
pub struct LocalPoolSpawner {
	queue: Arc<Mutex<Vec<BoxFuture<'static, ()>>>>,
}

impl LocalPoolSpawner {
	/// Fresh spawner with an empty queue.
	pub fn new() -> Self {
		Self { queue: Arc::new(Mutex::new(Vec::new())) }
	}

	/// Drain handle that shares the same underlying queue. Hand to `Executor`.
	pub fn drain_handle(&self) -> LocalPoolSpawnDrain {
		LocalPoolSpawnDrain { queue: self.queue.clone() }
	}

	fn enqueue(&self, fut: BoxFuture<'static, ()>) {
		self.queue.lock().expect("LocalPoolSpawner queue mutex poisoned").push(fut);
	}
}

impl Default for LocalPoolSpawner {
	fn default() -> Self {
		Self::new()
	}
}

/// Drain handle. Held by the harness's `Executor`. Pull queued spawns out for
/// `LocalPool::spawner().spawn(...)`.
pub struct LocalPoolSpawnDrain {
	queue: Arc<Mutex<Vec<BoxFuture<'static, ()>>>>,
}

impl LocalPoolSpawnDrain {
	/// Take all currently-queued futures.
	pub fn drain(&self) -> Vec<BoxFuture<'static, ()>> {
		std::mem::take(&mut *self.queue.lock().expect("LocalPoolSpawner queue mutex poisoned"))
	}
}

impl polkadot_overseer::gen::Spawner for LocalPoolSpawner {
	fn spawn(
		&self,
		_name: &'static str,
		_group: Option<&'static str>,
		future: BoxFuture<'static, ()>,
	) {
		self.enqueue(future);
	}

	fn spawn_blocking(
		&self,
		name: &'static str,
		group: Option<&'static str>,
		future: BoxFuture<'static, ()>,
	) {
		self.spawn(name, group, future)
	}
}

impl sp_core::traits::SpawnNamed for LocalPoolSpawner {
	fn spawn(
		&self,
		_name: &'static str,
		_group: Option<&'static str>,
		future: BoxFuture<'static, ()>,
	) {
		self.enqueue(future);
	}

	fn spawn_blocking(
		&self,
		name: &'static str,
		group: Option<&'static str>,
		future: BoxFuture<'static, ()>,
	) {
		<Self as sp_core::traits::SpawnNamed>::spawn(self, name, group, future)
	}
}
