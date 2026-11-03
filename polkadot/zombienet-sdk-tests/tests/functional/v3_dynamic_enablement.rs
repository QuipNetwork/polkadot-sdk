// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! V3 candidates require the `CandidateReceiptV3` node feature (bit 4) on the relay AND the v3
//! runtime on the para. The relay side is toggled with a `set_node_feature` extrinsic; the para
//! side via a runtime upgrade to the v3 test runtime.
//!
//! Para 2902 walks up into V3 and then back out:
//!
//!   off/off → V2, on/off → V2, on/on → V3, on/off → V2
//!
//! The final step is the para rollback: 2902 is upgraded to a V3-disabled runtime while the feature
//! stays on, so the collator is still in V3 mode when the code swaps.

use crate::utils::{
	assert_candidates_version, assert_validator_backed_candidates, enable_node_features,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assign_cores, wait_for_nth_session_change, wait_for_pvf_prepare,
	wait_for_runtime_upgrade,
};
use polkadot_primitives::{node_features::FeatureIndex, CandidateDescriptorVersion, Id as ParaId};
use rstest::rstest;
use serde_json::json;
use std::{collections::HashMap, ops::Range};
use zombienet_sdk::{
	subxt::{dynamic::Value, ext::scale_value::value, tx::dynamic, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfigBuilder,
};

const SINGLE_CORE_CANDIDATES: Range<u32> = 7..11;
const ELASTIC_CANDIDATES: Range<u32> = 17..31;
const V3_CANDIDATES: Range<u32> = 4..11;

/// Relay config changes (node features included) enact `SESSION_DELAY` sessions after the session
/// they are applied in. Mirrors `polkadot_runtime_parachains::shared::SESSION_DELAY`.
const SESSION_DELAY: u32 = 2;

#[rstest]
#[case::para_rollback(0)]
#[case::para_rollback_rpo_2(2)]
#[tokio::test(flavor = "multi_thread")]
async fn v3_dynamic_enablement_test(
	// Para 2902's `relay_parent_offset`, held constant across the whole walk-up and rollback.
	//
	// At offset 0 the inherent's relay-parent-descendant check is inert
	// (`expected_rp_descendants_num == 0` short-circuits it), so a collator whose view of
	// `V3_SCHEDULING_ENABLED` lags the executing runtime only shows up later in the PVF. At
	// offset 2 that check is armed and catches the same disagreement in `set_validation_data`
	// instead, because the collator omits the descendants a v2-mode runtime demands.
	#[case] relay_parent_offset: u32,
) -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	// Runtime flavours for 2902, picked so the offset never changes — only the v3 const does.
	//
	// offset 0: async-backing -> v3 -> spec_version_incremented. The rollback target is a blob the
	//           network has never seen, so it also exercises a PVF prepare.
	// offset 2: relay-parent-offset-2 -> v3_rpo_2 -> relay-parent-offset-2, rolling back to the
	//           genesis blob.
	let (para_chain, v3_wasm, v3_disabled_wasm) = match relay_parent_offset {
		0 => (
			"async-backing",
			cumulus_test_runtime::v3::WASM_BINARY
				.ok_or_else(|| anyhow!("cumulus-test-runtime v3 WASM not built"))?,
			cumulus_test_runtime::spec_version_incremented::WASM_BINARY.ok_or_else(|| {
				anyhow!("cumulus-test-runtime spec_version_incremented WASM not built")
			})?,
		),
		2 => (
			"relay-parent-offset-2",
			cumulus_test_runtime::v3_rpo_2::WASM_BINARY
				.ok_or_else(|| anyhow!("cumulus-test-runtime v3_rpo_2 WASM not built"))?,
			cumulus_test_runtime::relay_parent_offset_2::WASM_BINARY.ok_or_else(|| {
				anyhow!("cumulus-test-runtime relay_parent_offset_2 WASM not built")
			})?,
		),
		other => return Err(anyhow!("no runtime flavours wired up for offset {other}")),
	};
	// One session of walk-back, matching what `scheduling_v3.rs` pairs offset 2 with. Any more and
	// the scheduling parent leaves prospective-parachains' scope at offset 2, dropping candidates
	// as `SchedulingParentNotInScope`. Offset 0 never walks back, so the value is irrelevant there.
	let max_relay_parent_session_age = 1;

	let scheduling_lookahead = 5;

	// 2902's rate falls once its relay parent trails behind the tip, so the offset-2 bound is wider
	// (scaled from `scheduling_v3.rs`'s `v3-rpo-4`, 15..30 over 40 blocks).
	let para_2902_candidates = || -> Range<u32> {
		match relay_parent_offset {
			2 => 4..11,
			_ => SINGLE_CORE_CANDIDATES,
		}
	};

	let v2_byte = 1u8 << FeatureIndex::CandidateReceiptV2 as u8; // 1 << 3 = 0b00001000
	let node_features_v2_only = json!({"bits": 8, "data": [v2_byte]});

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug,runtime=debug").into()])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"lookahead": scheduling_lookahead,
								// 2 extra cores beyond each auto-registered para core.
								// Para 2901 uses elastic scaling and is assigned 0 and 1 in addition.
								"num_cores": 2,
								"max_validators_per_core": 2,
								"group_rotation_frequency": 4
							},
							"node_features": node_features_v2_only,
							"max_relay_parent_session_age": max_relay_parent_session_age
						}
					}
				}))
				// Standard collator protocol validators (groups 0, 1).
				.with_validator(|node| node.with_name("validator-0"));

			let r = (1..4).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			});
			(4..10).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("validator-{i}")).with_args(vec![
						("-lparachain=debug,runtime=debug,parachain::collator-protocol=trace")
							.into(),
						("--experimental-collator-protocol").into(),
					])
				})
			})
		})
		// Para 2900: basic V2 (single core), stays V2. Two collators share the core.
		.with_parachain(|p| {
			p.with_id(2900)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![
					("--authoring=slot-based").into(),
					("-lparachain=debug,aura=debug").into(),
				])
				.with_collator(|n| n.with_name("collator-2900-a"))
				.with_collator(|n| n.with_name("collator-2900-b"))
		})
		// Para 2901: V2 elastic scaling (3 cores) — throughput must survive enablement.
		.with_parachain(|p| {
			p.with_id(2901)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("elastic-scaling")
				.with_default_args(vec![
					("--authoring=slot-based").into(),
					("-lparachain=debug,aura=debug").into(),
				])
				.with_collator(|n| n.with_name("collator-2901"))
		})
		// Para 2902: starts V2, upgraded to v3 mid-test, then rolled back. Two collators.
		// The chain spec fixes its `relay_parent_offset` for the whole test.
		.with_parachain(|p| {
			p.with_id(2902)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain(para_chain)
				.with_default_args(vec![
					("--authoring=slot-based").into(),
					("-lparachain=debug,aura=debug").into(),
				])
				.with_collator(|n| n.with_name("collator-2902-a"))
				.with_collator(|n| n.with_name("collator-2902-b"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-2900-a")?;
	let para_node_slot = network.get_node("collator-2901")?;
	let para_node_v3 = network.get_node("collator-2902-a")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let para_client_v3: OnlineClient<PolkadotConfig> = para_node_v3.wait_client().await?;

	// Assign 2 extra cores to para 2901 for elastic scaling (3 cores total).
	assign_cores(&relay_client, 2901, vec![0, 1]).await?;

	let para_2900 = ParaId::from(2900);
	let para_2901 = ParaId::from(2901);
	let para_2902 = ParaId::from(2902);

	// State (relay `CandidateReceiptV3` feature, para v3 runtime): off / off → V2.
	log::info!("state off/off (relay off, para off) → V2");
	assert_candidates_version(
		&relay_client,
		CandidateDescriptorVersion::V2,
		HashMap::from([
			(para_2900, SINGLE_CORE_CANDIDATES),
			(para_2901, ELASTIC_CANDIDATES),
			(para_2902, para_2902_candidates()),
		]),
		10,
	)
	.await?;

	// off/off → on/off: enable the relay feature. Para 2902 still runs the V2 runtime
	log::info!("state on/off (relay on, para off) → V2");
	enable_node_features(&relay_client, &[4]).await?;

	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	wait_for_nth_session_change(&mut blocks_sub, SESSION_DELAY).await?;
	// 2902 is still on the V2 runtime, so it stays V2 once the feature is enacted.
	assert_candidates_version(
		&relay_client,
		CandidateDescriptorVersion::V2,
		HashMap::from([
			(para_2900, SINGLE_CORE_CANDIDATES),
			(para_2901, ELASTIC_CANDIDATES),
			(para_2902, para_2902_candidates()),
		]),
		10,
	)
	.await?;

	// on/off → on/on: upgrade para 2902 to the v3 runtime. Both sides on → V3.
	// v3 shares its spec_version with V2, so use set_code_without_checks.
	// Not session anchored: `assert_candidates_version` waits for a session change before it
	// validates anything, which is well past the V2 collations in flight at the code swap.
	log::info!("state on/on (relay on, para on) → V3");
	let upgrade_call = dynamic(
		"Sudo",
		"sudo_unchecked_weight",
		vec![
			value! { System(set_code_without_checks { code: Value::from_bytes(v3_wasm) }) },
			value! { { ref_time: 1u64, proof_size: 1u64 } },
		],
	);
	para_client_v3
		.tx()
		.sign_and_submit_then_watch_default(&upgrade_call, &dev::alice())
		.await?
		.wait_for_finalized_success()
		.await?;
	wait_for_runtime_upgrade(&para_client_v3).await?;
	// At offset 0 2902's genesis code is 2900's, so the network holds 3 distinct validation codes
	// once the v3 one is added; every other offset gives 2902 its own genesis code, making 4.
	wait_for_pvf_prepare(&network, if relay_parent_offset == 0 { 3 } else { 4 }).await?;
	assert_candidates_version(
		&relay_client,
		CandidateDescriptorVersion::V3,
		HashMap::from([(para_2902, V3_CANDIDATES)]),
		10,
	)
	.await?;

	// on/on → on/off: upgrade 2902 to a V3-disabled runtime while the node feature stays on, so the
	// collator is still in V3 mode at the moment of the swap — the V3→V2 shape switch, not just a
	// change of claim queue depth. Not session gated (the new runtime applies at the next para
	// block), but wait a session out anyway so V3 collations that were in flight when the code was
	// swapped are not counted as a version violation.
	log::info!("state on/off (relay on, para rolled back off v3) → V2");
	// Unchecked for both offsets: the offset-2 rollback target shares a spec_version with
	// `v3_rpo_2`, which `System::set_code` would reject for not increasing the version.
	let rollback_call = dynamic(
		"Sudo",
		"sudo_unchecked_weight",
		vec![
			value! { System(set_code_without_checks { code: Value::from_bytes(v3_disabled_wasm) }) },
			value! { { ref_time: 1u64, proof_size: 1u64 } },
		],
	);
	para_client_v3
		.tx()
		.sign_and_submit_then_watch_default(&rollback_call, &dev::alice())
		.await?
		.wait_for_finalized_success()
		.await?;
	wait_for_runtime_upgrade(&para_client_v3).await?;
	// Only offset 0 rolls back to a blob the network has never seen
	// (`spec_version_incremented`), so only there does a fourth prepare happen.
	// Offset 2 rolls back to 2902's genesis code, already prepared, so the concluded-prepares
	// metric never advances.
	if relay_parent_offset == 0 {
		wait_for_pvf_prepare(&network, 4).await?;
	}
	// One session is enough here: this is not a session-gated config change, and
	// `wait_for_runtime_upgrade` above has already seen the code-swap block finalized, so every
	// relay parent from the next session on is past it.
	blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	wait_for_nth_session_change(&mut blocks_sub, 1).await?;
	assert_candidates_version(
		&relay_client,
		CandidateDescriptorVersion::V2,
		HashMap::from([
			(para_2900, SINGLE_CORE_CANDIDATES),
			(para_2901, ELASTIC_CANDIDATES),
			(para_2902, para_2902_candidates()),
		]),
		10,
	)
	.await?;

	assert_validator_backed_candidates(relay_node, 30).await?;
	for i in 4..=9 {
		let node = network.get_node(format!("validator-{i}"))?;
		assert_validator_backed_candidates(node, 30).await?;
	}

	assert_finality_lag(&para_node.wait_client().await?, 6).await?;
	assert_finality_lag(&para_node_slot.wait_client().await?, 15).await?;
	// 2902 finalizes further behind once its relay parent trails the tip, so at any non-zero offset
	// allow the 15 every other offset-2 test in this file already uses.
	assert_finality_lag(
		&para_node_v3.wait_client().await?,
		if relay_parent_offset > 0 { 15 } else { 6 },
	)
	.await?;

	log::info!("V3 dynamic enablement test finished successfully");

	Ok(())
}
