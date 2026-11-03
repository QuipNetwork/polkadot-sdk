// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Setup a network with 4 validators and 4 parachains. Then assign core 0
//! to be shared by all paras and check the block production in each one.

use crate::utils::{
	create_force_register_call, env_or_default, fetch_header_and_validation_code,
	initialize_network, maybe_enable_experimental_collator_protocol, COL_IMAGE_ENV,
	INTEGRATION_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_para_throughput, submit_extrinsic_and_wait_for_finalization_success_with_timeout,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use std::{collections::HashMap, ops::Range};
use zombienet_sdk::{
	subxt::{dynamic::Value, ext::scale_value::value, tx},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder, RegistrationStrategy,
};

#[tokio::test(flavor = "multi_thread")]
async fn coretime_shared_core_test_3_paras() -> Result<(), anyhow::Error> {
	coretime_shared_core_inner(3u32).await
}

#[tokio::test(flavor = "multi_thread")]
async fn coretime_shared_core_test_4_paras() -> Result<(), anyhow::Error> {
	coretime_shared_core_inner(4u32).await
}

async fn coretime_shared_core_inner(number_of_paras: u32) -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config(number_of_paras)?;
	let network = initialize_network(config).await?;

	let alice_account = Value::from_bytes(dev::alice().public_key().0);
	let relaychain_nodes = network.relaychain().nodes();
	let relay_node = relaychain_nodes.first().ok_or(anyhow!("relaychain should have one node"))?;
	let relay_client = relay_node.wait_client().await?;

	let para_ids: Vec<u32> = (0..number_of_paras).map(|i| 2000 + i).collect();
	log::info!("register paras 2 by 2 to speed up the test. registering all at once will exceed the weight limit.");
	for chunk in para_ids.chunks(2) {
		let mut calls = vec![];
		for para_id in chunk {
			let node = network.get_node(format!("collator-{para_id}"))?;
			let client = node.wait_client().await?;
			let (head, validation_code) = fetch_header_and_validation_code(&client).await?;
			calls = [
				calls,
				create_force_register_call(
					&head[..],
					&validation_code[..],
					*para_id,
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
		log::info!("Registration for paras {chunk:?} completed");
	}

	let part_of_57600 = 57600 / number_of_paras;
	log::info!("assign core 0 to be shared by all paras ({part_of_57600}).");
	let assigments: Vec<Value> =
		para_ids.iter().map(|id| value! { (Task(*id), part_of_57600) }).collect();
	let sudo_call_assign_core = tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Coretime(assign_core { core: 0u32, begin: 0u32, assignment: (
				assigments
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
	log::info!("Core 0 assignment shared for all paras completed");

	// Check that all parachains produce blocks within 40 RC blocks
	// (since core 0 is shared between all paras)
	let num_blocks = 40;
	// N paras share 1 core (~1 para blocks/slot async backing).
	// Each session lasts for 10 blocks (fast-runtime) and the first 2 blocks of each session
	// don't produce backed candidates (because max_claim_queue_offset = 1).
	// So only 80% of the relay chain blocks will produce backed candidates.
	let exp = (num_blocks as f64 * 0.8) / (number_of_paras as f64);
	let min = exp.round() as u32 - 1;
	let max = exp.round() as u32 + 2; // +2 because it's not `RangeInclusive`.
	log::info!("Checking parachain block production with range ({min}..{max})");
	let mut para_throughput_map: HashMap<ParaId, Range<u32>> = Default::default();
	for id in para_ids.iter() {
		para_throughput_map.insert(ParaId::from(*id), min..max);
	}
	assert_para_throughput(&relay_client, num_blocks, para_throughput_map, []).await?;
	log::info!("All parachains producing blocks");

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config(number_of_paras: u32) -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r
        .with_chain("rococo-local")
        .with_default_command("polkadot")
        .with_default_image(polkadot_image.as_str())
        .with_default_args(maybe_enable_experimental_collator_protocol(
            vec!["-lparachain=debug,runtime=debug".into()],
        ))
        .with_genesis_overrides(json!({
            "patch": {
                "configuration": {
                    "config": {
                        "needed_approvals": 3,
                        "scheduler_params": {
                            "max_validators_per_core": 1,
                            "num_cores": number_of_paras
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
            g.with_count((number_of_paras + 1u32) as usize)
			.with_base_node(|node| {
                node.with_name("validator")
                    .with_args(vec!["-lruntime=debug,parachain=debug,parachain::backing=trace,parachain::collator-protocol=trace,parachain::prospective-parachains=trace,runtime::parachains::scheduler=trace,runtime::inclusion-inherent=trace,runtime::inclusion=trace".into()])
            })
		})
	});

	let para_ids: Vec<u32> = (0..number_of_paras).map(|i| 2000 + i).collect();
	builder = para_ids.into_iter().fold(builder, |acc, para_id| {
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
				.with_default_args(vec![
					"--authoring=slot-based".into(),
					"-lparachain=debug".into(),
				])
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
