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

//! Externalities extension that provides access to the current proof size
//! of the underlying recorder.

use parking_lot::Mutex;

use crate::ProofSizeProvider;
use std::{collections::VecDeque, sync::Arc};

sp_externalities::decl_extension! {
	/// The proof size extension to fetch the current storage proof size
	/// in externalities.
	pub struct ProofSizeExt(Box<dyn ProofSizeProvider + 'static + Sync + Send>);

	impl ProofSizeExt {
		fn start_transaction(&mut self, ty: sp_externalities::TransactionType) {
			self.0.start_transaction(ty.is_host());
		}

		fn rollback_transaction(&mut self, ty: sp_externalities::TransactionType) {
			self.0.rollback_transaction(ty.is_host());
		}

		fn commit_transaction(&mut self, ty: sp_externalities::TransactionType) {
			self.0.commit_transaction(ty.is_host());
		}
	}
}

impl ProofSizeExt {
	/// Creates a new instance of [`ProofSizeExt`].
	pub fn new<T: ProofSizeProvider + Sync + Send + 'static>(recorder: T) -> Self {
		ProofSizeExt(Box::new(recorder))
	}

	/// Returns the storage proof size.
	pub fn storage_proof_size(&self) -> u64 {
		self.0.estimate_encoded_size() as _
	}
}

/// Proof size estimations as recorded by [`RecordingProofSizeProvider`].
///
/// Each item is the estimated proof size as observed when calling
/// [`ProofSizeProvider::estimate_encoded_size`]. The items are ordered by their observation and
/// need to be replayed in the exact same order.
pub struct RecordedProofSizeEstimations(pub VecDeque<usize>);

impl From<Vec<u32>> for RecordedProofSizeEstimations {
	fn from(recordings: Vec<u32>) -> Self {
		Self(recordings.into_iter().map(|x| x as usize).collect())
	}
}

/// Inner structure of [`RecordingProofSizeProvider`].
struct RecordingProofSizeProviderInner {
	inner: Box<dyn ProofSizeProvider + Send + Sync>,
	/// Stores the observed proof estimations (in order of observation) per transaction.
	///
	/// Last element of the outer vector is the active transaction.
	proof_size_estimations: Vec<Vec<usize>>,
}

/// An implementation of [`ProofSizeProvider`] that records the return value of the calls to
/// [`ProofSizeProvider::estimate_encoded_size`].
///
/// Wraps an inner [`ProofSizeProvider`] that is used to get the actual encoded size estimations.
/// Each estimation is recorded in the order it was observed.
#[derive(Clone)]
pub struct RecordingProofSizeProvider {
	inner: Arc<Mutex<RecordingProofSizeProviderInner>>,
}

impl RecordingProofSizeProvider {
	/// Creates a new instance of [`RecordingProofSizeProvider`].
	pub fn new<T: ProofSizeProvider + Sync + Send + 'static>(recorder: T) -> Self {
		Self {
			inner: Arc::new(Mutex::new(RecordingProofSizeProviderInner {
				inner: Box::new(recorder),
				// Init the always existing transaction.
				proof_size_estimations: vec![Vec::new()],
			})),
		}
	}

	/// Returns the recorded estimations returned by each call to
	/// [`Self::estimate_encoded_size`].
	pub fn recorded_estimations(&self) -> Vec<usize> {
		self.inner.lock().proof_size_estimations.iter().flatten().copied().collect()
	}
}

impl ProofSizeProvider for RecordingProofSizeProvider {
	fn estimate_encoded_size(&self) -> usize {
		let mut inner = self.inner.lock();

		let estimation = inner.inner.estimate_encoded_size();

		inner
			.proof_size_estimations
			.last_mut()
			.expect("There is always at least one transaction open; qed")
			.push(estimation);

		estimation
	}

	fn start_transaction(&mut self, is_host: bool) {
		// We don't care about runtime transactions, because they are part of the consensus critical
		// path, that will always deterministically call this code.
		//
		// For example a runtime execution is creating 10 runtime transaction and calling in every
		// transaction the proof size estimation host function and 8 of these transactions are
		// rolled back. We need to keep all the 10 estimations. When the runtime execution is
		// replayed (by e.g. importing a block), we will deterministically again create 10 runtime
		// executions and roll back 8. However, in between we require all 10 estimations as
		// otherwise the execution would not be deterministically anymore.
		//
		// A host transaction is only rolled back while for example building a block and an
		// extrinsic failed in the early checks in the runtime. In this case, the extrinsic will
		// also never appear in a block and thus, will not need to be replayed later on.
		if is_host {
			self.inner.lock().proof_size_estimations.push(Default::default());
		}
	}

