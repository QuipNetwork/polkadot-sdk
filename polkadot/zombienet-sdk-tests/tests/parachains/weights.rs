// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

#[zombienet_sdk::subxt::subxt(
	runtime_metadata_path = "metadata-files/asset-hub-westend-local.scale",
	crate = "::zombienet_sdk::subxt::ext::subxt_core"
)]
mod ahw {}

use ahw::runtime_types::{
	pallet_revive_types::runtime_api::types::contract::{CodeV1, StorageDepositV1},
	sp_weights::weight_v2::Weight,
};
use anyhow::anyhow;
use asset_hub_westend_runtime::Runtime as AHWRuntime;
use ethabi::Token;
use futures::{stream::FuturesUnordered, StreamExt};
use pallet_revive::AddressMapper;
use rand::Rng;
use sp_core::{H160, H256};
use std::str::FromStr;
use zombienet_sdk::{
	subxt::{
		self, config::polkadot::PolkadotExtrinsicParamsBuilder, tx::SubmittableTransaction,
		OnlineClient, PolkadotConfig,
	},
	subxt_signer::{
		sr25519::{dev, Keypair},
		SecretUri,
	},
	LocalFileSystem, Network, NetworkConfigBuilder, NetworkNode,
};

const KEYS_COUNT: usize = 6000;
const CHUNK_SIZE: usize = 3000;
const CALL_CHUNK_SIZE: usize = 3000;

#[tokio::test(flavor = "multi_thread")]
async fn weights_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = setup_network().await?;
	let collator = network.get_node("collator")?;
	let para_client: OnlineClient<PolkadotConfig> = collator.wait_client().await?;
	let mut call_clients = vec![];
	for _ in 0..(KEYS_COUNT / CALL_CHUNK_SIZE + 1) {
		let call_client: OnlineClient<PolkadotConfig> = collator.wait_client().await?;
		call_clients.push(call_client);
	}
	log::info!("Network is ready, waiting for warm-up to finish");
	let _ = wait_warmup_finish(collator).await;

	log::info!("Warm-up finished, starting test");
	let alice = dev::alice();
	let keys = create_keys(KEYS_COUNT);
	let mut nonce = 0;
	let mut nonce = || {
		let current_nonce = nonce;
		nonce += 1;
		current_nonce
	};

	setup_accounts(&para_client, &alice, &keys, nonce()).await?;
	assert_block_proposing_time_no_greater_than_1s(collator).await;
	log::info!("Accounts ready");

	let contract_address = instantiate_contract(&para_client, &alice).await?;
	log::info!("Contract instantiated: {:?}", contract_address);

	log::info!("Minting...");
	let mint_100 = sp_core::hex2array!(
		"a0712d680000000000000000000000000000000000000000000000000000000000000064"
	)
	.to_vec();
	let mint_100_payload = vec![mint_100; KEYS_COUNT];
	call_contract(
		&para_client,
		call_clients,
		contract_address,
		&alice,
		&keys,
		nonce(),
		mint_100_payload,
	)
	.await?;
	assert_block_proposing_time_no_greater_than_1s(collator).await;

	log::info!("Minting finished, preparing transfers");
	let mut transfer_50_payload = keys
		.iter()
		.map(|key| {
			let transfer_selector = sp_core::hex2array!("a9059cbb");
			let mut data = transfer_selector.to_vec();
			let account_id = key.public_key().0.into();
			let h160 =
				<AHWRuntime as pallet_revive::Config>::AddressMapper::to_address(&account_id);
			data.extend(ethabi::encode(&[Token::Address(h160), Token::Uint(50.into())]));

			data
		})
		.collect::<Vec<_>>();
	transfer_50_payload.rotate_left(1);

	log::info!("Restarting collator to ensure it starts with a clean state and waiting for warm-up to finish");

	collator.restart(None).await?;
	let mut call_clients = vec![];
	let para_client: OnlineClient<PolkadotConfig> = collator.wait_client().await?;

	for _ in 0..(KEYS_COUNT / CALL_CHUNK_SIZE + 1) {
		let call_client: OnlineClient<PolkadotConfig> = collator.wait_client().await?;
		call_clients.push(call_client);
	}

	let _ = wait_warmup_finish(collator).await;
	log::info!("Warm-up finished, transfering ERC20 tokens");

	call_contract(
		&para_client,
		call_clients,
		contract_address,
		&alice,
		&keys,
		nonce(),
		transfer_50_payload,
	)
	.await?;
	assert_block_proposing_time_no_greater_than_1s(collator).await;

	Ok(())
}

async fn assert_block_proposing_time_no_greater_than_1s(collator: &NetworkNode) {
	let num_blocks_under_1s = collator.reports(
		"substrate_proposer_block_proposal_time_bucket{chain=\"asset-hub-westend-local\",le=\"1\"}",
	).await.expect("Could not fetch report");

	let num_total_proposed_blocks = collator
		.reports("substrate_proposer_block_proposal_time_count{chain=\"asset-hub-westend-local\"}")
		.await
		.expect("Could not fetch report");

	let num_blocks_hit_deadline = collator.reports("substrate_proposer_end_proposal_reason{reason=\"hit_deadline\",chain=\"asset-hub-westend-local\"}").await.expect("Could not fetch report");
	assert_eq!(num_blocks_under_1s, num_total_proposed_blocks);
	assert_eq!(num_blocks_hit_deadline, 0.0, "There should be no blocks that hit the deadline");
}

