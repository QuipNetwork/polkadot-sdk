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
use crate::{
	ClientError, H160, LOG_TARGET, Log, ReceiptGasInfoV1, ReceiptInfo,
	client::{
		SubstrateBlock, SubstrateBlockNumber,
		version_aware_runtime_api::VersionAwareRuntimeApiProvider,
	},
	subxt_client::{
		SrcChainConfig,
		revive::{
			calls::EthTransact,
			events::{ContractEmitted, EthExtrinsicRevert},
		},
	},
};

use pallet_revive::{
	create1,
	evm::{GenericTransaction, H256, TransactionSigned, U256},
};
use sp_crypto_hashing::keccak_256;
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	future::Future,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};
use subxt::{
	client::OfflineClientAtBlockT,
	events::{DecodeAsEvent, Phase},
	extrinsics::Extrinsics,
};

type EventDetails<'a> = subxt::events::Event<'a, SrcChainConfig>;

/// Outcome of decoding a single pallet-revive event.
enum ReviveEvent {
	Revert,
	Log(Log),
}

/// Decode a single event detail into a [`ReviveEvent`], or `None` if it is not a pallet-revive
/// event we care about.
fn decode_revive_event(
	event: &EventDetails<'_>,
	block_number: U256,
	transaction_hash: H256,
	transaction_index: usize,
	block_hash: H256,
) -> Option<ReviveEvent> {
	let pallet_name = event.pallet_name();
	let event_name = event.event_name();

	if EthExtrinsicRevert::is_event(pallet_name, event_name) {
		return Some(ReviveEvent::Revert);
	}
	if ContractEmitted::is_event(pallet_name, event_name) {
		match event.decode_fields_as::<ContractEmitted>() {
			Some(Ok(evt)) => {
				return Some(ReviveEvent::Log(Log {
					address: evt.contract,
					topics: evt.topics,
					data: Some(evt.data.into()),
					block_number,
					transaction_hash,
					transaction_index: transaction_index.into(),
					block_hash,
					log_index: event.index().into(),
					..Default::default()
				}));
			},
			Some(Err(err)) => log::warn!(
				target: LOG_TARGET,
				"Failed to decode ContractEmitted event {} in block {block_number} (tx {transaction_hash:?}): {err:?}, log dropped from receipt",
				event.index()
			),
			// `is_event()` already confirmed the variant, so this is unreachable in practice.
			None => {},
		}
	}
	None
}

/// Iterate decoded block events and bucket revert flags and logs per extrinsic.
/// Events for other extrinsics are skipped.
///
/// Events are stored sequentially without size markers, so a single
/// undecodable event (e.g. from a runtime upgrade that shifted variant
/// indices) corrupts the offset for all subsequent events.
/// Decode errors are logged and skipped to avoid losing the entire receipt.
///
/// Returns `(reverted_extrinsics, logs_by_extrinsic)` keyed by extrinsic index.
fn extract_revive_events(
	block_events: &subxt::events::Events<SrcChainConfig>,
	substrate_block_number: SubstrateBlockNumber,
	eth_block_number: U256,
	eth_block_hash: H256,
	eth_tx_hash_for: impl Fn(usize) -> Option<H256>,
) -> (HashSet<usize>, HashMap<usize, Vec<Log>>) {
	let mut reverted_extrinsics: HashSet<usize> = HashSet::new();
	let mut logs_by_extrinsic: HashMap<usize, Vec<Log>> = HashMap::new();

	for (event_index, event_result) in block_events.iter().enumerate() {
		let event = match event_result {
			Ok(e) => e,
			Err(err) => {
				log::debug!(
					target: LOG_TARGET,
					"Failed to decode event {event_index} in block #{substrate_block_number}: {err:?}"
				);
				continue;
			},
		};

		let extrinsic_index = match event.phase() {
			Phase::ApplyExtrinsic(idx) => idx as usize,
			_ => continue,
		};

		let Some(eth_tx_hash) = eth_tx_hash_for(extrinsic_index) else { continue };

		match decode_revive_event(
			&event,
			eth_block_number,
			eth_tx_hash,
			extrinsic_index,
			eth_block_hash,
		) {
			Some(ReviveEvent::Revert) => {
				reverted_extrinsics.insert(extrinsic_index);
			},
			Some(ReviveEvent::Log(log)) => {
				logs_by_extrinsic.entry(extrinsic_index).or_default().push(log);
			},
			None => {},
		}
	}

	(reverted_extrinsics, logs_by_extrinsic)
}

