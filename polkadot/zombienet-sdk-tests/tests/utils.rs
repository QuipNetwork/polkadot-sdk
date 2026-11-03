// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Result};
use cumulus_zombienet_sdk_helpers::assert_para_throughput_with;
use polkadot_primitives::{CandidateDescriptorVersion, Id as ParaId};
use std::{collections::HashMap, ops::Range};
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{dynamic::Value, ext::scale_value::value, tx::dynamic, OnlineClient, PolkadotConfig},
	tx_helper::parachain::{fetch_genesis_header, fetch_validation_code},
	Arg, LocalFileSystem, Network, NetworkConfig, NetworkNode,
};

pub const PARACHAIN_VALIDATOR_METRIC: &str = "polkadot_node_is_parachain_validator";
pub const ACTIVE_VALIDATOR_METRIC: &str = "polkadot_node_is_active_validator";
pub const INTEGRATION_IMAGE_ENV: &str = "ZOMBIENET_INTEGRATION_TEST_IMAGE";
pub const CUMULUS_IMAGE_ENV: &str = "CUMULUS_IMAGE";
pub const COL_IMAGE_ENV: &str = "COL_IMAGE";
pub const MALUS_IMAGE_ENV: &str = "MALUS_IMAGE";
pub const EXPERIMENTAL_COLLATOR_PROTOCOL_ENV: &str = "EXPERIMENTAL_COLLATOR_PROTOCOL";
pub const BLOCK_HEIGHT_FINALIZED_METRIC: &str = "substrate_block_height{status=\"finalized\"}";
pub const APPROVAL_CHECKING_FINALITY_LAG_METRIC: &str =
	"polkadot_parachain_approval_checking_finality_lag";
pub const APPROVAL_NO_SHOWS_TOTAL_METRIC: &str = "polkadot_parachain_approvals_no_shows_total";
pub const DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_COMPLETE_PATTERN: &str =
	"*Data recovery from systematic chunks complete*";
pub const DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_NOT_POSSIBLE_PATTERN: &str =
	"*Data recovery from systematic chunks is not possible*";
pub const DATA_RECOVERY_CHUNKS_PATTERN: &str = "*Data recovery from chunks complete*";
pub const AVAILABILITY_RECOVERY_RECOVERIES_FINISHED: &str =
	"polkadot_parachain_availability_recovery_recoveries_finished{result=\"failure\"}";
pub const NODE_ROLES_METRIC: &str = "node_roles";

pub async fn initialize_network(
	config: NetworkConfig,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	// Spawn network
	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	// Do not terminate network after the test is finished.
	// This is needed for CI to get logs from k8s.
	// Network shall be terminated from CI after logs are downloaded.
	// NOTE! For local execution (native provider) below call has no effect.
	network.detach().await;

	Ok(network)
}

pub fn env_or_default(var: &str, default: &str) -> String {
	std::env::var(var).unwrap_or_else(|_| default.to_string())
}

/// Appends `--experimental-collator-protocol` to the command line arguments list if the
/// `EXPERIMENTAL_COLLATOR_PROTOCOL` env variable is set.
pub fn maybe_enable_experimental_collator_protocol(mut args: Vec<Arg>) -> Vec<Arg> {
	if std::env::var(EXPERIMENTAL_COLLATOR_PROTOCOL_ENV).is_ok() {
		log::info!("Enabling the experimental collator protocol");
		args.push("--experimental-collator-protocol".into());
	}

	args
}

/// Enables the given `node_features` bits at runtime via a single sudo extrinsic.
///
/// All bit indices in `feature_bits` are set to `true` in one batched `set_node_feature` call.
/// The change takes effect after the next session change.
pub async fn enable_node_features(
	client: &OnlineClient<PolkadotConfig>,
	feature_bits: &[u8],
) -> Result<(), anyhow::Error> {
	let calls: Vec<Value> = feature_bits
		.iter()
		.map(|&bit| {
			value! { Configuration(set_node_feature { index: bit, value: true }) }
		})
		.collect();

	let call = dynamic("Sudo", "sudo", vec![value! { Utility(batch { calls: calls }) }]);

	client
		.tx()
		.sign_and_submit_then_watch_default(
			&call,
			&zombienet_sdk::subxt_signer::sr25519::dev::alice(),
		)
		.await?
		.wait_for_finalized_success()
		.await?;

	Ok(())
}

pub async fn check_log_lines(
	validator_nodes: &[&NetworkNode],
	checks: &[(&str, LogLineCountOptions)],
) -> Result<(), anyhow::Error> {
	for (pattern, opts) in checks {
		for validator in validator_nodes {
			let result =
				validator.wait_log_line_count_with_timeout(*pattern, true, opts.clone()).await?;

			assert!(
				result.success(),
				"Can't find a matching line ({pattern}) in node {}",
				validator.name()
			);
		}
		log::info!("All nodes pass the log line match - {pattern}");
	}
	Ok(())
}

