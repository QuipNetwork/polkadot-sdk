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

//! Contains the logic for executing PVFs. Used by the polkadot-execute-worker binary.

#![deny(unused_crate_dependencies)]
#![warn(missing_docs)]

pub use polkadot_node_core_pvf_common::{
	error::ExecuteError, executor_interface::execute_artifact,
};
use polkadot_parachain_primitives::primitives::ValidationParams;

// NOTE: Initializing logging in e.g. tests will not have an effect in the workers, as they are
//       separate spawned processes. Run with e.g. `RUST_LOG=parachain::pvf-execute-worker=trace`.
const LOG_TARGET: &str = "parachain::pvf-execute-worker";

use codec::{Decode, Encode};
use cpu_time::ProcessTime;
use nix::{
	errno::Errno,
	sys::{
		resource::{Usage, UsageWho},
		wait::WaitStatus,
	},
	unistd::{ForkResult, Pid},
};
use polkadot_node_core_pvf_common::{
	compute_checksum,
	error::InternalValidationError,
	execute::{
		ExecuteRequest, Handshake, JobError, JobResponse, JobResult, WorkerError, WorkerResponse,
	},
	executor_interface::params_to_wasmtime_semantics,
	framed_recv_blocking, framed_send_blocking,
	worker::{
		cpu_time_monitor_loop, get_total_cpu_usage, pipe2_cloexec, recv_child_response, run_worker,
		send_result, stringify_errno, stringify_panic_payload,
		thread::{self, WaitOutcome},
		PipeFd, WorkerInfo, WorkerKind,
	},
	worker_dir,
};
use polkadot_node_primitives::{BlockData, POV_BOMB_LIMIT};
use polkadot_parachain_primitives::primitives::{
	TrailingOption, ValidationParamsExtension, ValidationResult,
};
use polkadot_primitives::{CandidateDescriptorVersion, ExecutorParams};
use std::{
	io::{self, Read},
	os::{
		fd::{AsRawFd, FromRawFd},
		unix::net::UnixStream,
	},
	path::PathBuf,
	process,
	sync::{mpsc::channel, Arc},
	time::Duration,
};

/// The number of threads for the child process:
/// 1 - Main thread
/// 2 - Cpu monitor thread
/// 3 - Execute thread
///
/// NOTE: The correctness of this value is enforced by a test. If the number of threads inside
/// the child process changes in the future, this value must be changed as well.
pub const EXECUTE_WORKER_THREAD_NUMBER: u32 = 3;

/// Receives a handshake with information specific to the execute worker.
fn recv_execute_handshake(stream: &mut UnixStream) -> io::Result<Handshake> {
	let handshake_enc = framed_recv_blocking(stream)?;
	let handshake = Handshake::decode(&mut &handshake_enc[..]).map_err(|_| {
		io::Error::new(
			io::ErrorKind::Other,
			"execute pvf recv_execute_handshake: failed to decode Handshake".to_owned(),
		)
	})?;
	Ok(handshake)
}

fn recv_request(stream: &mut UnixStream) -> io::Result<ExecuteRequest> {
	let request_bytes = framed_recv_blocking(stream)?;
	let request = ExecuteRequest::decode(&mut &request_bytes[..]).map_err(|_| {
		io::Error::new(
			io::ErrorKind::Other,
			"execute pvf recv_request: failed to decode ExecuteRequest".to_string(),
		)
	})?;

	Ok(request)
}

/// Sends an error to the host and returns the original error wrapped in `io::Error`.
macro_rules! map_and_send_err {
	($error:expr, $err_constructor:expr, $stream:expr, $worker_info:expr) => {{
		let err: WorkerError = $err_constructor($error.to_string()).into();
		let io_err = io::Error::new(io::ErrorKind::Other, err.to_string());
		let _ = send_result::<WorkerResponse, WorkerError>($stream, Err(err), $worker_info);
		io_err
	}};
}

