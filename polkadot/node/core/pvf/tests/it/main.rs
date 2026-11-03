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

//! General PVF host integration tests checking the functionality of the PVF host itself.

use assert_matches::assert_matches;
#[cfg(all(feature = "ci-only-tests", target_os = "linux"))]
use polkadot_node_core_pvf::SecurityStatus;
use polkadot_node_core_pvf::{
	start, testing::build_workers_and_get_paths, Config, InvalidCandidate, Metrics,
	PossiblyInvalidError, PrepareError, PrepareJobKind, PvfPrepData, ValidationError,
	ValidationHost, JOB_TIMEOUT_WALL_CLOCK_FACTOR,
};
use polkadot_node_core_pvf_common::{
	compute_checksum, execute::ValidationContext, ArtifactChecksum,
};
use polkadot_node_primitives::{PoV, POV_BOMB_LIMIT};
use polkadot_node_subsystem::messages::PvfExecKind;
use polkadot_parachain_primitives::primitives::{BlockData, ValidationResult};
use polkadot_primitives::{
	ExecutorParam, ExecutorParams, Hash, PersistedValidationData, PvfExecKind as RuntimePvfExecKind,
};
use polkadot_primitives_test_helpers::dummy_candidate_receipt;
use sp_core::H256;

const VALIDATION_CODE_BOMB_LIMIT: u32 = 30 * 1024 * 1024;

use std::{io::Write, sync::Arc, time::Duration};
use tokio::sync::Mutex;

mod adder;
#[cfg(target_os = "linux")]
mod process;
mod worker_common;

const TEST_EXECUTION_TIMEOUT: Duration = Duration::from_secs(6);
const TEST_PREPARATION_TIMEOUT: Duration = Duration::from_secs(6);

struct TestHost {
	// Keep a reference to the tempdir as it gets deleted on drop.
	cache_dir: tempfile::TempDir,
	host: Mutex<ValidationHost>,
}

impl TestHost {
	async fn new() -> Self {
		Self::new_with_config(|_| ()).await
	}

	async fn new_with_config<F>(f: F) -> Self
	where
		F: FnOnce(&mut Config),
	{
		let (prepare_worker_path, execute_worker_path) = build_workers_and_get_paths();

		let cache_dir = tempfile::tempdir().unwrap();
		let mut config = Config::new(
			cache_dir.path().to_owned(),
			None,
			false,
			prepare_worker_path,
			execute_worker_path,
			2,
			1,
			2,
		);
		f(&mut config);
		let (host, task) = start(config, Metrics::default()).await.unwrap();
		let _ = tokio::task::spawn(task);
		Self { cache_dir, host: Mutex::new(host) }
	}

	async fn precheck_pvf(
		&self,
		code: &[u8],
		executor_params: ExecutorParams,
	) -> Result<(), PrepareError> {
		let (result_tx, result_rx) = futures::channel::oneshot::channel();

		self.host
			.lock()
			.await
			.precheck_pvf(
				PvfPrepData::from_code(
					code.into(),
					executor_params,
					TEST_PREPARATION_TIMEOUT,
					PrepareJobKind::Prechecking,
					VALIDATION_CODE_BOMB_LIMIT,
				),
				result_tx,
			)
			.await
			.unwrap();
		result_rx.await.unwrap()
	}

	async fn validate_candidate(
		&self,
		code: &[u8],
		pvd: PersistedValidationData,
		pov: PoV,
		executor_params: ExecutorParams,
		relay_parent: Hash,
	) -> Result<ValidationResult, ValidationError> {
		let (result_tx, result_rx) = futures::channel::oneshot::channel();

		let candidate_receipt: polkadot_primitives::CandidateReceiptV2 =
			dummy_candidate_receipt(relay_parent).into();
		let validation_context = ValidationContext {
			candidate_receipt,
			pvd: Arc::new(pvd),
			pov: Arc::new(pov),
			executor_params: executor_params.clone(),
			exec_timeout: TEST_EXECUTION_TIMEOUT,
			v3_seen: false,
		};

		self.host
			.lock()
			.await
			.execute_pvf(
				PvfPrepData::from_code(
					code.into(),
					executor_params,
					TEST_PREPARATION_TIMEOUT,
					PrepareJobKind::Compilation,
					VALIDATION_CODE_BOMB_LIMIT,
				),
				validation_context,
				polkadot_node_core_pvf::Priority::Normal,
				PvfExecKind::Backing(relay_parent),
				result_tx,
			)
			.await
			.unwrap();
		result_rx.await.unwrap()
	}