	fn rollback_transaction(&mut self, is_host: bool) {
		let mut inner = self.inner.lock();

		// The host side transaction needs to be reverted, because this is only done when an
		// entire execution is rolled back. So, the execution will never be part of the consensus
		// critical path.
		if is_host && inner.proof_size_estimations.len() > 1 {
			inner.proof_size_estimations.pop();
		}
	}

	fn commit_transaction(&mut self, is_host: bool) {
		let mut inner = self.inner.lock();

		if is_host && inner.proof_size_estimations.len() > 1 {
			let last = inner
				.proof_size_estimations
				.pop()
				.expect("There are more than one element in the vector; qed");

			inner
				.proof_size_estimations
				.last_mut()
				.expect("There are more than one element in the vector; qed")
				.extend(last);
		}
	}
}

/// An implementation of [`ProofSizeProvider`] that replays estimations recorded by
/// [`RecordingProofSizeProvider`].
///
/// The recorded estimations are removed as they are required by calls to
/// [`Self::estimate_encoded_size`]. Will return `0` when all estimations are consumed.
pub struct ReplayProofSizeProvider(Arc<Mutex<RecordedProofSizeEstimations>>);

impl ReplayProofSizeProvider {
	/// Creates a new instance from the given [`RecordedProofSizeEstimations`].
	pub fn from_recorded(recorded: RecordedProofSizeEstimations) -> Self {
		Self(Arc::new(Mutex::new(recorded)))
	}
}

impl From<RecordedProofSizeEstimations> for ReplayProofSizeProvider {
	fn from(value: RecordedProofSizeEstimations) -> Self {
		Self::from_recorded(value)
	}
}

impl ProofSizeProvider for ReplayProofSizeProvider {
	fn estimate_encoded_size(&self) -> usize {
		self.0.lock().0.pop_front().unwrap_or_default()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicUsize, Ordering};

	// Mock ProofSizeProvider for testing
	#[derive(Clone)]
	struct MockProofSizeProvider {
		size: Arc<AtomicUsize>,
	}

	impl MockProofSizeProvider {
		fn new(initial_size: usize) -> Self {
			Self { size: Arc::new(AtomicUsize::new(initial_size)) }
		}

		fn set_size(&self, new_size: usize) {
			self.size.store(new_size, Ordering::Relaxed);
		}
	}

	impl ProofSizeProvider for MockProofSizeProvider {
		fn estimate_encoded_size(&self) -> usize {
			self.size.load(Ordering::Relaxed)
		}

		fn start_transaction(&mut self, _is_host: bool) {}
		fn rollback_transaction(&mut self, _is_host: bool) {}
		fn commit_transaction(&mut self, _is_host: bool) {}
	}