/// The entrypoint that the spawned execute worker should start with.
///
/// # Parameters
///
/// - `socket_path`: specifies the path to the socket used to communicate with the host.
///
/// - `worker_dir_path`: specifies the path to the worker-specific temporary directory.
///
/// - `node_version`: if `Some`, is checked against the `worker_version`. A mismatch results in
///   immediate worker termination. `None` is used for tests and in other situations when version
///   check is not necessary.
///
/// - `worker_version`: see above
pub fn worker_entrypoint(
	socket_path: PathBuf,
	worker_dir_path: PathBuf,
	node_version: Option<&str>,
	worker_version: Option<&str>,
) {
	run_worker(
		WorkerKind::Execute,
		socket_path,
		worker_dir_path,
		node_version,
		worker_version,
		|mut stream, worker_info, security_status| {
			let artifact_path = worker_dir::execute_artifact(&worker_info.worker_dir_path);

			let Handshake { executor_params } =
				recv_execute_handshake(&mut stream).map_err(|e| {
					map_and_send_err!(
						e,
						InternalValidationError::HostCommunication,
						&mut stream,
						worker_info
					)
				})?;

			let executor_params: Arc<ExecutorParams> = Arc::new(executor_params);
			let execute_thread_stack_size = max_stack_size(&executor_params);

			loop {
				let request = recv_request(&mut stream).map_err(|e| {
					map_and_send_err!(
						e,
						InternalValidationError::HostCommunication,
						&mut stream,
						worker_info
					)
				})?;

				let pvd = request.pvd;
				let pov = request.pov;
				let execution_timeout = request.execution_timeout;
				let artifact_checksum = request.artifact_checksum;
				gum::debug!(
					target: LOG_TARGET,
					?worker_info,
					?security_status,
					"worker: validating artifact {}",
					artifact_path.display(),
				);

				// Get the artifact bytes.
				let compiled_artifact_blob = std::fs::read(&artifact_path).map_err(|e| {
					map_and_send_err!(
						e,
						InternalValidationError::CouldNotOpenFile,
						&mut stream,
						worker_info
					)
				})?;

				if artifact_checksum != compute_checksum(&compiled_artifact_blob) {
					send_result::<WorkerResponse, WorkerError>(
						&mut stream,
						Ok(WorkerResponse {
							job_response: JobResponse::CorruptedArtifact,
							duration: Duration::ZERO,
							pov_size: 0,
						}),
						worker_info,
					)?;
					continue;
				}

				let (pipe_read_fd, pipe_write_fd) = pipe2_cloexec().map_err(|e| {
					map_and_send_err!(
						e,
						InternalValidationError::CouldNotCreatePipe,
						&mut stream,
						worker_info
					)
				})?;

				let usage_before = nix::sys::resource::getrusage(UsageWho::RUSAGE_CHILDREN)
					.map_err(|errno| {
						let e = stringify_errno("getrusage before", errno);
						map_and_send_err!(
							e,
							InternalValidationError::Kernel,
							&mut stream,
							worker_info
						)
					})?;
				let stream_fd = stream.as_raw_fd();

				let compiled_artifact_blob = Arc::new(compiled_artifact_blob);

				let raw_block_data =
					match sp_maybe_compressed_blob::decompress(&pov.block_data.0, POV_BOMB_LIMIT) {
						Ok(data) => data,
						Err(_) => {
							send_result::<WorkerResponse, WorkerError>(
								&mut stream,
								Ok(WorkerResponse {
									job_response: JobResponse::PoVDecompressionFailure,
									duration: Duration::ZERO,
									pov_size: 0,
								}),
								worker_info,
							)?;
							continue;
						},
					};

				let pov_size = raw_block_data.len() as u32;

				let params = ValidationParams {
					parent_head: pvd.parent_head.clone(),
					block_data: BlockData(raw_block_data.to_vec()),
					relay_parent_number: pvd.relay_parent_number,
					relay_parent_storage_root: pvd.relay_parent_storage_root,
				};
				let mut encoded_params = params.encode();

				// Append V3+ extension based on descriptor version.
				// SAFETY: ValidationParams is the complete message passed to the PVF.
				// TrailingOption is safe here because:
				// 1. ValidationParams is not embedded in any larger struct
				// 2. The extension bytes are the ONLY thing after ValidationParams
				// 3. The PVF will decode ValidationParams + optional extension as the entire input
				let extension: TrailingOption<ValidationParamsExtension> =
					match request.descriptor_version {
						CandidateDescriptorVersion::V3 => {
							// V3 candidate - append extension with both parent hashes
							TrailingOption(Some(ValidationParamsExtension::V3 {
								relay_parent: request.relay_parent,
								scheduling_parent: request.scheduling_parent,
							}))
						},
						CandidateDescriptorVersion::V1 |
						CandidateDescriptorVersion::V2 |
						CandidateDescriptorVersion::Unknown(_) => {
							// V1/V2/Unknown - no extension appended
							TrailingOption(None)
						},
					};
				encoded_params.extend(extension.encode());

				let params = Arc::new(encoded_params);

				cfg_if::cfg_if! {
					if #[cfg(target_os = "linux")] {
						let result = if security_status.can_do_secure_clone {
							handle_clone(
								pipe_write_fd,
								pipe_read_fd,
								stream_fd,
								&compiled_artifact_blob,
								&executor_params,
								&params,
								execution_timeout,
								execute_thread_stack_size,
								worker_info,
								security_status.can_unshare_user_namespace_and_change_root,
								usage_before,
								pov_size,
							)?
						} else {
							// Fall back to using fork.
							handle_fork(
								pipe_write_fd,
								pipe_read_fd,
								stream_fd,
								&compiled_artifact_blob,
								&executor_params,
								&params,
								execution_timeout,
								execute_thread_stack_size,
								worker_info,
								usage_before,
								pov_size,
							)?
						};
					} else {
						let result = handle_fork(
							pipe_write_fd,
							pipe_read_fd,
							stream_fd,
							&compiled_artifact_blob,
							&executor_params,
							&params,
							execution_timeout,
							execute_thread_stack_size,
							worker_info,
							usage_before,
							pov_size,
						)?;
					}
				}

				gum::trace!(
					target: LOG_TARGET,
					?worker_info,
					"worker: sending result to host: {:?}",
					result
				);
				send_result(&mut stream, result, worker_info)?;
			}
		},
	);
}