	async fn replace_artifact_checksum(
		&self,
		checksum: ArtifactChecksum,
		new_checksum: ArtifactChecksum,
	) {
		self.host
			.lock()
			.await
			.replace_artifact_checksum(checksum, new_checksum)
			.await
			.unwrap();
	}

	#[cfg(all(feature = "ci-only-tests", target_os = "linux"))]
	async fn security_status(&self) -> SecurityStatus {
		self.host.lock().await.security_status.clone()
	}
}

#[tokio::test]
async fn prepare_job_terminates_on_timeout() {
	let host = TestHost::new().await;

	let start = std::time::Instant::now();
	let result = host
		.precheck_pvf(rococo_runtime::WASM_BINARY.unwrap(), Default::default())
		.await;

	match result {
		Err(PrepareError::TimedOut) => {},
		r => panic!("{:?}", r),
	}

	let duration = std::time::Instant::now().duration_since(start);
	assert!(duration >= TEST_PREPARATION_TIMEOUT);
	assert!(duration < TEST_PREPARATION_TIMEOUT * JOB_TIMEOUT_WALL_CLOCK_FACTOR);
}

#[tokio::test]
async fn execute_job_terminates_on_timeout() {
	let host = TestHost::new().await;
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	let start = std::time::Instant::now();
	let result = host
		.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd,
			pov,
			Default::default(),
			H256::default(),
		)
		.await;

	match result {
		Err(ValidationError::Invalid(InvalidCandidate::HardTimeout)) => {},
		r => panic!("{:?}", r),
	}

	let duration = std::time::Instant::now().duration_since(start);
	assert!(duration >= TEST_EXECUTION_TIMEOUT);
	assert!(duration < TEST_EXECUTION_TIMEOUT * JOB_TIMEOUT_WALL_CLOCK_FACTOR);
}

#[cfg(feature = "ci-only-tests")]
#[tokio::test]
async fn ensure_parallel_execution() {
	// Run some jobs that do not complete, thus timing out.
	let host = TestHost::new().await;
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };
	let execute_pvf_future_1 = host.validate_candidate(
		test_parachain_halt::wasm_binary_unwrap(),
		pvd.clone(),
		pov.clone(),
		Default::default(),
		H256::default(),
	);
	let execute_pvf_future_2 = host.validate_candidate(
		test_parachain_halt::wasm_binary_unwrap(),
		pvd,
		pov,
		Default::default(),
		H256::default(),
	);

	let start = std::time::Instant::now();
	let (res1, res2) = futures::join!(execute_pvf_future_1, execute_pvf_future_2);
	assert_matches!(
		(res1, res2),
		(
			Err(ValidationError::Invalid(InvalidCandidate::HardTimeout)),
			Err(ValidationError::Invalid(InvalidCandidate::HardTimeout))
		)
	);

	// Total time should be < 2 x TEST_EXECUTION_TIMEOUT (two workers run in parallel).
	let duration = std::time::Instant::now().duration_since(start);
	let max_duration = 2 * TEST_EXECUTION_TIMEOUT;
	assert!(
		duration < max_duration,
		"Expected duration {}ms to be less than {}ms",
		duration.as_millis(),
		max_duration.as_millis()
	);
}