async fn wait_warmup_finish(collator: &NetworkNode) -> Result<(), anyhow::Error> {
	while collator.reports("substrate_tasks_ended_total{kind=\"blocking\",reason=\"finished\",task_group=\"default\",task_name=\"warm-up-trie-cache\",chain=\"asset-hub-westend-local\"}").await? < 0.5 {
		std::thread::sleep(std::time::Duration::from_secs(10));
	}
	Ok(())
}

async fn setup_network() -> Result<Network<LocalFileSystem>, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("westend-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_default_db_snapshot("https://storage.googleapis.com/zombienet-db-snaps/polkadot/test_weights/relaychain.tgz")
				.with_validator(|node| node.with_name("validator-0"))
				.with_validator(|node| node.with_name("validator-1"))
		})
		.with_parachain(|p| {
			p.with_id(2000)
				.with_default_command("polkadot-parachain")
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.with_chain("asset-hub-westend-local")
				.with_default_db_snapshot("https://storage.googleapis.com/zombienet-db-snaps/polkadot/test_weights/parachain.tgz")
				.with_collator(|n| {
					n.with_name("collator").validator(true).with_args(vec![
						("--warm-up-trie-cache").into(),
						("-linfo").into(),
						("--pool-type=fork-aware").into(),
						("--trie-cache-size=32212254720").into(),
						("--rpc-max-subscriptions-per-connection=327680").into(),
						("--rpc-max-connections=102400".into()),
						("--pool-limit=819200").into(),
						("--pool-kbytes=2048000").into(),
					])
				})
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;
	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	Ok(network)
}

fn create_keys(n: usize) -> Vec<Keypair> {
	let mut rng = rand::thread_rng();
	let seed: u32 = rng.gen();
	(0..n)
		.map(|i| {
			let uri = SecretUri::from_str(&format!("//key{seed}_test{i}")).unwrap();
			Keypair::from_uri(&uri).unwrap()
		})
		.collect()
}

fn tx_params<T: subxt::Config>(
	nonce: u64,
) -> <subxt::config::DefaultExtrinsicParams<T> as subxt::config::ExtrinsicParams<T>>::Params {
	PolkadotExtrinsicParamsBuilder::<T>::new().nonce(nonce).build()
}

async fn setup_accounts(
	client: &OnlineClient<PolkadotConfig>,
	caller: &Keypair,
	keys: &[Keypair],
	nonce: u64,
) -> Result<(), anyhow::Error> {
	let caller_account_id = caller.public_key().to_account_id();
	let mut caller_nonce = client.tx().account_nonce(&caller_account_id).await?;
	for chunk in keys.chunks(CHUNK_SIZE) {
		let mut transfers = vec![];
		for key in chunk.iter() {
			let key_account = key.public_key().into();
			let call = &ahw::tx().balances().transfer_keep_alive(key_account, 1000000000000);
			let params = tx_params(caller_nonce);
			caller_nonce += 1;
			let signed_tx = client.tx().create_signed(call, caller, params).await?;
			transfers.push(signed_tx);
		}
		submit_txs(transfers).await?;
	}

	let map_call = &ahw::tx().revive().map_account();
	let mut is_caller_mapped = false;
	for chunk in keys.chunks(CHUNK_SIZE) {
		let mut mappings = vec![];
		for key in chunk.iter() {
			let signed_tx = client.tx().create_signed(map_call, key, tx_params(nonce)).await?;
			mappings.push(signed_tx);
		}
		if !is_caller_mapped {
			is_caller_mapped = true;
			let signed_tx =
				client.tx().create_signed(map_call, caller, tx_params(caller_nonce)).await?;
			mappings.push(signed_tx);
		}
		submit_txs(mappings).await?;
	}

	Ok(())
}

async fn instantiate_contract(
	client: &OnlineClient<PolkadotConfig>,
	caller: &Keypair,
) -> Result<H160, anyhow::Error> {
	let code_path = std::env::current_dir().unwrap().join("tests/parachains/contract.polkavm");
	let code = std::fs::read(code_path)?;
	let (ref_time, proof_size, deposit) = instantiate_params(client, code.clone(), caller).await?;

	// We need a nonce before instantiating the contract
	let account_id = caller.public_key().0.into();
	let caller_h160 = <AHWRuntime as pallet_revive::Config>::AddressMapper::to_address(&account_id);
	log::info!("H160 Account: {:?}", caller_h160);
	let caller_revive_nonce = client
		.runtime_api()
		.at_latest()
		.await?
		.call(ahw::apis().revive_api().nonce(caller_h160))
		.await?;
	let contract_address = pallet_revive::create1(&caller_h160, caller_revive_nonce.into());
	let weight = Weight { ref_time, proof_size };
	let call = &ahw::tx().revive().instantiate_with_code(0, weight, deposit, code, vec![], None);
	client
		.tx()
		.sign_and_submit_then_watch_default(call, caller)
		.await?
		.wait_for_finalized_success()
		.await?;

	Ok(contract_address)
}

async fn instantiate_params(
	client: &OnlineClient<PolkadotConfig>,
	code: Vec<u8>,
	caller: &Keypair,
) -> Result<(u64, u64, u128), anyhow::Error> {
	let account_id = caller.public_key().to_account_id();
	let code = CodeV1::Upload(code);
	let call = ahw::apis()
		.revive_api()
		.instantiate(account_id, 0, None, None, code, vec![], None);
	let dry_run = client.runtime_api().at_latest().await?.call(call).await?;
	let deposit = match dry_run.storage_deposit {
		StorageDepositV1::Charge(c) => c,
		StorageDepositV1::Refund(_) => 0,
	};

	// Make sure we have enough gas and multiply by 4, since without it the calls fail not enough
	// gas.
	Ok((dry_run.weight_required.ref_time * 4, dry_run.weight_required.proof_size * 4, deposit * 4))
}

async fn call_params(
	client: &OnlineClient<PolkadotConfig>,
	contract: H160,
	payload: Vec<u8>,
	caller: &Keypair,
) -> Result<(u64, u64, u128), anyhow::Error> {
	let account_id = caller.public_key().to_account_id();
	let call = ahw::apis().revive_api().call(account_id, contract, 0, None, None, payload);
	let dry_run = client.runtime_api().at_latest().await?.call(call).await?;
	let deposit = match dry_run.storage_deposit {
		StorageDepositV1::Charge(c) => c,
		StorageDepositV1::Refund(_) => 0,
	};

	Ok((dry_run.weight_required.ref_time, dry_run.weight_required.proof_size, deposit))
}

async fn call_contract(
	client: &OnlineClient<PolkadotConfig>,
	mut call_clients: Vec<OnlineClient<PolkadotConfig>>,
	contract: H160,
	caller: &Keypair,
	keys: &[Keypair],
	nonce: u64,
	payload: Vec<Vec<u8>>,
) -> Result<(), anyhow::Error> {
	let payload_sample = payload.first().cloned().expect("Payload is not empty");
	let (ref_time, proof_size, deposit) =
		call_params(client, contract, payload_sample, caller).await?;

	let mut txs = vec![];
	for (i_chunk, chunk) in keys.chunks(CALL_CHUNK_SIZE).enumerate() {
		let para_client = call_clients.pop().unwrap();
		for (i, key) in chunk.iter().enumerate() {
			let weight = Weight { ref_time, proof_size };
			let payload = payload[i_chunk * CALL_CHUNK_SIZE + i].clone();
			let call = &ahw::tx().revive().call(contract, 0, weight, deposit, payload);
			let signed_tx = para_client.tx().create_signed(call, key, tx_params(nonce)).await?;
			txs.push(signed_tx);
		}
	}
	let finalized_blocks = submit_txs(txs).await?;
	for block in finalized_blocks {
		let weight = client
			.storage()
			.at(block)
			.fetch(&ahw::storage().system().block_weight())
			.await?;
		log::info!("Weight of block {:?}: {:?}", block, weight);
	}

	Ok(())
}

async fn submit_txs(
	txs: Vec<SubmittableTransaction<PolkadotConfig, OnlineClient<PolkadotConfig>>>,
) -> Result<std::collections::HashSet<H256>, anyhow::Error> {
	let futs = txs.iter().map(|tx| tx.submit_and_watch()).collect::<FuturesUnordered<_>>();
	let res = futs.collect::<Vec<_>>().await;
	let res: Result<Vec<_>, _> = res.into_iter().collect();
	let res = res.expect("All the transactions submitted successfully");
	let mut statuses = futures::stream::select_all(res);
	let mut finalized_blocks = std::collections::HashSet::new();
	while let Some(a) = statuses.next().await {
		match a {
			Ok(st) => match st {
				subxt::tx::TxStatus::Validated => log::trace!("VALIDATED"),
				subxt::tx::TxStatus::Broadcasted => log::trace!("BROADCASTED"),
				subxt::tx::TxStatus::NoLongerInBestBlock => log::warn!("NO LONGER IN BEST BLOCK"),
				subxt::tx::TxStatus::InBestBlock(_) => log::trace!("IN BEST BLOCK"),
				subxt::tx::TxStatus::InFinalizedBlock(block) => {
					log::trace!("IN FINALIZED BLOCK");
					finalized_blocks.insert(block.block_hash());
				},
				subxt::tx::TxStatus::Error { message } => log::warn!("ERROR: {message}"),
				subxt::tx::TxStatus::Invalid { message } => log::trace!("INVALID: {message}"),
				subxt::tx::TxStatus::Dropped { message } => log::trace!("DROPPED: {message}"),
			},
			Err(e) => {
				println!("Error status {e:?}");
			},
		}
	}
	Ok(finalized_blocks)
}