/// Returns the revive transactions from a block, and whether any extrinsic decode failed.
fn extract_eth_transacts<C: OfflineClientAtBlockT<SrcChainConfig>>(
	block_extrinsics: &Extrinsics<'_, SrcChainConfig, C>,
	block_number: SubstrateBlockNumber,
) -> Result<(Vec<(EthTransact, usize)>, bool), ClientError> {
	let mut extrinsics = Vec::new();
	let mut undecoded_extrinsic = false;
	for (ext_idx, ext) in block_extrinsics.iter().enumerate() {
		let ext = match ext {
			Ok(ext) => ext,
			// Don't error here since the call type is unknown.
			Err(err) => {
				log::debug!(target: LOG_TARGET,
					"Failed to decode extrinsic {ext_idx} of block #{block_number}: {err:?}");
				undecoded_extrinsic = true;
				continue;
			},
		};
		match ext.decode_call_data_fields_as::<EthTransact>() {
			Some(Ok(call)) => extrinsics.push((call, ext_idx)),
			Some(Err(err)) => {
				log::error!(target: LOG_TARGET,
					"Failed to decode the EthTransact call in extrinsic {ext_idx} of block \
					#{block_number}: {err:?}");
				return Err(subxt::Error::from(err).into());
			},
			// Not a revive transaction.
			None => {},
		}
	}

	Ok((extrinsics, undecoded_extrinsic))
}

type FetchReceiptDataFn = Arc<
	dyn Fn(SubstrateBlock) -> Pin<Box<dyn Future<Output = Option<Vec<ReceiptGasInfoV1>>> + Send>>
		+ Send
		+ Sync,
>;

type FetchEthBlockHashFn = Arc<
	dyn Fn(H256, SubstrateBlockNumber) -> Pin<Box<dyn Future<Output = Option<H256>> + Send>>
		+ Send
		+ Sync,
>;

type RecoverEthAddressFn = Arc<dyn Fn(&TransactionSigned) -> Result<H160, ()> + Send + Sync>;

/// Utility to extract receipts from extrinsics.
#[derive(Clone)]
pub struct ReceiptExtractor {
	/// Fetch the receipt data info.
	fetch_receipt_data: FetchReceiptDataFn,

	/// Fetch ethereum block hash.
	fetch_eth_block_hash: FetchEthBlockHashFn,

	/// Auto-discovered first EVM block on the chain.
	/// Set once during backward sync when the first non-EVM block is encountered.
	/// Uses `u64::MAX` as sentinel for "not yet discovered".
	first_evm_block: Arc<AtomicU64>,

	/// Recover the ethereum address from a transaction signature.
	recover_eth_address: RecoverEthAddressFn,
}

impl ReceiptExtractor {
	/// Create a new `ReceiptExtractor`.
	pub async fn new(
		runtime_api_provider: VersionAwareRuntimeApiProvider,
	) -> Result<Self, ClientError> {
		Self::new_with_custom_address_recovery(
			runtime_api_provider,
			Arc::new(|signed_tx: &TransactionSigned| signed_tx.recover_eth_address()),
		)
		.await
	}

