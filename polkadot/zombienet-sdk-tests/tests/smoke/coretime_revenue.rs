// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Binaries for this test should be built with `fast-runtime` feature enabled:
//! `cargo build -r -F fast-runtime -p polkadot-parachain-bin && \`
//! `cargo build -r -F fast-runtime --bin polkadot --bin polkadot-execute-worker --bin
//! polkadot-prepare-worker`
//!
//! Running with normal runtimes is possible but would take ages. Running fast relay runtime with
//! normal parachain runtime WILL mess things up.

use anyhow::anyhow;

#[zombienet_sdk::subxt::subxt(
	runtime_metadata_path = "metadata-files/coretime-westend-local.scale",
	crate = "::zombienet_sdk::subxt::ext::subxt_core"
)]
mod coretime_westend {}

#[zombienet_sdk::subxt::subxt(
	runtime_metadata_path = "metadata-files/westend-local.scale",
	crate = "::zombienet_sdk::subxt::ext::subxt_core"
)]
mod westend {}

use westend::runtime_types::{
	polkadot_parachain_primitives::primitives,
	staging_xcm::v4::{
		asset::{Asset, AssetId, Assets, Fungibility},
		junction::Junction,
		junctions::Junctions,
		location::Location,
	},
	xcm::{VersionedAssets, VersionedLocation},
};

use serde_json::json;
use std::{fmt::Display, sync::Arc};
use tokio::sync::RwLock;
use zombienet_sdk::{
	subxt::{events::StaticEvent, utils::AccountId32, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfigBuilder,
};

use coretime_westend::{
	self as coretime_api,
	broker::events as broker_events,
	runtime_types::{
		pallet_broker::types::{ConfigRecord as BrokerConfigRecord, Finality as BrokerFinality},
		sp_arithmetic::per_things::Perbill,
	},
};

use westend::on_demand_assignment_provider::events as on_demand_events;

type CoretimeRuntimeCall = coretime_api::runtime_types::coretime_westend_runtime::RuntimeCall;
type CoretimeUtilityCall = coretime_api::runtime_types::pallet_utility::pallet::Call;
type CoretimeBrokerCall = coretime_api::runtime_types::pallet_broker::pallet::Call;

// On-demand coretime base fee (set at the genesis)
const ON_DEMAND_BASE_FEE: u128 = 50_000_000;

async fn get_total_issuance(
	relay: OnlineClient<PolkadotConfig>,
	coretime: OnlineClient<PolkadotConfig>,
) -> (u128, u128) {
	(
		relay
			.storage()
			.at_latest()
			.await
			.unwrap()
			.fetch(&westend::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap(),
		coretime
			.storage()
			.at_latest()
			.await
			.unwrap()
			.fetch(&coretime_api::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap(),
	)
}

async fn assert_total_issuance(
	relay: OnlineClient<PolkadotConfig>,
	coretime: OnlineClient<PolkadotConfig>,
	ti: (u128, u128),
) {
	let actual_ti = get_total_issuance(relay, coretime).await;
	log::debug!("Asserting total issuance: actual: {actual_ti:?}, expected: {ti:?}");
	assert_eq!(ti, actual_ti);
}

type EventOf<C> = Arc<RwLock<Vec<(u64, zombienet_sdk::subxt::events::EventDetails<C>)>>>;

macro_rules! trace_event {
	($event:ident : $mod:ident => $($ev:ident),*) => {
		match $event.variant_name() {
			$(
				stringify!($ev) =>
					log::trace!("{:#?}", $event.as_event::<$mod::$ev>().unwrap().unwrap()),
			)*
			_ => ()
		}
	};
}

async fn para_watcher<C: zombienet_sdk::subxt::Config + Clone>(
	api: OnlineClient<C>,
	events: EventOf<C>,
) where
	<C::Header as zombienet_sdk::subxt::config::Header>::Number: Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();
		log::debug!("Finalized parachain block {}", block.number());

		for event in block.events().await.unwrap().iter() {
			let event = event.unwrap();
			log::debug!("Got event: {} :: {}", event.pallet_name(), event.variant_name());
			{
				events.write().await.push((block.number().into(), event.clone()));
			}

			if event.pallet_name() == "Broker" {
				trace_event!(event: broker_events =>
					Purchased, SaleInitialized, HistoryInitialized, CoreAssigned, Pooled,
					ClaimsReady, RevenueClaimBegun,	RevenueClaimItem, RevenueClaimPaid
				);
			}
		}
	}
}

async fn relay_watcher<C: zombienet_sdk::subxt::Config + Clone>(
	api: OnlineClient<C>,
	events: EventOf<C>,
) where
	<C::Header as zombienet_sdk::subxt::config::Header>::Number: Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();
		log::debug!("Finalized parachain block {}", block.number());

		for event in block.events().await.unwrap().iter() {
			let event = event.unwrap();
			log::debug!("Got event: {} :: {}", event.pallet_name(), event.variant_name());
			{
				events.write().await.push((block.number().into(), event.clone()));
			}

			if event.pallet_name() == "OnDemandAssignmentProvider" {
				trace_event!(event: on_demand_events =>
					AccountCredited, SpotPriceSet, OnDemandOrderPlaced
				);
			}
		}
	}
}