#[tokio::test]
async fn execute_queue_doesnt_stall_if_workers_died() {
	let host = TestHost::new_with_config(|cfg| {
		cfg.execute_workers_max_num = 5;
	})
	.await;
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	// Here we spawn 8 validation jobs for the `halt` PVF and share those between 5 workers. The
	// first five jobs should timeout and the workers killed. For the next 3 jobs a new batch of
	// workers should be spun up.
	let start = std::time::Instant::now();
	futures::future::join_all((0u8..=8).map(|_| {
		host.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd.clone(),
			pov.clone(),
			Default::default(),
			H256::default(),
		)
	}))
	.await;

	// Total time should be >= 2 x TEST_EXECUTION_TIMEOUT (two separate sets of workers that should
	// both timeout).
	let duration = std::time::Instant::now().duration_since(start);
	let max_duration = 2 * TEST_EXECUTION_TIMEOUT;
	assert!(
		duration >= max_duration,
		"Expected duration {}ms to be greater than or equal to {}ms",
		duration.as_millis(),
		max_duration.as_millis()
	);
}

#[cfg(feature = "ci-only-tests")]
#[tokio::test]
async fn execute_queue_doesnt_stall_with_varying_executor_params() {
	let host = TestHost::new_with_config(|cfg| {
		cfg.execute_workers_max_num = 2;
	})
	.await;
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	let executor_params_1 = ExecutorParams::default();
	let executor_params_2 = ExecutorParams::from(&[ExecutorParam::StackLogicalMax(1024)][..]);

	// Here we spawn 6 validation jobs for the `halt` PVF and share those between 2 workers. Every
	// 3rd job will have different set of executor parameters. All the workers should be killed
	// and in this case the queue should respawn new workers with needed executor environment
	// without waiting. The jobs will be executed in 3 batches, each running two jobs in parallel,
	// and execution time would be roughly 3 * TEST_EXECUTION_TIMEOUT
	let start = std::time::Instant::now();
	futures::future::join_all((0u8..6).map(|i| {
		host.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd.clone(),
			pov.clone(),
			match i % 3 {
				0 => executor_params_1.clone(),
				_ => executor_params_2.clone(),
			},
			H256::default(),
		)
	}))
	.await;

	let duration = std::time::Instant::now().duration_since(start);
	let min_duration = 3 * TEST_EXECUTION_TIMEOUT;
	let max_duration = 4 * TEST_EXECUTION_TIMEOUT;
	assert!(
		duration >= min_duration,
		"Expected duration {}ms to be greater than or equal to {}ms",
		duration.as_millis(),
		min_duration.as_millis()
	);
	assert!(
		duration <= max_duration,
		"Expected duration {}ms to be less than or equal to {}ms",
		duration.as_millis(),
		max_duration.as_millis()
	);
}

// Test that deleting a prepared artifact does not lead to a dispute when we try to execute it.
#[tokio::test]
async fn deleting_prepared_artifact_does_not_dispute() {
	let host = TestHost::new().await;
	let cache_dir = host.cache_dir.path();
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// Manually delete the prepared artifact from disk. The in-memory artifacts table won't change.
	{
		// Get the artifact path (asserting it exists).
		let mut cache_dir: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();
		// Should contain the artifact and the worker dir.
		assert_eq!(cache_dir.len(), 2);
		let mut artifact_path = cache_dir.pop().unwrap().unwrap();
		if artifact_path.path().is_dir() {
			artifact_path = cache_dir.pop().unwrap().unwrap();
		}

		// Delete the artifact.
		std::fs::remove_file(artifact_path.path()).unwrap();
	}

	// Try to validate, artifact should get recreated.
	let result = host
		.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd,
			pov,
			Default::default(),
			H256::default(),
		)
		.await;

	assert_matches!(result, Err(ValidationError::Invalid(InvalidCandidate::HardTimeout)));
}