fn validate_using_artifact(
	compiled_artifact_blob: &[u8],
	executor_params: &ExecutorParams,
	params: &[u8],
) -> JobResponse {
	let descriptor_bytes = match unsafe {
		// SAFETY: this should be safe since the compiled artifact passed here comes from the
		//         file created by the prepare workers. These files are obtained by calling
		//         [`executor_interface::prepare`].
		execute_artifact(compiled_artifact_blob, executor_params, params)
	} {
		Err(ExecuteError::RuntimeConstruction(wasmerr)) => {
			return JobResponse::runtime_construction("execute", &wasmerr.to_string())
		},
		Err(err) => return JobResponse::format_invalid("execute", &err.to_string()),
		Ok(d) => d,
	};

	let result_descriptor = match ValidationResult::decode(&mut &descriptor_bytes[..]) {
		Err(err) => {
			return JobResponse::format_invalid(
				"validation result decoding failed",
				&err.to_string(),
			)
		},
		Ok(r) => r,
	};

	JobResponse::Ok { result_descriptor }
}

#[cfg(target_os = "linux")]
fn handle_clone(
	pipe_write_fd: i32,
	pipe_read_fd: i32,
	stream_fd: i32,
	compiled_artifact_blob: &Arc<Vec<u8>>,
	executor_params: &Arc<ExecutorParams>,
	params: &Arc<Vec<u8>>,
	execution_timeout: Duration,
	execute_stack_size: usize,
	worker_info: &WorkerInfo,
	have_unshare_newuser: bool,
	usage_before: Usage,
	pov_size: u32,
) -> io::Result<Result<WorkerResponse, WorkerError>> {
	use polkadot_node_core_pvf_common::worker::security;

	// SAFETY: new process is spawned within a single threaded process. This invariant
	// is enforced by tests. Stack size being specified to ensure child doesn't overflow
	match unsafe {
		security::clone::clone_on_worker(
			worker_info,
			have_unshare_newuser,
			Box::new(|| {
				handle_child_process(
					pipe_write_fd,
					pipe_read_fd,
					stream_fd,
					Arc::clone(compiled_artifact_blob),
					Arc::clone(executor_params),
					Arc::clone(params),
					execution_timeout,
					execute_stack_size,
				)
			}),
		)
	} {
		Ok(child) => handle_parent_process(
			pipe_read_fd,
			pipe_write_fd,
			worker_info,
			child,
			usage_before,
			pov_size,
			execution_timeout,
		),
		Err(security::clone::Error::Clone(errno)) => {
			Ok(Err(internal_error_from_errno("clone", errno)))
		},
	}
}

