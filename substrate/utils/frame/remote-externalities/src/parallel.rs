// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Parallel worker infrastructure for remote externalities.

use crate::client::{Client, ConnectionManager};
use std::{
	collections::VecDeque,
	future::Future,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc, Mutex,
	},
	time::Duration,
};
use tokio::time::sleep;

/// What to do with the worker's client after a failed work item, before retrying.
pub(crate) enum RetryAction {
	/// Reconnect the client and keep it in the pool.
	Recreate,
	/// Drop the client from the pool because it lacks the target block.
	Remove,
}

/// Result of processing a work item.
pub(crate) enum ProcessResult<W> {
	/// Work completed successfully, optionally queue more work items.
	Success { new_work: Vec<W> },
	/// Work failed and should be retried.
	Retry {
		/// The work item to retry (possibly modified, e.g. with reduced batch size).
		work: W,
		/// How long to sleep before retrying.
		sleep_duration: Duration,
		/// What to do with the worker's client before retrying.
		action: RetryAction,
	},
}

/// Run parallel workers that process items from a work queue.
///
/// This function handles all the common parallel worker infrastructure:
/// - Spawning `parallel` worker tasks with semaphore-based concurrency
/// - Work queue management (pop/push)
/// - Active worker tracking for proper termination
/// - Client recreation on retry
/// - Sleep delays on retry
///
/// The `processor` closure receives:
/// - `worker_index`: The index of the worker (0..parallel)
/// - `work`: The work item to process
/// - `client`: A client for RPC calls
///
/// It should return `ProcessResult::Success` with any new work items to queue,
/// or `ProcessResult::Retry` if the work should be retried.
pub(crate) async fn run_workers<W, F, Fut>(
	initial_work: VecDeque<W>,
	conn_manager: &ConnectionManager,
	parallel: usize,
	processor: F,
) where
	F: Fn(usize, W, Client) -> Fut + Clone + Send + Sync + 'static,
	Fut: Future<Output = ProcessResult<W>> + Send,
	W: Send + 'static,
{
	let work_queue = Arc::new(Mutex::new(initial_work));
	let active_workers = Arc::new(AtomicUsize::new(0));
	let conn_manager = conn_manager.clone();

	let mut handles = vec![];

	for worker_index in 0..parallel {
		let work_queue = work_queue.clone();
		let active_workers = active_workers.clone();
		let conn_manager = conn_manager.clone();
		let processor = processor.clone();

		handles.push(tokio::spawn(async move {
			let mut is_active = false;

			loop {
				let work = {
					let mut queue = work_queue.lock().unwrap();
					match queue.pop_front() {
						Some(w) => {
							if !is_active {
								active_workers.fetch_add(1, Ordering::SeqCst);
								is_active = true;
							}
							Some(w)
						},
						None => {
							if is_active {
								active_workers.fetch_sub(1, Ordering::SeqCst);
								is_active = false;
							}
							None
						},
					}
				};

				let Some(work) = work else {
					// Prevent creating a busy loop.
					sleep(Duration::from_millis(100)).await;

					let queue_len = work_queue.lock().unwrap().len();
					let active = active_workers.load(Ordering::SeqCst);

					if queue_len == 0 && active == 0 {
						break;
					}
					continue;
				};

				let client = conn_manager.get(worker_index).await;

				match processor(worker_index, work, client.clone()).await {
					ProcessResult::Success { new_work } => {
						if !new_work.is_empty() {
							work_queue.lock().unwrap().extend(new_work);
						}
					},
					ProcessResult::Retry { work, sleep_duration, action } => {
						work_queue.lock().unwrap().push_back(work);

						sleep(sleep_duration).await;

						match action {
							RetryAction::Remove => conn_manager.remove_client(&client).await,
							RetryAction::Recreate => {
								conn_manager.recreate_client(worker_index, client).await
							},
						}
					},
				}
			}
		}));
	}

	futures::future::join_all(handles).await;
}