	/// Create a new `ReceiptExtractor` with custom Ethereum address recovery logic.
	///
	/// Use `ReceiptExtractor::new` if the default Ethereum address recovery
	/// logic ([`TransactionSigned::recover_eth_address`] based) is enough.
	pub async fn new_with_custom_address_recovery(
		runtime_api_provider: VersionAwareRuntimeApiProvider,
		recover_eth_address_fn: RecoverEthAddressFn,
	) -> Result<Self, ClientError> {
		let provider = runtime_api_provider.clone();
		let fetch_eth_block_hash = Arc::new(move |substrate_block_hash, substrate_block_number| {
			let provider = provider.clone();

			let fut = async move {
				let runtime_api = provider
					.at_block_hash_and_number(substrate_block_hash, substrate_block_number)
					.await
					.inspect_err(|err| {
						log::debug!(
							target: LOG_TARGET,
							"Failed to access the runtime API at block #{substrate_block_number} \
							({substrate_block_hash:?}) for an eth_block_hash query: {err:?}"
						);
					})
					.ok()?;
				runtime_api
					.eth_block_hash(U256::from(substrate_block_number))?
					.await
					.inspect_err(|err| {
						log::debug!(
							target: LOG_TARGET,
							"Failed to query eth_block_hash at block #{substrate_block_number} \
							({substrate_block_hash:?}): {err:?}"
						);
					})
					.ok()
					.flatten()
			};

			Box::pin(fut) as Pin<Box<_>>
		});

		let provider = runtime_api_provider;
		let fetch_receipt_data = Arc::new(move |at_block: SubstrateBlock| {
			let provider = provider.clone();

			let fut = async move {
				let block_hash = at_block.block_hash();
				let runtime_api = provider
					.at_resolved_block(at_block)
					.await
					.inspect_err(|err| {
						log::debug!(
							target: LOG_TARGET,
							"Failed to access the runtime API at block {block_hash:?} for an \
							eth_receipt_data query: {err:?}"
						);
					})
					.ok()?;
				runtime_api
					.eth_receipt_data()?
					.await
					.inspect_err(|err| {
						log::debug!(
							target: LOG_TARGET,
							"Failed to query eth_receipt_data at block {block_hash:?}: {err:?}"
						);
					})
					.ok()
			};

			Box::pin(fut) as Pin<Box<_>>
		});

		Ok(Self {
			fetch_receipt_data,
			fetch_eth_block_hash,
			first_evm_block: Arc::new(AtomicU64::new(u64::MAX)),
			recover_eth_address: recover_eth_address_fn,
		})
	}

	#[cfg(test)]
	pub fn new_mock() -> Self {
		let fetch_receipt_data = Arc::new(|_| Box::pin(std::future::ready(None)) as Pin<Box<_>>);
		// This method is useful when testing eth - substrate mapping.
		let fetch_eth_block_hash =
			Arc::new(|block_hash: H256, block_number: SubstrateBlockNumber| {
				// Generate hash from substrate block hash and number
				let bytes: Vec<u8> = [block_hash.as_bytes(), &block_number.to_be_bytes()].concat();
				let eth_block_hash = H256::from(keccak_256(&bytes));
				Box::pin(std::future::ready(Some(eth_block_hash))) as Pin<Box<_>>
			});

		Self {
			fetch_receipt_data,
			fetch_eth_block_hash,
			first_evm_block: Arc::new(AtomicU64::new(u64::MAX)),
			recover_eth_address: Arc::new(|signed_tx: &TransactionSigned| {
				signed_tx.recover_eth_address()
			}),
		}
	}

	/// Check if the block is before the `first_evm_block` floor.
	/// When sentinel (`u64::MAX`), no blocks are rejected (permissive default).
	pub fn is_before_first_evm_block(&self, block_number: SubstrateBlockNumber) -> bool {
		let val = self.first_evm_block.load(Ordering::Acquire);
		val != u64::MAX && block_number < val
	}

	/// Set the first EVM block. Only stores if lower than the current value.
	pub fn set_first_evm_block(&self, block_number: SubstrateBlockNumber) {
		let prev = self.first_evm_block.fetch_min(block_number, Ordering::AcqRel);
		if block_number > prev {
			log::debug!(target: LOG_TARGET,
				"Ignored attempt to raise first_evm_block to #{block_number}, current is #{prev}");
		}
	}

	/// The auto-discovered first EVM block, or `None` if not yet discovered.
	pub fn first_evm_block(&self) -> Option<SubstrateBlockNumber> {
		let val = self.first_evm_block.load(Ordering::Acquire);
		(val != u64::MAX).then_some(val)
	}

	/// Resolve the Ethereum block hash for a substrate block, falling back to the substrate hash.
	async fn resolve_eth_block_hash(
		&self,
		substrate_block_hash: H256,
		substrate_block_number: SubstrateBlockNumber,
	) -> H256 {
		match (self.fetch_eth_block_hash)(substrate_block_hash, substrate_block_number).await {
			Some(hash) => hash,
			None => {
				log::trace!(target: LOG_TARGET,
					"eth_block_hash returned None for substrate block \
					 #{substrate_block_number} ({substrate_block_hash:?}), \
					 falling back to substrate hash as ETH hash");
				substrate_block_hash
			},
		}
	}