fn handle_fork(
	pipe_write_fd: i32,
	pipe_read_fd: i32,
	stream_fd: i32,
	compiled_artifact_blob: &Arc<Vec<u8>>,
	executor_params: &Arc<ExecutorParams>,
	params: &Arc<Vec<u8>>,
	execution_timeout: Duration,
	execute_worker_stack_size: usize,
	worker_info: &WorkerInfo,
	usage_before: Usage,
	pov_size: u32,
) -> io::Result<Result<WorkerResponse, WorkerError>> {
	// SAFETY: new process is spawned within a single threaded process. This invariant
	// is enforced by tests.
	match unsafe { nix::unistd::fork() } {
		Ok(ForkResult::Child) => handle_child_process(
			pipe_write_fd,
			pipe_read_fd,
			stream_fd,
			Arc::clone(compiled_artifact_blob),
			Arc::clone(executor_params),
			Arc::clone(params),
			execution_timeout,
			execute_worker_stack_size,
		),
		Ok(ForkResult::Parent { child }) => handle_parent_process(
			pipe_read_fd,
			pipe_write_fd,
			worker_info,
			child,
			usage_before,
			pov_size,
			execution_timeout,
		),
		Err(errno) => Ok(Err(internal_error_from_errno("fork", errno))),
	}
}

/// This is used to handle child process during pvf execute worker.
/// It executes the artifact and pipes back the response to the parent process.
///
/// # Returns
///
/// - pipe back `JobResponse` to the parent process.
fn handle_child_process(
	pipe_write_fd: i32,
	pipe_read_fd: i32,
	stream_fd: i32,
	compiled_artifact_blob: Arc<Vec<u8>>,
	executor_params: Arc<ExecutorParams>,
	params: Arc<Vec<u8>>,
	execution_timeout: Duration,
	execute_thread_stack_size: usize,
) -> ! {
	// SAFETY: this is an open and owned file descriptor at this point.
	let mut pipe_write = unsafe { PipeFd::from_raw_fd(pipe_write_fd) };

	// Drop the read end so we don't have too many FDs open.
	if let Err(errno) = nix::unistd::close(pipe_read_fd) {
		send_child_response(&mut pipe_write, job_error_from_errno("closing pipe", errno));
	}

	// Dropping the stream closes the underlying socket. We want to make sure
	// that the sandboxed child can't get any kind of information from the
	// outside world. The only IPC it should be able to do is sending its
	// response over the pipe.
	if let Err(errno) = nix::unistd::close(stream_fd) {
		send_child_response(&mut pipe_write, job_error_from_errno("closing stream", errno));
	}

	gum::debug!(
		target: LOG_TARGET,
		worker_job_pid = %process::id(),
		"worker job: executing artifact",
	);

	// Conditional variable to notify us when a thread is done.
	let condvar = thread::get_condvar();
	let cpu_time_start = ProcessTime::now();

	// Spawn a new thread that runs the CPU time monitor.
	let (cpu_time_monitor_tx, cpu_time_monitor_rx) = channel::<()>();
	let cpu_time_monitor_thread = thread::spawn_worker_thread(
		"cpu time monitor thread",
		move || cpu_time_monitor_loop(cpu_time_start, execution_timeout, cpu_time_monitor_rx),
		Arc::clone(&condvar),
		WaitOutcome::TimedOut,
	)
	.unwrap_or_else(|err| {
		send_child_response(&mut pipe_write, Err(JobError::CouldNotSpawnThread(err.to_string())))
	});

	let execute_thread = thread::spawn_worker_thread_with_stack_size(
		"execute thread",
		move || validate_using_artifact(&compiled_artifact_blob, &executor_params, &params),
		Arc::clone(&condvar),
		WaitOutcome::Finished,
		execute_thread_stack_size,
	)
	.unwrap_or_else(|err| {
		send_child_response(&mut pipe_write, Err(JobError::CouldNotSpawnThread(err.to_string())))
	});

	let outcome = thread::wait_for_threads(condvar);

	let response = match outcome {
		WaitOutcome::Finished => {
			let _ = cpu_time_monitor_tx.send(());
			execute_thread.join().map_err(|e| JobError::Panic(stringify_panic_payload(e)))
		},
		// If the CPU thread is not selected, we signal it to end, the join handle is
		// dropped and the thread will finish in the background.
		WaitOutcome::TimedOut => match cpu_time_monitor_thread.join() {
			Ok(Some(_cpu_time_elapsed)) => Err(JobError::TimedOut),
			Ok(None) => Err(JobError::CpuTimeMonitorThread(
				"error communicating over finished channel".into(),
			)),
			Err(e) => Err(JobError::CpuTimeMonitorThread(stringify_panic_payload(e))),
		},
		WaitOutcome::Pending => {
			unreachable!("we run wait_while until the outcome is no longer pending; qed")
		},
	};

	send_child_response(&mut pipe_write, response);
}