// Test that corruption of a prepared artifact due to disk issues does not lead to a dispute when we
// try to execute it.
#[tokio::test]
async fn corrupted_on_disk_prepared_artifact_does_not_dispute() {
	let host = TestHost::new().await;
	let cache_dir = host.cache_dir.path();
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// Manually corrupting the prepared artifact from disk. The in-memory artifacts table won't
	// change.
	let artifact_path = {
		// Get the artifact path (asserting it exists).
		let mut cache_dir: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();
		// Should contain the artifact and the worker dir.
		assert_eq!(cache_dir.len(), 2);
		let mut artifact_path = cache_dir.pop().unwrap().unwrap();
		if artifact_path.path().is_dir() {
			artifact_path = cache_dir.pop().unwrap().unwrap();
		}

		// Corrupt the artifact.
		let mut f = std::fs::OpenOptions::new()
			.write(true)
			.truncate(true)
			.open(artifact_path.path())
			.unwrap();
		f.write_all(b"corrupted wasm").unwrap();
		f.flush().unwrap();
		artifact_path
	};

	assert!(artifact_path.path().exists());

	// Try to validate, artifact should get removed because of the corruption.
	let result = host
		.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd,
			pov,
			Default::default(),
			H256::default(),
		)
		.await;

	assert_matches!(
		result,
		Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::CorruptedArtifact))
	);

	// because of CorruptedArtifact we may retry
	host.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// The actual artifact removal is done concurrently
	// with sending of the result of the execution
	// it is not a problem for further re-preparation as
	// artifact filenames are random
	for _ in 1..5 {
		if !artifact_path.path().exists() {
			break;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}

	assert!(
		!artifact_path.path().exists(),
		"the corrupted artifact ({}) should be deleted by the host",
		artifact_path.path().display()
	);
}

// Test that corruption of a prepared artifact does not lead to a dispute when we try to execute it.
#[tokio::test]
async fn corrupted_prepared_artifact_does_not_dispute() {
	let host = TestHost::new().await;
	let cache_dir = host.cache_dir.path();
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// Manually corrupting the prepared artifact from disk. The in-memory artifacts table won't
	// change.
	let (artifact_path, checksum, new_checksum) = {
		// Get the artifact path (asserting it exists).
		let mut cache_dir: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();
		// Should contain the artifact and the worker dir.
		assert_eq!(cache_dir.len(), 2);
		let mut artifact_path = cache_dir.pop().unwrap().unwrap();
		if artifact_path.path().is_dir() {
			artifact_path = cache_dir.pop().unwrap().unwrap();
		}

		let checksum =
			compute_checksum(&std::fs::read(artifact_path.path()).expect("artifact exists"));
		let new_artifact = b"corrupted wasm";
		let new_checksum = compute_checksum(new_artifact);

		// Corrupt the artifact.
		let mut f = std::fs::OpenOptions::new()
			.write(true)
			.truncate(true)
			.open(artifact_path.path())
			.unwrap();
		f.write_all(new_artifact).unwrap();
		f.flush().unwrap();
		(artifact_path, checksum, new_checksum)
	};

	assert!(artifact_path.path().exists());

	host.replace_artifact_checksum(checksum, new_checksum).await;

	// Try to validate, artifact should get removed because of the corruption.
	let result = host
		.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd,
			pov,
			Default::default(),
			H256::default(),
		)
		.await;

	assert_matches!(
		result,
		Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::RuntimeConstruction(_)))
	);

	// because of RuntimeConstruction we may retry
	host.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// The actual artifact removal is done concurrently
	// with sending of the result of the execution
	// it is not a problem for further re-preparation as
	// artifact filenames are random
	for _ in 1..5 {
		if !artifact_path.path().exists() {
			break;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}

	assert!(
		!artifact_path.path().exists(),
		"the corrupted artifact ({}) should be deleted by the host",
		artifact_path.path().display()
	);
}

#[tokio::test]
async fn cache_cleared_on_startup() {
	// Don't drop this host, it owns the `TempDir` which gets cleared on drop.
	let host = TestHost::new().await;

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// The cache dir should contain one artifact and one worker dir.
	let cache_dir = host.cache_dir.path().to_owned();
	assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 2);

	// Start a new host, previous artifact should be cleared.
	let _host = TestHost::new_with_config(|cfg| {
		cfg.cache_path = cache_dir.clone();
	})
	.await;
	assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 0);
}

// This test checks if the adder parachain runtime can be prepared with 10Mb preparation memory
// limit enforced. At the moment of writing, the limit if far enough to prepare the PVF. If it
// starts failing, either Wasmtime version has changed, or the PVF code itself has changed, and
// more memory is required now. Multi-threaded preparation, if ever enabled, may also affect
// memory consumption.
#[tokio::test]
async fn prechecking_within_memory_limits() {
	let host = TestHost::new().await;
	let result = host
		.precheck_pvf(
			::test_parachain_adder::wasm_binary_unwrap(),
			ExecutorParams::from(&[ExecutorParam::PrecheckingMaxMemory(10 * 1024 * 1024)][..]),
		)
		.await;

	assert_matches!(result, Ok(_));
}