	/// Decode the raw call payload into a [`TransactionSigned`] and construct its [`ReceiptInfo`].
	fn decode_transaction_and_build_receipt(
		&self,
		eth_block_hash: H256,
		block_number: U256,
		call: EthTransact,
		transaction_hash: H256,
		transaction_index: usize,
		receipt_gas_info: ReceiptGasInfoV1,
		reverted: bool,
		logs: Vec<Log>,
	) -> Result<(TransactionSigned, ReceiptInfo), ClientError> {
		let signed_tx =
			TransactionSigned::decode(&call.payload).map_err(|_| ClientError::TxDecodingFailed)?;
		let from = (self.recover_eth_address)(&signed_tx).map_err(|_| {
			log::error!(target: LOG_TARGET, "Failed to recover eth address from signed tx");
			ClientError::RecoverEthAddressFailed
		})?;

		let tx_info = GenericTransaction::from_signed(
			signed_tx.clone(),
			receipt_gas_info.effective_gas_price,
			Some(from),
		);

		let contract_address = if tx_info.to.is_none() {
			Some(create1(
				&from,
				tx_info
					.nonce
					.unwrap_or_default()
					.try_into()
					.map_err(|_| ClientError::ConversionFailed)?,
			))
		} else {
			None
		};

		let receipt = ReceiptInfo::new(
			eth_block_hash,
			block_number,
			contract_address,
			from,
			logs,
			tx_info.to,
			receipt_gas_info.effective_gas_price,
			U256::from(receipt_gas_info.gas_used),
			!reverted,
			transaction_hash,
			transaction_index.into(),
			tx_info.r#type.unwrap_or_default(),
		);
		Ok((signed_tx, receipt))
	}

	/// Extract receipts from block.
	pub async fn extract_from_block(
		&self,
		block: &SubstrateBlock,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		let eth_block_hash =
			self.resolve_eth_block_hash(block.block_hash(), block.block_number()).await;

		self.extract_from_block_with_eth_hash(block, eth_block_hash).await
	}

	/// Extract receipts from block, using a pre-fetched ethereum block hash.
	///
	/// Fetches block events once in a single pass before building receipts.
	pub async fn extract_from_block_with_eth_hash(
		&self,
		block: &SubstrateBlock,
		eth_block_hash: H256,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		if self.is_before_first_evm_block(block.block_number()) {
			return Ok(vec![]);
		}

		let eth_tx_by_index: BTreeMap<usize, (EthTransact, H256, ReceiptGasInfoV1)> = self
			.get_block_extrinsics(block)
			.await?
			.map(|(call, receipt_gas_info, extrinsic_index)| {
				let hash = H256(keccak_256(&call.payload));
				(extrinsic_index, (call, hash, receipt_gas_info))
			})
			.collect();

		if eth_tx_by_index.is_empty() {
			return Ok(vec![]);
		}

		let substrate_block_number = block.block_number();
		let eth_block_number: U256 = substrate_block_number.into();
		let block_events = block.events().fetch().await.inspect_err(|err| {
			log::debug!(target: LOG_TARGET, "Error fetching events for block #{substrate_block_number}: {err:?}");
		})?;
		let (reverted_extrinsics, mut logs_by_extrinsic) = extract_revive_events(
			&block_events,
			substrate_block_number,
			eth_block_number,
			eth_block_hash,
			|idx| eth_tx_by_index.get(&idx).map(|(_, hash, _)| *hash),
		);

		eth_tx_by_index
			.into_iter()
			.map(|(transaction_index, (call, transaction_hash, receipt_gas_info))| {
				let reverted = reverted_extrinsics.contains(&transaction_index);
				let logs = logs_by_extrinsic.remove(&transaction_index).unwrap_or_default();
				self.decode_transaction_and_build_receipt(
					eth_block_hash,
					eth_block_number,
					call,
					transaction_hash,
					transaction_index,
					receipt_gas_info,
					reverted,
					logs,
				)
				.inspect_err(|err| {
					log::warn!(target: LOG_TARGET, "Error extracting extrinsic: {err:?}");
				})
			})
			.collect()
	}

