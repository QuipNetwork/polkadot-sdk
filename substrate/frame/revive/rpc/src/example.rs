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
//! Example utilities
use crate::{EthRpcClient, ReceiptInfo};
use anyhow::Context;
use pallet_revive::evm::*;
use pallet_revive_types::runtime_api::GenericTransactionV1;
use std::sync::Arc;

/// Transaction type enum for specifying which type of transaction to send
#[derive(Debug, Clone, Copy)]
pub enum TransactionType {
	Legacy,
	Eip2930,
	Eip1559,
	Eip4844,
}

/// Transaction builder.
pub struct TransactionBuilder<Client: EthRpcClient + Sync + Send> {
	client: Arc<Client>,
	signer: Account,
	value: U256,
	input: Bytes,
	to: Option<H160>,
	nonce: Option<U256>,
	gas: Option<U256>,
	mutate: Box<dyn FnOnce(&mut TransactionUnsigned)>,
}

#[derive(Debug)]
pub struct SubmittedTransaction<Client: EthRpcClient + Sync + Send> {
	tx: GenericTransaction,
	hash: H256,
	client: Arc<Client>,
}

impl<Client: EthRpcClient + Sync + Send> SubmittedTransaction<Client> {
	/// Get the hash of the transaction.
	pub fn hash(&self) -> H256 {
		self.hash
	}

	/// The gas limit sent with the transaction, if one was specified.
	///
	/// This mirrors [`GenericTransaction::gas`], which is optional for legacy/incomplete
	/// payloads, so it can be `None`.
	pub fn gas(&self) -> Option<U256> {
		self.tx.gas
	}

	pub fn generic_transaction(&self) -> GenericTransaction {
		self.tx.clone()
	}

	/// Wait for the receipt regardless of success or failure status.
	pub async fn wait_for_receipt_any(&self) -> anyhow::Result<ReceiptInfo> {
		let hash = self.hash();
		for _ in 0..30 {
			tokio::time::sleep(std::time::Duration::from_secs(2)).await;
			if let Some(receipt) = self.client.get_transaction_receipt(hash).await? {
				return Ok(receipt);
			}
		}
		anyhow::bail!("Timeout, failed to get receipt for {hash:?}")
	}

	/// Wait for the receipt and assert the transaction succeeded.
	pub async fn wait_for_receipt(&self) -> anyhow::Result<ReceiptInfo> {
		let receipt = self.wait_for_receipt_any().await?;
		if receipt.is_success() {
			if let Some(gas) = self.gas() {
				assert!(
					gas >= receipt.gas_used,
					"Gas used {:?} should be less than or equal to gas limit {:?}",
					receipt.gas_used,
					gas
				);
			}
			Ok(receipt)
		} else {
			anyhow::bail!("Transaction failed receipt: {receipt:?}")
		}
	}
}

impl<Client: EthRpcClient + Send + Sync> TransactionBuilder<Client> {
	pub fn new(client: Arc<Client>) -> Self {
		Self {
			client,
			signer: Account::default(),
			value: U256::zero(),
			input: Bytes::default(),
			to: None,
			nonce: None,
			gas: None,
			mutate: Box::new(|_| {}),
		}
	}
	/// Set the signer.
	pub fn signer(mut self, signer: Account) -> Self {
		self.signer = signer;
		self
	}

	/// Set the value.
	pub fn value(mut self, value: U256) -> Self {
		self.value = value;
		self
	}

	/// Set the input.
	pub fn input(mut self, input: Vec<u8>) -> Self {
		self.input = Bytes(input);
		self
	}

	/// Set the destination.
	pub fn to(mut self, to: H160) -> Self {
		self.to = Some(to);
		self
	}

	/// Set the nonce.
	pub fn nonce(mut self, nonce: U256) -> Self {
		self.nonce = Some(nonce);
		self
	}

	/// Set the gas limit explicitly, skipping eth_estimateGas.
	pub fn gas(mut self, gas: U256) -> Self {
		self.gas = Some(gas);
		self
	}

	/// Set a mutation function, that mutates the transaction before sending.
	pub fn mutate(mut self, mutate: impl FnOnce(&mut TransactionUnsigned) + 'static) -> Self {
		self.mutate = Box::new(mutate);
		self
	}