// This test checks if the adder parachain runtime can be prepared with 512Kb preparation memory
// limit enforced. At the moment of writing, the limit if not enough to prepare the PVF, and the
// preparation is supposed to generate an error. If the test starts failing, either Wasmtime
// version has changed, or the PVF code itself has changed, and less memory is required now.
#[tokio::test]
async fn prechecking_out_of_memory() {
	use polkadot_node_core_pvf::PrepareError;

	let host = TestHost::new().await;
	let result = host
		.precheck_pvf(
			::test_parachain_adder::wasm_binary_unwrap(),
			ExecutorParams::from(&[ExecutorParam::PrecheckingMaxMemory(512 * 1024)][..]),
		)
		.await;

	assert_matches!(result, Err(PrepareError::OutOfMemory));
}

// With one worker, run multiple preparation jobs serially. They should not conflict.
#[tokio::test]
async fn prepare_can_run_serially() {
	let host = TestHost::new_with_config(|cfg| {
		cfg.prepare_workers_hard_max_num = 1;
	})
	.await;

	let _stats = host
		.precheck_pvf(::test_parachain_adder::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();

	// Prepare a different wasm blob to prevent skipping work.
	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();
}

// CI machines should be able to enable all the security features.
#[cfg(all(feature = "ci-only-tests", target_os = "linux"))]
#[tokio::test]
async fn all_security_features_work() {
	let can_enable_landlock = {
		let res = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, 0usize, 0usize, 1u32) };
		if res == -1 {
			let err = std::io::Error::last_os_error().raw_os_error().unwrap();
			if err == libc::ENOSYS {
				false
			} else {
				panic!("Unexpected errno from landlock check: {err}");
			}
		} else {
			true
		}
	};

	let host = TestHost::new().await;

	assert_eq!(
		host.security_status().await,
		SecurityStatus {
			// Disabled in tests to not enforce the presence of security features. This CI-only test
			// is the only one that tests them.
			secure_validator_mode: false,
			can_enable_landlock,
			can_enable_seccomp: true,
			can_unshare_user_namespace_and_change_root: true,
			can_do_secure_clone: true,
		}
	);
}

// Regression test to make sure the unshare-pivot-root capability does not depend on the PVF
// artifacts cache existing.
#[cfg(all(feature = "ci-only-tests", target_os = "linux"))]
#[tokio::test]
async fn nonexistent_cache_dir() {
	let host = TestHost::new_with_config(|cfg| {
		cfg.cache_path = cfg.cache_path.join("nonexistent_cache_dir");
	})
	.await;

	assert!(host.security_status().await.can_unshare_user_namespace_and_change_root);

	let _stats = host
		.precheck_pvf(::test_parachain_adder::wasm_binary_unwrap(), Default::default())
		.await
		.unwrap();
}

// Checks the the artifact is not re-prepared when the executor environment parameters change
// in a way not affecting the preparation
#[tokio::test]
async fn artifact_does_not_reprepare_on_non_meaningful_exec_parameter_change() {
	let host = TestHost::new_with_config(|cfg| {
		cfg.prepare_workers_hard_max_num = 1;
	})
	.await;
	let cache_dir = host.cache_dir.path();

	let set1 = ExecutorParams::default();
	let set2 = ExecutorParams::from(
		&[ExecutorParam::PvfExecTimeout(RuntimePvfExecKind::Backing, 2500)][..],
	);

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), set1)
		.await
		.unwrap();

	let md1 = {
		let mut cache_dir: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();
		assert_eq!(cache_dir.len(), 2);
		let mut artifact_path = cache_dir.pop().unwrap().unwrap();
		if artifact_path.path().is_dir() {
			artifact_path = cache_dir.pop().unwrap().unwrap();
		}
		std::fs::metadata(artifact_path.path()).unwrap()
	};

	// FS times are not monotonical so we wait 2 secs here to be sure that the creation time of the
	// second attifact will be different
	tokio::time::sleep(Duration::from_secs(2)).await;

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), set2)
		.await
		.unwrap();

	let md2 = {
		let mut cache_dir: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();
		assert_eq!(cache_dir.len(), 2);
		let mut artifact_path = cache_dir.pop().unwrap().unwrap();
		if artifact_path.path().is_dir() {
			artifact_path = cache_dir.pop().unwrap().unwrap();
		}
		std::fs::metadata(artifact_path.path()).unwrap()
	};

	assert_eq!(md1.created().unwrap(), md2.created().unwrap());
}

