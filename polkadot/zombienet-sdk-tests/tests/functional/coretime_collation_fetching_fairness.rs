// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Setup a network with 4 validators and 2 parachains. Then,
//! assign core 0 to be shared in 3:1 proportion between paras and
//! verify that the block production respect the proportion.

use crate::utils::{
	assert_nodes_are_validators, create_force_register_call, env_or_default,
	fetch_header_and_validation_code, initialize_network,
	maybe_enable_experimental_collator_protocol, COL_IMAGE_ENV, INTEGRATION_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_para_throughput, submit_extrinsic_and_wait_for_finalization_success_with_timeout,
	wait_for_nth_session_change,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{dynamic::Value, ext::scale_value::value, tx},
	subxt_signer::sr25519::dev,
	Arg, NetworkConfig, NetworkConfigBuilder, RegistrationStrategy,
};

// para_id, collator_debug_args
const PARAS: [(u32, &str); 2] =
	[(2000, "-lparachain=debug,parachain::collator-protocol=trace"), (2001, "-lparachain=debug")];

#[tokio::test(flavor = "multi_thread")]
async fn coretime_collation_fetching_fairness_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let validator_nodes = network.relaychain().nodes();
	let relay_node = validator_nodes
		.first()
		.ok_or(anyhow!("Relaychain should have at least one node"))?;
	let relay_client = relay_node.wait_client().await?;

	// Check authority status
	log::info!("Checking validator node roles");
	assert_nodes_are_validators(&validator_nodes).await?;
	log::info!("All validators confirmed as authorities");

	log::info!("Register paras");
	let alice_account = Value::from_bytes(dev::alice().public_key().0);
	let mut calls = vec![];
	for (para_id, _) in PARAS {
		let node = network.get_node(format!("collator-{para_id}"))?;
		let client = node.wait_client().await?;
		let (head, validation_code) = fetch_header_and_validation_code(&client).await?;
		calls = [
			calls,
			create_force_register_call(
				&head[..],
				&validation_code[..],
				para_id,
				alice_account.clone(),
			),
		]
		.concat();
	}

	let sudo_call = tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Utility(batch { calls: calls })
		}],
	);

	let res = submit_extrinsic_and_wait_for_finalization_success_with_timeout(
		&relay_client,
		&sudo_call,
		&dev::alice(),
		600u64,
	)
	.await;
	assert!(res.is_ok(), "Extrinsic failed to finalize: {:?}", res.unwrap_err());
	log::info!("Registration for paras completed");

	log::info!("assign core 0, shared 3:1 between paras.");
	let sudo_call_assign_core = tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Coretime(assign_core { core: 0u32, begin: 0u32, assignment: (
				(Task(2000u32), 43200),
				(Task(2001u32), 14400u16),
			), end_hint: None() })
		}],
	);

	let res = submit_extrinsic_and_wait_for_finalization_success_with_timeout(
		&relay_client,
		&sudo_call_assign_core,
		&dev::alice(),
		600u64,
	)
	.await;
	assert!(res.is_ok(), "Extrinsic failed to finalize: {:?}", res.unwrap_err());
	log::info!("Core 0 assignment shared 3:1 (2000,2001).");

	// Wait 2 sessions for registration/core assignment
	log::info!("Waiting for 2 session boundaries");
	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	wait_for_nth_session_change(&mut blocks_sub, 2).await?;
	log::info!("Session boundaries passed");

	// This check assumes that para 2000 runs slot based collator which respects its claim queue
	// and para 2001 runs lookahead which generates blocks for each relay parent.
	log::info!(
		"Check block production for each para in 12 RC blocks, in 3:1 proportion (2000:2001)"
	);
	assert_para_throughput(
		&relay_client,
		12,
		[(ParaId::from(2000), 6..10), (ParaId::from(2001), 2..5)],
		[],
	)
	.await?;

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(polkadot_image.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				"-lparachain=debug,runtime=debug".into(),
			]))
			.with_genesis_overrides(json!({
				"patch": {
					"configuration": {
						"config": {
							"needed_approvals": 3,
							"scheduler_params": {
								"max_validators_per_core": 4,
								"num_cores": 1
							}
						}
					}
				}
			}))
			.with_default_resources(|r| {
				r.with_limit_memory("4G")
					.with_limit_cpu("2")
					.with_request_memory("2G")
					.with_request_cpu("1")
			})
			.with_node_group(|g| {
				g.with_count(4).with_base_node(|node| {
					node.with_name("validator").with_args(vec![
						"-lparachain=debug,parachain::collator-protocol=trace".into(),
					])
				})
			})
	});

	builder = PARAS.into_iter().fold(builder, |acc, (para_id, debug_args)| {
		let args: Vec<Arg> = vec![debug_args.into(), "--authoring=slot-based".into()];

		acc.with_parachain(|p| {
			p.with_id(para_id)
				.with_registration_strategy(RegistrationStrategy::Manual)
				.with_chain(format!("glutton-westend-local-{para_id}").as_str())
				.with_genesis_overrides(json!({
					"patch": {
						"glutton": {
							"compute": "50000000",
							"storage": "2500000000",
							"trashDataCount": 5120
						}
					}
				}))
				.with_default_image(col_image.as_str())
				.with_default_command("polkadot-parachain")
				.with_default_args(args)
				.with_collator(|n| n.with_name(&format!("collator-{para_id}")))
		})
	});

	builder = builder.with_global_settings(|global_settings| {
		match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		}
	});

	builder.build().map_err(|e| {
		let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
		anyhow!("config errs: {errs}")
	})
}