pub type MetricCheckSetup<'a> = (&'a str, Box<dyn Fn(f64) -> bool>, u64);
pub async fn check_metrics(
	validator_nodes: &[&NetworkNode],
	metric_checks: &[MetricCheckSetup<'_>],
) -> Result<(), anyhow::Error> {
	for (metric, predicate, timeout) in metric_checks {
		for validator in validator_nodes {
			let res = if *timeout == 0 {
				let res = validator.assert_with(*metric, &predicate).await?;
				if !res {
					Err(anyhow!("target value for metric {metric} doesn't pass the predicate"))
				} else {
					Ok(())
				}
			} else {
				validator.wait_metric_with_timeout(*metric, &predicate, *timeout).await
			};

			res.map_err(|e| anyhow!("node {} check failed ({metric}): {e}", validator.name()))?;
		}
		log::info!("All nodes pass the metric {metric} predicate");
	}

	Ok(())
}

/// Get genesis header and validation code for parachain
pub async fn fetch_header_and_validation_code(
	para_client: &OnlineClient<PolkadotConfig>,
) -> Result<(Vec<u8>, Vec<u8>), anyhow::Error> {
	log::info!("Fetching genesis header and validation code for parachain");
	let genesis_header = fetch_genesis_header(para_client).await?;
	let validation_code = fetch_validation_code(para_client).await?;

	log::info!(
		"Genesis header: {} bytes, Validation code: {} bytes",
		genesis_header.len(),
		validation_code.len()
	);

	Ok((genesis_header, validation_code))
}
/// Creates calls to force register the parachain and add valication code as trusted.
pub fn create_force_register_call(
	genesis_header: &[u8],
	validation_code: &[u8],
	para_id: u32,
	registrar_account: Value,
) -> Vec<Value> {
	let genesis_head_value = Value::from_bytes(genesis_header);
	let validation_code_value = Value::from_bytes(validation_code);

	let force_register_call = value! {
		Registrar(force_register { who: registrar_account, deposit: 0u128, id: para_id, genesis_head: genesis_head_value, validation_code: validation_code_value.clone() })
	};

	let add_trusted_validation_code_call = value! {
		Paras(add_trusted_validation_code { validation_code: validation_code_value })
	};

	let calls = vec![add_trusted_validation_code_call, force_register_call];

	calls
}

/// Metric name for the total number of backing statements signed by a validator.
const SIGNED_STATEMENTS_METRIC: &str =
	"polkadot_parachain_candidate_backing_signed_statements_total";

/// Asserts that a validator node has signed at least one backing statement.
pub async fn assert_validator_backed_candidates(
	node: &NetworkNode,
	timeout_secs: u64,
) -> Result<(), anyhow::Error> {
	node.wait_metric_with_timeout(SIGNED_STATEMENTS_METRIC, |v| v >= 1.0, timeout_secs)
		.await
		.map_err(|e| {
			anyhow!(
				"Validator {} did not sign any backing statements within {timeout_secs}s: {e}",
				node.name()
			)
		})
}

/// Asserts that candidates of the expected version are being backed for the given parachains.
///
/// Waits for the first session change (so that genesis configuration like `node_features` is
/// active), then checks that the number of candidates matching `expected_version` falls within
/// `expected_range` after `max_blocks` relay chain blocks for each para ID.
pub async fn assert_candidates_version(
	relay_client: &OnlineClient<PolkadotConfig>,
	expected_version: CandidateDescriptorVersion,
	expected_ranges: HashMap<ParaId, Range<u32>>,
	max_blocks: u32,
) -> Result<(), anyhow::Error> {
	assert_para_throughput_with(relay_client, max_blocks, expected_ranges, |receipt| {
		let para_id = receipt.descriptor.para_id();
		let version = receipt.descriptor.version();
		log::info!(
			"Para {} candidate backed: version={:?}, \
			 relay_parent={:?}, \
			 session_index={:?}, \
			 scheduling_parent={:?}",
			para_id,
			version,
			receipt.descriptor.relay_parent(),
			receipt.descriptor.session_index(),
			receipt.descriptor.scheduling_parent(),
		);

		if version != expected_version {
			return Err(anyhow!(
				"Para {para_id} candidate has version {:?}, expected {:?}",
				version,
				expected_version,
			));
		}

		if expected_version == CandidateDescriptorVersion::V2 {
			if receipt.descriptor.session_index().is_none() {
				return Err(anyhow!("Para {para_id} V2 candidate has session_index=None",));
			}
			if receipt.descriptor.relay_parent() != receipt.descriptor.scheduling_parent() {
				return Err(anyhow!(
					"Para {para_id} V2 candidate has scheduling_parent={:?} \
					 != relay_parent={:?}",
					receipt.descriptor.scheduling_parent(),
					receipt.descriptor.relay_parent(),
				));
			}
		}

		if expected_version == CandidateDescriptorVersion::V3 {
			if receipt.descriptor.session_index().is_none() {
				return Err(anyhow!("Para {para_id} V3 candidate has session_index=None"));
			}
			if receipt.descriptor.scheduling_session().is_none() {
				return Err(anyhow!("Para {para_id} V3 candidate hash scheduling_session=None"));
			}
		}

		Ok(true)
	})
	.await
}

/// Check if all the nodes are validators (node_roles == 4.0)
pub async fn assert_nodes_are_validators(nodes: &[&NetworkNode]) -> Result<(), anyhow::Error> {
	for node in nodes {
		node.wait_metric_with_timeout(NODE_ROLES_METRIC, |v| v == 4.0, 60u64)
			.await
			.map_err(|e| anyhow!("Validator {} role check failed: {e}", node.name()))?;
	}

	Ok(())
}