	#[test]
	fn recording_proof_size_provider_basic_functionality() {
		let mock = MockProofSizeProvider::new(100);
		let tracker = RecordingProofSizeProvider::new(mock.clone());

		// Initial state - no estimations recorded yet
		assert_eq!(tracker.recorded_estimations(), Vec::<usize>::new());

		// Call estimate_encoded_size and verify it's recorded
		let size = tracker.estimate_encoded_size();
		assert_eq!(size, 100);
		assert_eq!(tracker.recorded_estimations(), vec![100]);

		// Change the mock size and call again
		mock.set_size(200);
		let size = tracker.estimate_encoded_size();
		assert_eq!(size, 200);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200]);

		// Multiple calls with same size
		let size = tracker.estimate_encoded_size();
		assert_eq!(size, 200);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200, 200]);
	}

	#[test]
	fn recording_proof_size_provider_host_transactions() {
		let mock = MockProofSizeProvider::new(100);
		let mut tracker = RecordingProofSizeProvider::new(mock.clone());

		// Record some estimations in the initial transaction
		tracker.estimate_encoded_size();
		tracker.estimate_encoded_size();
		assert_eq!(tracker.recorded_estimations(), vec![100, 100]);

		// Start a host transaction
		tracker.start_transaction(true);
		mock.set_size(200);
		tracker.estimate_encoded_size();

		// Should have 3 estimations total
		assert_eq!(tracker.recorded_estimations(), vec![100, 100, 200]);

		// Commit the host transaction
		tracker.commit_transaction(true);

		// All estimations should still be there
		assert_eq!(tracker.recorded_estimations(), vec![100, 100, 200]);

		// Add more estimations
		mock.set_size(300);
		tracker.estimate_encoded_size();
		assert_eq!(tracker.recorded_estimations(), vec![100, 100, 200, 300]);
	}

	#[test]
	fn recording_proof_size_provider_host_transaction_rollback() {
		let mock = MockProofSizeProvider::new(100);
		let mut tracker = RecordingProofSizeProvider::new(mock.clone());

		// Record some estimations in the initial transaction
		tracker.estimate_encoded_size();
		assert_eq!(tracker.recorded_estimations(), vec![100]);

		// Start a host transaction
		tracker.start_transaction(true);
		mock.set_size(200);
		tracker.estimate_encoded_size();
		tracker.estimate_encoded_size();

		// Should have 3 estimations total
		assert_eq!(tracker.recorded_estimations(), vec![100, 200, 200]);

		// Rollback the host transaction
		tracker.rollback_transaction(true);

		// Should only have the original estimation
		assert_eq!(tracker.recorded_estimations(), vec![100]);
	}

	#[test]
	fn recording_proof_size_provider_runtime_transactions_ignored() {
		let mock = MockProofSizeProvider::new(100);
		let mut tracker = RecordingProofSizeProvider::new(mock.clone());

		// Record initial estimation
		tracker.estimate_encoded_size();
		assert_eq!(tracker.recorded_estimations(), vec![100]);

		// Start a runtime transaction (is_host = false)
		tracker.start_transaction(false);
		mock.set_size(200);
		tracker.estimate_encoded_size();

		// Should have both estimations
		assert_eq!(tracker.recorded_estimations(), vec![100, 200]);

		// Commit runtime transaction - should not affect recording
		tracker.commit_transaction(false);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200]);

		// Rollback runtime transaction - should not affect recording
		tracker.rollback_transaction(false);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200]);
	}

	#[test]
	fn recording_proof_size_provider_nested_host_transactions() {
		let mock = MockProofSizeProvider::new(100);
		let mut tracker = RecordingProofSizeProvider::new(mock.clone());

		// Initial estimation
		tracker.estimate_encoded_size();
		assert_eq!(tracker.recorded_estimations(), vec![100]);

		// Start first host transaction
		tracker.start_transaction(true);
		mock.set_size(200);
		tracker.estimate_encoded_size();

		// Start nested host transaction
		tracker.start_transaction(true);
		mock.set_size(300);
		tracker.estimate_encoded_size();

		assert_eq!(tracker.recorded_estimations(), vec![100, 200, 300]);

		// Commit nested transaction
		tracker.commit_transaction(true);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200, 300]);

		// Commit outer transaction
		tracker.commit_transaction(true);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200, 300]);
	}

	#[test]
	fn recording_proof_size_provider_nested_host_transaction_rollback() {
		let mock = MockProofSizeProvider::new(100);
		let mut tracker = RecordingProofSizeProvider::new(mock.clone());

		// Initial estimation
		tracker.estimate_encoded_size();

		// Start first host transaction
		tracker.start_transaction(true);
		mock.set_size(200);
		tracker.estimate_encoded_size();

		// Start nested host transaction
		tracker.start_transaction(true);
		mock.set_size(300);
		tracker.estimate_encoded_size();

		assert_eq!(tracker.recorded_estimations(), vec![100, 200, 300]);

		// Rollback nested transaction
		tracker.rollback_transaction(true);
		assert_eq!(tracker.recorded_estimations(), vec![100, 200]);

		// Rollback outer transaction
		tracker.rollback_transaction(true);
		assert_eq!(tracker.recorded_estimations(), vec![100]);
	}

	#[test]
	fn recording_proof_size_provider_rollback_on_base_transaction_does_nothing() {
		let mock = MockProofSizeProvider::new(100);
		let mut tracker = RecordingProofSizeProvider::new(mock.clone());

		// Record some estimations
		tracker.estimate_encoded_size();
		tracker.estimate_encoded_size();
		assert_eq!(tracker.recorded_estimations(), vec![100, 100]);

		// Try to rollback the base transaction - should do nothing
		tracker.rollback_transaction(true);
		assert_eq!(tracker.recorded_estimations(), vec![100, 100]);
	}

	#[test]
	fn recorded_proof_size_estimations_struct() {
		let estimations = vec![100, 200, 300];
		let recorded = RecordedProofSizeEstimations(estimations.into());
		let expected: VecDeque<usize> = vec![100, 200, 300].into();
		assert_eq!(recorded.0, expected);
	}

	#[test]
	fn replay_proof_size_provider_basic_functionality() {
		let estimations = vec![100, 200, 300, 150];
		let recorded = RecordedProofSizeEstimations(estimations.into());
		let replay = ReplayProofSizeProvider::from_recorded(recorded);

		// Should replay estimations in order
		assert_eq!(replay.estimate_encoded_size(), 100);
		assert_eq!(replay.estimate_encoded_size(), 200);
		assert_eq!(replay.estimate_encoded_size(), 300);
		assert_eq!(replay.estimate_encoded_size(), 150);
	}

	#[test]
	fn replay_proof_size_provider_exhausted_returns_zero() {
		let estimations = vec![100, 200];
		let recorded = RecordedProofSizeEstimations(estimations.into());
		let replay = ReplayProofSizeProvider::from_recorded(recorded);

		// Consume all estimations
		assert_eq!(replay.estimate_encoded_size(), 100);
		assert_eq!(replay.estimate_encoded_size(), 200);

		// Should return 0 when exhausted
		assert_eq!(replay.estimate_encoded_size(), 0);
		assert_eq!(replay.estimate_encoded_size(), 0);
	}

	#[test]
	fn replay_proof_size_provider_empty_returns_zero() {
		let recorded = RecordedProofSizeEstimations(VecDeque::new());
		let replay = ReplayProofSizeProvider::from_recorded(recorded);

		// Should return 0 for empty estimations
		assert_eq!(replay.estimate_encoded_size(), 0);
		assert_eq!(replay.estimate_encoded_size(), 0);
	}

	#[test]
	fn replay_proof_size_provider_from_trait() {
		let estimations = vec![42, 84];
		let recorded = RecordedProofSizeEstimations(estimations.into());
		let replay: ReplayProofSizeProvider = recorded.into();

		assert_eq!(replay.estimate_encoded_size(), 42);
		assert_eq!(replay.estimate_encoded_size(), 84);
		assert_eq!(replay.estimate_encoded_size(), 0);
	}

	#[test]
	fn record_and_replay_integration() {
		let mock = MockProofSizeProvider::new(100);
		let recorder = RecordingProofSizeProvider::new(mock.clone());

		// Record some estimations
		recorder.estimate_encoded_size();
		mock.set_size(200);
		recorder.estimate_encoded_size();
		mock.set_size(300);
		recorder.estimate_encoded_size();

		// Get recorded estimations
		let recorded_estimations = recorder.recorded_estimations();
		assert_eq!(recorded_estimations, vec![100, 200, 300]);

		// Create replay provider from recorded estimations
		let recorded = RecordedProofSizeEstimations(recorded_estimations.into());
		let replay = ReplayProofSizeProvider::from_recorded(recorded);

		// Replay should return the same sequence
		assert_eq!(replay.estimate_encoded_size(), 100);
		assert_eq!(replay.estimate_encoded_size(), 200);
		assert_eq!(replay.estimate_encoded_size(), 300);
		assert_eq!(replay.estimate_encoded_size(), 0);
	}

	#[test]
	fn replay_proof_size_provider_single_value() {
		let estimations = vec![42];
		let recorded = RecordedProofSizeEstimations(estimations.into());
		let replay = ReplayProofSizeProvider::from_recorded(recorded);

		// Should return the single value then default to 0
		assert_eq!(replay.estimate_encoded_size(), 42);
		assert_eq!(replay.estimate_encoded_size(), 0);
	}
}