	/// Return the ETH extrinsics of the block grouped with reconstruction receipt info and
	/// extrinsic index
	async fn get_block_extrinsics(
		&self,
		block: &SubstrateBlock,
	) -> Result<impl Iterator<Item = (EthTransact, ReceiptGasInfoV1, usize)>, ClientError> {
		let block_extrinsics = block.extrinsics().fetch().await.inspect_err(|err| {
			log::debug!(target: LOG_TARGET, "Error fetching for #{:?} extrinsics: {err:?}", block.block_number());
		})?;

		let block_number = block.block_number();
		let (extrinsics, undecoded_extrinsic) =
			extract_eth_transacts(&block_extrinsics, block_number)?;

		// Skip the runtime query for blocks with no revive extrinsics.
		let receipt_data = if extrinsics.is_empty() && !undecoded_extrinsic {
			Vec::new()
		} else {
			(self.fetch_receipt_data)(block.clone()).await.ok_or_else(|| {
				log::trace!(target: LOG_TARGET,
				"Receipt data not found for block #{} ({:?})",
				block.block_number(), block.block_hash());
				ClientError::ReceiptDataNotFound
			})?
		};

		// Sanity check we received enough data from the pallet revive.
		if receipt_data.len() != extrinsics.len() {
			log::error!(
				target: LOG_TARGET,
				"Receipt data length ({}) does not match extrinsics length ({})",
				receipt_data.len(),
				extrinsics.len()
			);
			Err(ClientError::ReceiptDataLengthMismatch)
		} else {
			Ok(extrinsics
				.into_iter()
				.zip(receipt_data)
				.map(|((call, ext_idx), rec)| (call, rec, ext_idx)))
		}
	}

	/// Extract a [`TransactionSigned`] and a [`ReceiptInfo`] for a specific transaction in a
	/// [`SubstrateBlock`]
	pub async fn extract_from_transaction(
		&self,
		block: &SubstrateBlock,
		transaction_index: usize,
	) -> Result<(TransactionSigned, ReceiptInfo), ClientError> {
		let (eth_call, receipt_gas_info, transaction_hash) = self
			.get_block_extrinsics(block)
			.await?
			.find_map(|(call, receipt_gas_info, extrinsic_index)| {
				(extrinsic_index == transaction_index).then(|| {
					let hash = H256(keccak_256(&call.payload));
					(call, receipt_gas_info, hash)
				})
			})
			.ok_or_else(|| {
				log::trace!(target: LOG_TARGET,
					"extract_from_transaction: no EVM extrinsic at tx_index {transaction_index} \
					 in block #{} ({:?})", block.block_number(), block.block_hash());
				ClientError::EthExtrinsicNotFound
			})?;

		let substrate_block_number = block.block_number();
		let eth_block_number: U256 = substrate_block_number.into();
		let eth_block_hash =
			self.resolve_eth_block_hash(block.block_hash(), substrate_block_number).await;
		let block_events = block.events().fetch().await.inspect_err(|err| {
			log::debug!(target: LOG_TARGET, "Error fetching events for block #{substrate_block_number}: {err:?}");
		})?;
		let (reverted_extrinsics, mut logs_by_extrinsic) = extract_revive_events(
			&block_events,
			substrate_block_number,
			eth_block_number,
			eth_block_hash,
			|idx| (idx == transaction_index).then_some(transaction_hash),
		);

		let reverted = reverted_extrinsics.contains(&transaction_index);
		let logs = logs_by_extrinsic.remove(&transaction_index).unwrap_or_default();
		self.decode_transaction_and_build_receipt(
			eth_block_hash,
			eth_block_number,
			eth_call,
			transaction_hash,
			transaction_index,
			receipt_gas_info,
			reverted,
			logs,
		)
	}