/// Returns stack size based on the number of threads.
/// The stack size is represented by 2MiB * number_of_threads + native stack;
///
/// # Background
///
/// Wasmtime powers the Substrate Executor. It compiles the wasm bytecode into native code.
/// That native code does not create any stacks and just reuses the stack of the thread that
/// wasmtime was invoked from.
///
/// Also, we configure the executor to provide the deterministic stack and that requires
/// supplying the amount of the native stack space that wasm is allowed to use. This is
/// realized by supplying the limit into `wasmtime::Config::max_wasm_stack`.
///
/// There are quirks to that configuration knob:
///
/// 1. It only limits the amount of stack space consumed by wasm but does not ensure nor check that
///    the stack space is actually available.
///
///    That means, if the calling thread has 1 MiB of stack space left and the wasm code consumes
///    more, then the wasmtime limit will **not** trigger. Instead, the wasm code will hit the
///    guard page and the Rust stack overflow handler will be triggered. That leads to an
///    **abort**.
///
/// 2. It cannot and does not limit the stack space consumed by Rust code.
///
///    Meaning that if the wasm code leaves no stack space for Rust code, then the Rust code
///    will abort and that will abort the process as well.
///
/// Typically on Linux the main thread gets the stack size specified by the `ulimit` and
/// typically it's configured to 8 MiB. Rust's spawned threads are 2 MiB. OTOH, the
/// DEFAULT_NATIVE_STACK_MAX is set to 256 MiB. Not nearly enough.
///
/// Hence we need to increase it. The simplest way to fix that is to spawn an execute thread with
/// the desired stack limit. We must also make sure the job process has enough stack for *all* its
/// threads. This function can be used to get the stack size of either the execute thread or execute
/// job process.
fn max_stack_size(executor_params: &ExecutorParams) -> usize {
	let (_sem, deterministic_stack_limit) = params_to_wasmtime_semantics(executor_params);
	return (2 * 1024 * 1024 + deterministic_stack_limit.native_stack_max) as usize;
}