// Checks if the artifact is re-prepared if the re-preparation is needed by the nature of
// the execution environment parameters change
#[tokio::test]
async fn artifact_does_reprepare_on_meaningful_exec_parameter_change() {
	let host = TestHost::new_with_config(|cfg| {
		cfg.prepare_workers_hard_max_num = 1;
	})
	.await;
	let cache_dir = host.cache_dir.path();

	let set1 = ExecutorParams::default();
	let set2 = ExecutorParams::from(&[ExecutorParam::StackLogicalMax(1024)][..]);

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), set1)
		.await
		.unwrap();
	let cache_dir_contents: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();

	assert_eq!(cache_dir_contents.len(), 2);

	let _stats = host
		.precheck_pvf(test_parachain_halt::wasm_binary_unwrap(), set2)
		.await
		.unwrap();
	let cache_dir_contents: Vec<_> = std::fs::read_dir(cache_dir).unwrap().collect();

	assert_eq!(cache_dir_contents.len(), 3); // new artifact has been added
}

// Checks that we cannot prepare oversized compressed code
#[tokio::test]
async fn invalid_compressed_code_fails_prechecking() {
	let host = TestHost::new().await;
	let raw_code = vec![2u8; VALIDATION_CODE_BOMB_LIMIT as usize + 1];
	let validation_code = sp_maybe_compressed_blob::compress_strongly(
		&raw_code,
		VALIDATION_CODE_BOMB_LIMIT as usize + 1,
	)
	.unwrap();

	let res = host.precheck_pvf(&validation_code, Default::default()).await;

	assert_matches!(res, Err(PrepareError::CouldNotDecompressCodeBlob(_)));
}

// Checks that we cannot validate with oversized compressed code
#[tokio::test]
async fn invalid_compressed_code_fails_validation() {
	let host = TestHost::new().await;
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let pov = PoV { block_data: BlockData(Vec::new()) };

	let raw_code = vec![2u8; VALIDATION_CODE_BOMB_LIMIT as usize + 1];
	let validation_code = sp_maybe_compressed_blob::compress_strongly(
		&raw_code,
		VALIDATION_CODE_BOMB_LIMIT as usize + 1,
	)
	.unwrap();

	let result = host
		.validate_candidate(&validation_code, pvd, pov, Default::default(), H256::default())
		.await;

	assert_matches!(
		result,
		Err(ValidationError::Preparation(PrepareError::CouldNotDecompressCodeBlob(_)))
	);
}

// Checks that we cannot validate with an oversized PoV
#[tokio::test]
async fn invalid_compressed_pov_fails_validation() {
	let host = TestHost::new().await;
	let pvd = PersistedValidationData {
		parent_head: Default::default(),
		relay_parent_number: 1u32,
		relay_parent_storage_root: H256::default(),
		max_pov_size: 4096 * 1024,
	};
	let raw_block_data = vec![1u8; POV_BOMB_LIMIT + 1];
	let block_data =
		sp_maybe_compressed_blob::compress_weakly(&raw_block_data, POV_BOMB_LIMIT + 1).unwrap();
	let pov = PoV { block_data: BlockData(block_data) };

	let result = host
		.validate_candidate(
			test_parachain_halt::wasm_binary_unwrap(),
			pvd,
			pov,
			Default::default(),
			H256::default(),
		)
		.await;

	assert_matches!(
		result,
		Err(ValidationError::Invalid(InvalidCandidate::PoVDecompressionFailure))
	);
}