	/// Get the Ethereum block hash for the Substrate block with specific hash.
	pub async fn get_ethereum_block_hash(
		&self,
		block_hash: &H256,
		block_number: SubstrateBlockNumber,
	) -> Option<H256> {
		(self.fetch_eth_block_hash)(*block_hash, block_number).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use pallet_revive::evm::{Account, TransactionLegacyUnsigned, TransactionUnsigned};

	fn signed_call(account: &Account, tx: TransactionUnsigned) -> (EthTransact, H256) {
		let payload = account.sign_transaction(tx).signed_payload();
		let hash = H256(keccak_256(&payload));
		(EthTransact { payload }, hash)
	}

	fn legacy_call_tx(to: H160) -> TransactionUnsigned {
		TransactionUnsigned::from(TransactionLegacyUnsigned {
			chain_id: Some(U256::from(1)),
			to: Some(to),
			gas: U256::from(21_000),
			..Default::default()
		})
	}

	fn gas_info() -> ReceiptGasInfoV1 {
		ReceiptGasInfoV1 {
			gas_used: U256::from(21_000),
			effective_gas_price: U256::from(1_000_000_000),
		}
	}

	#[test]
	fn build_receipt_for_call() {
		let extractor = ReceiptExtractor::new_mock();
		let account = Account::default();
		let eth_block_hash = H256::from([0xAB; 32]);
		let block_number = U256::from(42);
		let (call, tx_hash) = signed_call(&account, legacy_call_tx(account.address()));

		// Successful call
		let (signed_tx, receipt) = extractor
			.decode_transaction_and_build_receipt(
				eth_block_hash,
				block_number,
				call,
				tx_hash,
				3,
				gas_info(),
				false,
				vec![],
			)
			.unwrap();

		assert!(receipt.is_success());
		assert_eq!(receipt.from, account.address());
		assert_eq!(receipt.to, Some(account.address()));
		assert_eq!(receipt.contract_address, None);
		assert_eq!(receipt.block_hash, eth_block_hash);
		assert_eq!(receipt.block_number, block_number);
		assert_eq!(receipt.transaction_hash, tx_hash);
		assert_eq!(receipt.transaction_index, U256::from(3));
		assert_eq!(receipt.gas_used, U256::from(21_000));
		assert_eq!(signed_tx.recover_eth_address().unwrap(), account.address());

		// Same call, but reverted
		let (call, tx_hash) = signed_call(&account, legacy_call_tx(account.address()));
		let (_, receipt) = extractor
			.decode_transaction_and_build_receipt(
				eth_block_hash,
				block_number,
				call,
				tx_hash,
				3,
				gas_info(),
				true,
				vec![],
			)
			.unwrap();

		assert!(!receipt.is_success());
		assert_eq!(receipt.from, account.address());
	}

	#[test]
	fn build_receipt_for_deploy() {
		let extractor = ReceiptExtractor::new_mock();
		let account = Account::default();
		let deploy_tx = TransactionUnsigned::from(TransactionLegacyUnsigned {
			chain_id: Some(U256::from(1)),
			gas: U256::from(100_000),
			nonce: U256::from(0),
			..Default::default()
		});
		let (call, tx_hash) = signed_call(&account, deploy_tx);

		let (_, receipt) = extractor
			.decode_transaction_and_build_receipt(
				H256::zero(),
				U256::from(1),
				call,
				tx_hash,
				0,
				gas_info(),
				false,
				vec![],
			)
			.unwrap();

		assert!(receipt.is_success());
		assert_eq!(receipt.to, None);
		assert_eq!(receipt.contract_address, Some(create1(&account.address(), 0)));
		assert_eq!(receipt.from, account.address());
	}

	#[test]
	fn build_receipt_rejects_invalid_payload() {
		let extractor = ReceiptExtractor::new_mock();

		// Corrupt payload
		let call = EthTransact { payload: vec![0xde, 0xad] };
		let hash = H256(keccak_256(&call.payload));
		let err = extractor
			.decode_transaction_and_build_receipt(
				H256::zero(),
				U256::from(1),
				call,
				hash,
				0,
				gas_info(),
				false,
				vec![],
			)
			.unwrap_err();
		assert!(matches!(err, ClientError::TxDecodingFailed));

		// Valid payload but address recovery fails
		let extractor = ReceiptExtractor {
			recover_eth_address: Arc::new(|_| Err(())),
			..ReceiptExtractor::new_mock()
		};
		let account = Account::default();
		let (call, hash) = signed_call(&account, legacy_call_tx(account.address()));
		let err = extractor
			.decode_transaction_and_build_receipt(
				H256::zero(),
				U256::from(1),
				call,
				hash,
				0,
				gas_info(),
				false,
				vec![],
			)
			.unwrap_err();
		assert!(matches!(err, ClientError::RecoverEthAddressFailed));
	}

	#[test]
	fn defaults_and_first_evm_block_only_decreases() {
		let extractor = ReceiptExtractor::new_mock();

		assert!(extractor.first_evm_block().is_none());

		// first_evm_block only decreases
		extractor.set_first_evm_block(100);
		assert_eq!(extractor.first_evm_block(), Some(100));

		extractor.set_first_evm_block(50);
		assert_eq!(extractor.first_evm_block(), Some(50));

		// Higher value is ignored
		extractor.set_first_evm_block(100);
		assert_eq!(extractor.first_evm_block(), Some(50));
	}

	use crate::block_info_provider::test::chain_config;
	use codec::{Compact, Encode};
	use frame_system::EventRecord;
	use revive_dev_runtime::{Runtime, RuntimeEvent};
	use subxt::{PolkadotConfig, client::OfflineClient, events::Events};

	/// An offline client carrying the generated runtime metadata for every block.
	fn offline_client() -> OfflineClient<PolkadotConfig> {
		OfflineClient::<PolkadotConfig>::new_with_config(chain_config())
	}

	/// Build `Events` by SCALE-encoding revive events against the generated runtime metadata.
	struct EventsBuilder {
		bytes: Vec<u8>,
		count: u32,
	}

	impl EventsBuilder {
		fn new() -> Self {
			Self { bytes: Vec::new(), count: 0 }
		}

		fn push_event(
			mut self,
			phase: frame_system::Phase,
			event: pallet_revive::Event<Runtime>,
		) -> Self {
			EventRecord::<RuntimeEvent, H256> {
				phase,
				event: RuntimeEvent::Revive(event),
				topics: vec![],
			}
			.encode_to(&mut self.bytes);
			self.count += 1;
			self
		}

		fn build(self) -> Events<SrcChainConfig> {
			let mut encoded_events = Vec::new();
			Compact(self.count).encode_to(&mut encoded_events);
			encoded_events.extend(self.bytes);

			let client = offline_client();
			let at_block =
				client.at_block(0u64).expect("spec version range covers all block numbers; qed");
			at_block.events().from_bytes(encoded_events)
		}
	}

	#[test]
	fn extract_revive_events_decodes_contract_emitted_log() {
		let contract = H160::from([0x11; 20]);
		let topics = vec![H256::from([0x22; 32]), H256::from([0x33; 32])];
		let data = vec![0xde, 0xad, 0xbe, 0xef];
		let events = EventsBuilder::new()
			.push_event(
				frame_system::Phase::ApplyExtrinsic(5),
				pallet_revive::Event::ContractEmitted {
					contract,
					data: data.clone(),
					topics: topics.clone(),
				},
			)
			.build();

		let tx_hash = H256::from([0xAA; 32]);
		let eth_block_hash = H256::from([0xBB; 32]);
		let substrate_block_number = 42u64;
		let eth_block_number = U256::from(substrate_block_number);

		let (reverts, logs) = extract_revive_events(
			&events,
			substrate_block_number,
			eth_block_number,
			eth_block_hash,
			|idx| (idx == 5).then_some(tx_hash),
		);

		assert!(reverts.is_empty());
		assert_eq!(logs.len(), 1);
		let log = &logs[&5][0];
		assert_eq!(log.address, contract);
		assert_eq!(log.topics, topics);
		assert_eq!(log.data.as_ref().unwrap().0, data);
		assert_eq!(log.block_hash, eth_block_hash);
		assert_eq!(log.block_number, eth_block_number);
		assert_eq!(log.transaction_hash, tx_hash);
		assert_eq!(log.transaction_index, U256::from(5));
	}

	#[test]
	fn extract_revive_events_skips_irrelevant_events() {
		// Events outside `ApplyExtrinsic` and events for extrinsics the tx-hash closure
		// doesn't resolve are both dropped.
		let empty_contract_emitted = pallet_revive::Event::ContractEmitted {
			contract: H160::zero(),
			data: vec![],
			topics: vec![],
		};
		let revert = pallet_revive::Event::EthExtrinsicRevert {
			dispatch_error: sp_runtime::DispatchError::Other("skipped-phase revert"),
		};
		let events = EventsBuilder::new()
			.push_event(frame_system::Phase::Finalization, empty_contract_emitted.clone())
			.push_event(frame_system::Phase::Initialization, revert.clone())
			.push_event(frame_system::Phase::ApplyExtrinsic(5), empty_contract_emitted)
			.push_event(frame_system::Phase::ApplyExtrinsic(5), revert)
			.build();

		// The tx-hash closure returns `Some` only for extrinsic 7 (not present)
		let (reverts, logs) =
			extract_revive_events(&events, 0, U256::zero(), H256::zero(), |idx| {
				(idx == 7).then_some(H256::zero())
			});

		assert!(reverts.is_empty());
		assert!(logs.is_empty());
	}

	#[test]
	fn extract_revive_events_accumulates_per_extrinsic() {
		let tx0 = H256::from([0x01; 32]);
		let tx1 = H256::from([0x02; 32]);
		let tx2 = H256::from([0x03; 32]);
		let emitted_by = |contract: H160| pallet_revive::Event::ContractEmitted {
			contract,
			data: vec![],
			topics: vec![],
		};
		let events = EventsBuilder::new()
			.push_event(frame_system::Phase::ApplyExtrinsic(0), emitted_by(H160::from([0xaa; 20])))
			.push_event(frame_system::Phase::ApplyExtrinsic(0), emitted_by(H160::from([0xbb; 20])))
			.push_event(
				frame_system::Phase::ApplyExtrinsic(1),
				pallet_revive::Event::EthExtrinsicRevert {
					dispatch_error: sp_runtime::DispatchError::Other("tx-1 revert"),
				},
			)
			.push_event(frame_system::Phase::ApplyExtrinsic(2), emitted_by(H160::from([0xcc; 20])))
			.build();

		let (reverts, logs) =
			extract_revive_events(&events, 0, U256::zero(), H256::zero(), |idx| match idx {
				0 => Some(tx0),
				1 => Some(tx1),
				2 => Some(tx2),
				_ => None,
			});

		assert_eq!(reverts, [1usize].into_iter().collect::<HashSet<_>>());
		assert_eq!(logs[&0].len(), 2);
		assert_eq!(logs[&2].len(), 1);
		// log_index is block-wide
		assert_eq!(logs[&0][0].log_index, U256::from(0));
		assert_eq!(logs[&0][1].log_index, U256::from(1));
		assert_eq!(logs[&2][0].log_index, U256::from(3));
	}

	const ETH_TRANSACT_PAYLOAD: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

	/// SCALE-encode a bare extrinsic the way a block body carries it, length prefix included.
	fn encode_bare(call: revive_dev_runtime::RuntimeCall) -> Vec<u8> {
		let extrinsic: revive_dev_runtime::UncheckedExtrinsic =
			pallet_revive::evm::runtime::UncheckedExtrinsic(
				sp_runtime::generic::UncheckedExtrinsic::new_bare(call),
			);
		extrinsic.encode()
	}

	fn eth_transact_extrinsic() -> Vec<u8> {
		encode_bare(revive_dev_runtime::RuntimeCall::Revive(pallet_revive::Call::eth_transact {
			payload: ETH_TRANSACT_PAYLOAD.to_vec(),
		}))
	}

	fn non_revive_extrinsic() -> Vec<u8> {
		encode_bare(revive_dev_runtime::RuntimeCall::System(frame_system::Call::remark {
			remark: vec![0x01],
		}))
	}

	/// Run the extraction over a synthetic block body.
	async fn extract_from(
		blobs: Vec<Vec<u8>>,
	) -> Result<(Vec<(EthTransact, usize)>, bool), ClientError> {
		const BLOCK_NUMBER: SubstrateBlockNumber = 42;

		let client = offline_client();
		let at_block = client
			.at_block(BLOCK_NUMBER)
			.expect("spec version range covers every block number; qed");
		let extrinsics = at_block.extrinsics().from_bytes(blobs).await;
		extract_eth_transacts(&extrinsics, BLOCK_NUMBER)
	}

	#[tokio::test]
	async fn extract_eth_transacts_collects_revive_calls() {
		let (calls, undecoded) =
			extract_from(vec![non_revive_extrinsic(), eth_transact_extrinsic()])
				.await
				.unwrap();

		assert!(!undecoded, "every extrinsic decoded");
		assert_eq!(calls.len(), 1, "only the revive extrinsic is collected");
		assert_eq!(calls[0].1, 1, "the extrinsic index is preserved");
		assert_eq!(calls[0].0.payload, ETH_TRANSACT_PAYLOAD, "the call fields are decoded");
	}

	#[tokio::test]
	async fn extract_eth_transacts_keeps_revive_calls_next_to_an_undecodable_one() {
		let (calls, undecoded) =
			extract_from(vec![vec![0xff; 4], eth_transact_extrinsic()]).await.unwrap();

		assert_eq!(calls.len(), 1, "an undecodable extrinsic must not hide a decoded one");
		assert_eq!(calls[0].1, 1, "the extrinsic index is preserved");
		assert_eq!(calls[0].0.payload, ETH_TRANSACT_PAYLOAD, "the call fields are decoded");
		assert!(undecoded, "it may be a revive one, so report it");
	}
}