async fn wait_for_event<
	C: zombienet_sdk::subxt::Config + Clone,
	E: StaticEvent,
	P: Fn(&E) -> bool + Copy,
>(
	events: EventOf<C>,
	pallet: &'static str,
	variant: &'static str,
	predicate: P,
) -> E {
	loop {
		let mut events = events.write().await;
		if let Some(entry) = events.iter().find(|&e| {
			e.1.pallet_name() == pallet &&
				e.1.variant_name() == variant &&
				predicate(&e.1.as_event::<E>().unwrap().unwrap())
		}) {
			let entry = entry.clone();
			events.retain(|e| e.0 > entry.0);
			return entry.1.as_event::<E>().unwrap().unwrap();
		}
		drop(events);
		tokio::time::sleep(std::time::Duration::from_secs(6)).await;
	}
}

async fn ti_watcher<C: zombienet_sdk::subxt::Config + Clone>(
	api: OnlineClient<C>,
	prefix: &'static str,
) where
	<C::Header as zombienet_sdk::subxt::config::Header>::Number: Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	let mut issuance = 0i128;

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();

		let ti = api
			.storage()
			.at(block.reference())
			.fetch(&westend::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap() as i128;

		let diff = ti - issuance;
		if diff != 0 {
			log::info!("{} #{} issuance {} ({:+})", prefix, block.number(), ti, diff);
		}
		issuance = ti;
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn coretime_revenue_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("westend-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_genesis_overrides(
					json!({ "configuration": { "config": { "scheduler_params": { "on_demand_base_fee": ON_DEMAND_BASE_FEE }}}}),
				)
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
				.with_validator(|node| node.with_name("charlie"))
		})
		.with_parachain(|p| {
			p.with_id(1005)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("coretime-westend-local")
				.with_collator(|n| n.with_name("coretime"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("alice")?;
	let para_node = network.get_node("coretime")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let para_client: OnlineClient<PolkadotConfig> = para_node.wait_client().await?;

	// Get total issuance on both sides
	let mut total_issuance = get_total_issuance(relay_client.clone(), para_client.clone()).await;
	log::info!("Reference total issuance: {total_issuance:?}");

	// Prepare everything
	let alice = dev::alice();
	let alice_acc = AccountId32(alice.public_key().0);

	let bob = dev::bob();

	let para_events: EventOf<PolkadotConfig> = Arc::new(RwLock::new(Vec::new()));
	let p_api = para_node.wait_client().await?;
	let p_events = para_events.clone();

	let _subscriber1 = tokio::spawn(async move {
		para_watcher(p_api, p_events).await;
	});

	let relay_events: EventOf<PolkadotConfig> = Arc::new(RwLock::new(Vec::new()));
	let r_api = relay_node.wait_client().await?;
	let r_events = relay_events.clone();

	let _subscriber2 = tokio::spawn(async move {
		relay_watcher(r_api, r_events).await;
	});

	let api: OnlineClient<PolkadotConfig> = para_node.wait_client().await?;
	let _s1 = tokio::spawn(async move {
		ti_watcher(api, "PARA").await;
	});
	let api: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let _s2 = tokio::spawn(async move {
		ti_watcher(api, "RELAY").await;
	});

	log::info!("Initiating teleport from RC's account of Alice to PC's one");

	// Teleport some Alice's tokens to the Coretime chain. Although her account is pre-funded on
	// the PC, that is still neccessary to bootstrap RC's `CheckedAccount`.
	relay_client
		.tx()
		.sign_and_submit_default(
			&westend::tx().xcm_pallet().teleport_assets(
				VersionedLocation::V4(Location {
					parents: 0,
					interior: Junctions::X1([Junction::Parachain(1005)]),
				}),
				VersionedLocation::V4(Location {
					parents: 0,
					interior: Junctions::X1([Junction::AccountId32 {
						network: None,
						id: alice.public_key().0,
					}]),
				}),
				VersionedAssets::V4(Assets(vec![Asset {
					id: AssetId(Location { parents: 0, interior: Junctions::Here }),
					fun: Fungibility::Fungible(1_500_000_000),
				}])),
				0,
			),
			&alice,
		)
		.await?;

	wait_for_event(
		para_events.clone(),
		"Balances",
		"Minted",
		|e: &coretime_api::balances::events::Minted| e.who == alice_acc,
	)
	.await;

	// RC's total issuance doen't change, but PC's one increases after the teleport.

	total_issuance.1 += 1_500_000_000;
	assert_total_issuance(relay_client.clone(), para_client.clone(), total_issuance).await;

	log::info!("Initializing broker and starting sales");

	// Initialize broker and start sales

	para_client
		.tx()
		.sign_and_submit_default(
			&coretime_api::tx().sudo().sudo(CoretimeRuntimeCall::Utility(
				CoretimeUtilityCall::batch {
					calls: vec![
						CoretimeRuntimeCall::Broker(CoretimeBrokerCall::configure {
							config: BrokerConfigRecord {
								advance_notice: 5,
								interlude_length: 1,
								leadin_length: 1,
								region_length: 1,
								ideal_bulk_proportion: Perbill(100),
								limit_cores_offered: None,
								renewal_bump: Perbill(10),
								contribution_timeout: 5,
							},
						}),
						CoretimeRuntimeCall::Broker(CoretimeBrokerCall::set_lease {
							task: 1005,
							until: 1000,
						}),
						CoretimeRuntimeCall::Broker(CoretimeBrokerCall::start_sales {
							end_price: 45_000_000,
							extra_cores: 2,
						}),
					],
				},
			)),
			&alice,
		)
		.await?;

	log::info!("Waiting for a full-length sale to begin");

	// Skip the first sale completeley as it may be a short one. Also, `request_core_count` requires
	// two session boundaries to propagate. Given that the `fast-runtime` session is 10 blocks and
	// the timeslice is 20 blocks, we should be just in time.

	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;
	log::info!("Skipped short sale");

	let sale: coretime_api::broker::events::SaleInitialized =
		wait_for_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;
	log::info!("{:?}", sale);

	// Alice buys a region

	log::info!("Alice is going to buy a region");

	para_client
		.tx()
		.sign_and_submit_default(&coretime_api::tx().broker().purchase(1_000_000_000), &alice)
		.await?;

	let purchase = wait_for_event(
		para_events.clone(),
		"Broker",
		"Purchased",
		|e: &broker_events::Purchased| e.who == alice_acc,
	)
	.await;

	let region_begin = purchase.region_id.begin;

	// Somewhere below this point, the revenue from this sale will be teleported to the RC and burnt
	// on both chains. Let's account that but not assert just yet.

	total_issuance.0 -= purchase.price;
	total_issuance.1 -= purchase.price;

	// Alice pools the region

	log::info!("Alice is going to put the region into the pool");

	para_client
		.tx()
		.sign_and_submit_default(
			&coretime_api::tx().broker().pool(
				purchase.region_id,
				alice_acc.clone(),
				BrokerFinality::Final,
			),
			&alice,
		)
		.await?;

	let pooled =
		wait_for_event(para_events.clone(), "Broker", "Pooled", |e: &broker_events::Pooled| {
			e.region_id.begin == region_begin
		})
		.await;

	// Wait until the beginning of the timeslice where the region belongs to

	log::info!("Waiting for the region to begin");

	let hist = wait_for_event(
		para_events.clone(),
		"Broker",
		"HistoryInitialized",
		|e: &broker_events::HistoryInitialized| e.when == pooled.region_id.begin,
	)
	.await;

	// Alice's private contribution should be there

	assert!(hist.private_pool_size > 0);

	// Bob places an order to buy insta coretime as RC

	log::info!("Bob is going to buy an on-demand core");

	let r = relay_client
		.tx()
		.sign_and_submit_then_watch_default(
			&westend::tx()
				.on_demand_assignment_provider()
				.place_order_allow_death(100_000_000, primitives::Id(100)),
			&bob,
		)
		.await?
		.wait_for_finalized_success()
		.await?;

	let order = r
		.find_first::<westend::on_demand_assignment_provider::events::OnDemandOrderPlaced>()?
		.unwrap();

	// As there's no spot traffic, Bob will only pay base fee

	assert_eq!(order.spot_price, ON_DEMAND_BASE_FEE);

	// Somewhere below this point, revenue is generated and is teleported to the PC (that happens
	// once a timeslice so we're not ready to assert it yet, let's just account). That checks out
	// tokens from the RC and mints them on the PC.

	total_issuance.1 += ON_DEMAND_BASE_FEE;

	// As soon as the PC receives the tokens, it divides them half by half into system and private
	// contributions (we have 3 cores, one is leased to Coretime itself, one is pooled by the
	// system, and one is pooled by Alice).

	// Now we're waiting for the moment when Alice may claim her revenue

	log::info!("Waiting for Alice's revenue to be ready to claim");

	let claims_ready = wait_for_event(
		para_events.clone(),
		"Broker",
		"ClaimsReady",
		|e: &broker_events::ClaimsReady| e.when == pooled.region_id.begin,
	)
	.await;

	// The revenue should be half of the spot price, which is equal to the base fee.

	assert_eq!(claims_ready.private_payout, ON_DEMAND_BASE_FEE / 2);

	// By this moment, we're sure that revenue was received by the PC and can assert the total
	// issuance

	assert_total_issuance(relay_client.clone(), para_client.clone(), total_issuance).await;

	// Try purchasing on-demand with credits:

	log::info!("Bob is going to buy on-demand credits for alice");

	let r = para_client
		.tx()
		.sign_and_submit_then_watch_default(
			&coretime_api::tx().broker().purchase_credit(100_000_000, alice_acc.clone()),
			&bob,
		)
		.await?
		.wait_for_finalized_success()
		.await?;

	assert!(r.find_first::<coretime_api::broker::events::CreditPurchased>()?.is_some());

	let _account_credited = wait_for_event(
		relay_events.clone(),
		"OnDemandAssignmentProvider",
		"AccountCredited",
		|e: &on_demand_events::AccountCredited| e.who == alice_acc && e.amount == 100_000_000,
	)
	.await;

	// Once the account is credit we can place an on-demand order using credits
	log::info!("Alice is going to place an on-demand order using credits");

	let r = relay_client
		.tx()
		.sign_and_submit_then_watch_default(
			&westend::tx()
				.on_demand_assignment_provider()
				.place_order_with_credits(100_000_000, primitives::Id(100)),
			&alice,
		)
		.await?
		.wait_for_finalized_success()
		.await?;

	let order = r
		.find_first::<westend::on_demand_assignment_provider::events::OnDemandOrderPlaced>()?
		.unwrap();

	assert_eq!(order.spot_price, ON_DEMAND_BASE_FEE);

	// NOTE: Purchasing on-demand with credits doesn't affect the total issuance, as the credits are
	// purchased on the PC. Therefore we don't check for total issuance changes.

	// Alice claims her revenue

	log::info!("Alice is going to claim her revenue");

	para_client
		.tx()
		.sign_and_submit_default(
			&coretime_api::tx().broker().claim_revenue(pooled.region_id, pooled.duration),
			&alice,
		)
		.await?;

	let claim_paid = wait_for_event(
		para_events.clone(),
		"Broker",
		"RevenueClaimPaid",
		|e: &broker_events::RevenueClaimPaid| e.who == alice_acc,
	)
	.await;

	log::info!("Revenue claimed, waiting for 2 timeslices until the system revenue is burnt");

	assert_eq!(claim_paid.amount, ON_DEMAND_BASE_FEE / 2);

	// As for the system revenue, it is teleported back to the RC and burnt there. Those burns are
	// batched and are processed once a timeslice, after a new one starts. So we have to wait for
	// two timeslice boundaries to pass to be sure the teleport has already happened somewhere in
	// between.

	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;

	total_issuance.0 -= ON_DEMAND_BASE_FEE / 2;
	total_issuance.1 -= ON_DEMAND_BASE_FEE / 2;

	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;

	assert_total_issuance(relay_client.clone(), para_client.clone(), total_issuance).await;

	assert_eq!(order.spot_price, ON_DEMAND_BASE_FEE);

	log::info!("Test finished successfully");

	Ok(())
}