	/// Call eth_call to get the result of a view function
	pub async fn eth_call(self) -> anyhow::Result<Vec<u8>> {
		let TransactionBuilder { client, signer, value, input, to, .. } = self;

		let from = signer.address();
		let result = client
			.call(
				GenericTransactionV1 {
					from: Some(from),
					input: input.into(),
					value: Some(value),
					to,
					..Default::default()
				},
				None,
				None,
			)
			.await
			.map_err(|e| anyhow::anyhow!("eth_call failed: {e}"))?;
		Ok(result.0)
	}

	/// Send the transaction.
	pub async fn send(self) -> anyhow::Result<SubmittedTransaction<Client>> {
		self.send_with_type(TransactionType::Legacy).await
	}

	/// Send the transaction with a specific transaction type.
	pub async fn send_with_type(
		self,
		tx_type: TransactionType,
	) -> anyhow::Result<SubmittedTransaction<Client>> {
		let TransactionBuilder { client, signer, value, input, to, nonce, gas, mutate } = self;

		let from = signer.address();
		let chain_id = client.chain_id().await?;
		let gas_price = client.gas_price().await?;
		let nonce = if let Some(nonce) = nonce {
			nonce
		} else {
			client
				.get_transaction_count(from, Default::default())
				.await
				.with_context(|| "Failed to fetch account nonce")?
		};

		let gas = if let Some(gas) = gas {
			gas
		} else {
			client
				.estimate_gas(
					GenericTransactionV1 {
						from: Some(from),
						input: input.clone().into(),
						value: Some(value),
						gas_price: Some(gas_price),
						to,
						..Default::default()
					},
					None,
				)
				.await
				.with_context(|| "Failed to fetch gas estimate")?
		};

		println!("Gas estimate: {gas:?}");

		let mut unsigned_tx: TransactionUnsigned = match tx_type {
			TransactionType::Legacy => TransactionLegacyUnsigned {
				gas,
				nonce,
				to,
				value,
				input,
				gas_price,
				chain_id: Some(chain_id),
				..Default::default()
			}
			.into(),
			TransactionType::Eip2930 => Transaction2930Unsigned {
				gas,
				nonce,
				to,
				value,
				input,
				gas_price,
				chain_id,
				access_list: vec![],
				r#type: TypeEip2930,
			}
			.into(),
			TransactionType::Eip1559 => Transaction1559Unsigned {
				gas,
				nonce,
				to,
				value,
				input,
				gas_price,
				max_fee_per_gas: gas_price,
				max_priority_fee_per_gas: U256::zero(),
				chain_id,
				access_list: vec![],
				r#type: TypeEip1559,
			}
			.into(),
			TransactionType::Eip4844 => {
				// For EIP-4844, we need a destination address (cannot be None for blob
				// transactions)
				let to = to.ok_or_else(|| {
					anyhow::anyhow!("EIP-4844 transactions require a destination address")
				})?;
				let max_priority_fee_per_gas = gas_price / 10; // 10% of gas price as priority fee
				Transaction4844Unsigned {
					gas,
					nonce,
					to,
					value,
					input,
					max_fee_per_gas: gas_price,
					max_priority_fee_per_gas,
					max_fee_per_blob_gas: gas_price, // Use gas_price as blob gas fee
					chain_id,
					access_list: vec![],
					blob_versioned_hashes: vec![],
					r#type: TypeEip4844,
				}
				.into()
			},
		};
		mutate(&mut unsigned_tx);

		let signed_tx = signer.sign_transaction(unsigned_tx);
		let bytes = signed_tx.signed_payload();

		let hash = client
			.send_raw_transaction(bytes.into())
			.await
			.with_context(|| "send_raw_transaction failed")?;

		Ok(SubmittedTransaction {
			tx: GenericTransaction::from_signed(signed_tx, gas_price, Some(from)),
			hash,
			client,
		})
	}
}

#[test]
fn test_dummy_payload_has_correct_len() {
	let signer = Account::from(subxt_signer::eth::dev::ethan());
	let unsigned_tx: TransactionUnsigned =
		TransactionLegacyUnsigned { input: vec![42u8; 100].into(), ..Default::default() }.into();

	let signed_tx = signer.sign_transaction(unsigned_tx.clone());
	let signed_payload = signed_tx.signed_payload();
	let unsigned_tx = signed_tx.unsigned();

	let dummy_payload = unsigned_tx.dummy_signed_payload();
	assert_eq!(dummy_payload.len(), signed_payload.len());
}