/// Waits for child process to finish and handle child response from pipe.
///
/// # Returns
///
/// - The response, either `Ok` or some error state.
fn handle_parent_process(
	pipe_read_fd: i32,
	pipe_write_fd: i32,
	worker_info: &WorkerInfo,
	job_pid: Pid,
	usage_before: Usage,
	pov_size: u32,
	timeout: Duration,
) -> io::Result<Result<WorkerResponse, WorkerError>> {
	// the read end will wait until all write ends have been closed,
	// this drop is necessary to avoid deadlock
	if let Err(errno) = nix::unistd::close(pipe_write_fd) {
		return Ok(Err(internal_error_from_errno("closing pipe write fd", errno)));
	};

	// SAFETY: pipe_read_fd is an open and owned file descriptor at this point.
	let mut pipe_read = unsafe { PipeFd::from_raw_fd(pipe_read_fd) };

	// Read from the child. Don't decode unless the process exited normally, which we check later.
	let mut received_data = Vec::new();
	pipe_read
		.read_to_end(&mut received_data)
		// Could not decode job response. There is either a bug or the job was hijacked.
		// Should retry at any rate.
		.map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;

	let status = nix::sys::wait::waitpid(job_pid, None);
	gum::trace!(
		target: LOG_TARGET,
		?worker_info,
		%job_pid,
		"execute worker received wait status from job: {:?}",
		status,
	);

	let usage_after = match nix::sys::resource::getrusage(UsageWho::RUSAGE_CHILDREN) {
		Ok(usage) => usage,
		Err(errno) => return Ok(Err(internal_error_from_errno("getrusage after", errno))),
	};

	// Using `getrusage` is needed to check whether child has timedout since we cannot rely on
	// child to report its own time.
	// As `getrusage` returns resource usage from all terminated child processes,
	// it is necessary to subtract the usage before the current child process to isolate its cpu
	// time
	let cpu_tv = get_total_cpu_usage(usage_after) - get_total_cpu_usage(usage_before);
	if cpu_tv >= timeout {
		gum::warn!(
			target: LOG_TARGET,
			?worker_info,
			%job_pid,
			"execute job took {}ms cpu time, exceeded execute timeout {}ms",
			cpu_tv.as_millis(),
			timeout.as_millis(),
		);
		return Ok(Err(WorkerError::JobTimedOut));
	}

	match status {
		Ok(WaitStatus::Exited(_, exit_status)) => {
			let mut reader = io::BufReader::new(received_data.as_slice());
			let result = recv_child_response(&mut reader, "execute")?;

			match result {
				Ok(job_response) => {
					// The exit status should have been zero if no error occurred.
					if exit_status != 0 {
						return Ok(Err(WorkerError::JobError(JobError::UnexpectedExitStatus(
							exit_status,
						))));
					}

					Ok(Ok(WorkerResponse { job_response, pov_size, duration: cpu_tv }))
				},
				Err(job_error) => {
					gum::warn!(
						target: LOG_TARGET,
						?worker_info,
						%job_pid,
						"execute job error: {}",
						job_error,
					);
					if matches!(job_error, JobError::TimedOut) {
						Ok(Err(WorkerError::JobTimedOut))
					} else {
						Ok(Err(WorkerError::JobError(job_error.into())))
					}
				},
			}
		},
		// The job was killed by the given signal.
		//
		// The job gets SIGSYS on seccomp violations, but this signal may have been sent for some
		// other reason, so we still need to check for seccomp violations elsewhere.
		Ok(WaitStatus::Signaled(_pid, signal, _core_dump)) => Ok(Err(WorkerError::JobDied {
			err: format!("received signal: {signal:?}"),
			job_pid: job_pid.as_raw(),
		})),
		Err(errno) => Ok(Err(internal_error_from_errno("waitpid", errno))),

		// It is within an attacker's power to send an unexpected exit status. So we cannot treat
		// this as an internal error (which would make us abstain), but must vote against.
		Ok(unexpected_wait_status) => Ok(Err(WorkerError::JobDied {
			err: format!("unexpected status from wait: {unexpected_wait_status:?}"),
			job_pid: job_pid.as_raw(),
		})),
	}
}

/// Write a job response to the pipe and exit process after.
///
/// # Arguments
///
/// - `pipe_write`: A `PipeFd` structure, the writing end of a pipe.
///
/// - `response`: Child process response
fn send_child_response(pipe_write: &mut PipeFd, response: JobResult) -> ! {
	framed_send_blocking(pipe_write, response.encode().as_slice())
		.unwrap_or_else(|_| process::exit(libc::EXIT_FAILURE));

	if response.is_ok() {
		process::exit(libc::EXIT_SUCCESS)
	} else {
		process::exit(libc::EXIT_FAILURE)
	}
}

fn internal_error_from_errno(context: &'static str, errno: Errno) -> WorkerError {
	WorkerError::InternalError(InternalValidationError::Kernel(stringify_errno(context, errno)))
}

fn job_error_from_errno(context: &'static str, errno: Errno) -> JobResult {
	Err(JobError::Kernel(stringify_errno(context, errno)))
}
