// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope};
use alloy_primitives::{Address, Bytes, Log as AlloyLog, B256};
use alloy_sol_types::{SolEvent, SolValue};
use emulated_integration_tests_common::snowbridge::WETH;
use snowbridge_inbound_queue_primitives::v2::IGatewayV2;

#[test]
fn forged_receipt_proof_is_rejected_after_path_check_fix() {
	let gateway = Address::from_slice(&[0x42; 20]);
	let token_value: u128 = 2_000_000_000_000u128;
	let sig = IGatewayV2::OutboundMessageAccepted::SIGNATURE_HASH;
	let topics = vec![sp_core::H256::from_slice(sig.as_slice())];

	let log_data = IGatewayV2::OutboundMessageAccepted {
		nonce: 1337u64,
		payload: IGatewayV2::Payload {
			origin: Address::from_slice(&[0x11; 20]),
			assets: vec![IGatewayV2::EthereumAsset {
				kind: 0,
				data: IGatewayV2::AsNativeTokenERC20 {
					token_id: Address::from_slice(&WETH),
					value: token_value,
				}
				.abi_encode()
				.into(),
			}],
			xcm: IGatewayV2::Xcm { kind: 0, data: vec![1, 2, 3].into() },
			claimer: Bytes::new(),
			value: 0,
			executionFee: 1,
			relayerFee: 0,
		},
	}
	.encode_data();

	let forged_receipt_log = AlloyLog::new_unchecked(
		gateway,
		vec![B256::from_slice(sig.as_slice())],
		Bytes::copy_from_slice(&log_data),
	);
	let forged_receipt = ReceiptEnvelope::Legacy(
		Receipt {
			status: Eip658Value::success(),
			cumulative_gas_used: 0,
			logs: vec![forged_receipt_log],
		}
		.with_bloom(),
	);
	let forged_receipt_bytes = alloy_rlp::encode(&forged_receipt);

	let fixture = snowbridge_pallet_ethereum_client_fixtures::make_inbound_fixture();
	let receipts_root = fixture.event.proof.execution_proof.execution_header.receipts_root();
	let root_node = fixture.event.proof.receipt_proof[0].clone();
	let exploit_proof_nodes = vec![root_node, forged_receipt_bytes];

	for tx_index in 0u64..10_000u64 {
		assert!(
			snowbridge_inbound_queue_primitives::receipt::verify_receipt_proof(
				receipts_root,
				tx_index,
				&exploit_proof_nodes,
			)
			.is_none(),
			"forged proof unexpectedly verified at tx_index={tx_index}"
		);
	}

	let _ = topics;
}
