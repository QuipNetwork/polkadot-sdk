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

use super::*;
use crate::mock::{
	new_test_ext, precompile_address, AssetConversion as AssetConversionPallet, Assets,
	NativeAndAssets, RuntimeOrigin, Test,
};
use alloy::primitives::U256;
use codec::Encode;
use frame_support::{
	assert_ok,
	traits::{fungibles::Inspect, tokens::fungible::NativeOrWithId},
};
use pallet_revive::{
	precompiles::{alloy::sol_types::SolCall, TransactionLimits},
	AddressMapper, Code, ExecConfig,
};
use sp_runtime::Weight;

/// SCALE-encode asset kinds for use in precompile calls.
fn encode_native() -> Vec<u8> {
	NativeOrWithId::<u32>::Native.encode()
}

fn encode_asset(id: u32) -> Vec<u8> {
	NativeOrWithId::<u32>::WithId(id).encode()
}

/// Convert an account id to an alloy Address.
fn account_addr(id: &u64) -> alloy::primitives::Address {
	let h160 = <Test as pallet_revive::Config>::AddressMapper::to_address(id);
	alloy::primitives::Address::from(h160.0)
}

/// Helper: set up asset 1, create a pool (Native <-> Asset1), and add liquidity.
fn setup_pool(provider: u64, native_amount: u64, asset_amount: u64) {
	let asset_id = 1u32;
	let native = NativeOrWithId::Native;
	let token = NativeOrWithId::WithId(asset_id);

	// Create asset.
	assert_ok!(Assets::force_create(RuntimeOrigin::root(), asset_id, provider, true, 1));
	// Mint more than needed: add_liquidity will reserve AssetAccountDeposit when creating
	// the pool's asset account, so the provider needs balance beyond the liquidity amount.
	assert_ok!(
		Assets::mint(RuntimeOrigin::signed(provider), asset_id, provider, asset_amount * 2,)
	);

	// Create pool.
	assert_ok!(AssetConversionPallet::create_pool(
		RuntimeOrigin::signed(provider),
		Box::new(native.clone()),
		Box::new(token.clone()),
	));
	// Add liquidity.
	assert_ok!(AssetConversionPallet::add_liquidity(
		RuntimeOrigin::signed(provider),
		Box::new(native),
		Box::new(token),
		native_amount,
		asset_amount,
		0,
		0,
		provider,
	));
}

/// Helper: call the precompile via `bare_call` and return the result.
fn bare_call(
	caller: u64,
	data: Vec<u8>,
) -> pallet_revive::ContractResult<pallet_revive::ExecReturnValue, u64> {
	pallet_revive::Pallet::<Test>::bare_call(
		RuntimeOrigin::signed(caller),
		precompile_address(),
		0u64.into(),
		TransactionLimits::WeightAndDeposit { weight_limit: Weight::MAX, deposit_limit: u64::MAX },
		data,
		&ExecConfig::new_substrate_tx(),
	)
}

/// Check if a bare_call result failed (either error or revert).
fn did_fail(result: &pallet_revive::ContractResult<pallet_revive::ExecReturnValue, u64>) -> bool {
	result.result.is_err() || result.result.as_ref().map_or(false, |v| v.did_revert())
}

#[test]
fn swap_exact_tokens_for_tokens_works() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;
		let swapper = 2u64;
		let recipient = 3u64;

		setup_pool(provider, 10_000, 10_000);
		assert_ok!(Assets::mint(RuntimeOrigin::signed(provider), 1, swapper, 1_000));

		let swapper_asset1_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &swapper);
		let recipient_native_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &recipient);

		let data = IAssetConversion::swapExactTokensForTokensCall {
			path: vec![encode_asset(1).into(), encode_native().into()],
			amountIn: U256::from(100),
			amountOutMin: U256::from(1),
			sendTo: account_addr(&recipient),
			keepAlive: false,
		}
		.abi_encode();

		let result = bare_call(swapper, data);
		let return_data = result.result.expect("swap must succeed");
		assert!(!return_data.did_revert(), "swap must not revert");

		let amount_out =
			IAssetConversion::swapExactTokensForTokensCall::abi_decode_returns(&return_data.data)
				.expect("return data must decode");
		assert!(amount_out > U256::ZERO, "must receive some tokens");

		let swapper_asset1_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &swapper);
		assert_eq!(swapper_asset1_before - swapper_asset1_after, 100);

		let recipient_native_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &recipient);
		assert_eq!(
			U256::from(recipient_native_after - recipient_native_before),
			amount_out,
			"received amount must match return value"
		);
	});
}

#[test]
fn swap_tokens_for_exact_tokens_works() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;
		let swapper = 2u64;

		setup_pool(provider, 10_000, 10_000);
		assert_ok!(Assets::mint(RuntimeOrigin::signed(provider), 1, swapper, 1_000));

		let swapper_native_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &swapper);

		// Swap native -> asset1, requesting exactly 50 asset1 output.
		let data = IAssetConversion::swapTokensForExactTokensCall {
			path: vec![encode_native().into(), encode_asset(1).into()],
			amountOut: U256::from(50),
			amountInMax: U256::from(10_000),
			sendTo: account_addr(&swapper),
			keepAlive: false,
		}
		.abi_encode();

		let result = bare_call(swapper, data);
		let return_data = result.result.expect("swap must succeed");
		assert!(!return_data.did_revert(), "swap must not revert");

		let amount_in =
			IAssetConversion::swapTokensForExactTokensCall::abi_decode_returns(&return_data.data)
				.expect("return data must decode");
		assert!(amount_in > U256::ZERO, "must spend some tokens");

		// Verify recipient got exactly 50 asset1.
		let swapper_asset1_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &swapper);
		assert_eq!(swapper_asset1_after, 1_050, "swapper must receive exactly 50 asset1");

		let swapper_native_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &swapper);
		assert_eq!(
			U256::from(swapper_native_before - swapper_native_after),
			amount_in,
			"spent native must match return value"
		);
	});
}

#[test]
fn quote_exact_tokens_for_tokens_works() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;

		setup_pool(provider, 10_000, 10_000);

		let data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: encode_asset(1).into(),
			asset2: encode_native().into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let result = bare_call(provider, data);
		let return_data = result.result.expect("quote must succeed");
		assert!(!return_data.did_revert(), "quote must not revert");

		let quoted =
			IAssetConversion::quoteExactTokensForTokensCall::abi_decode_returns(&return_data.data)
				.expect("return data must decode");

		// With 10000/10000 pool and 0.3% fee, swapping 100 asset1:
		// amount_out = (100 * 997 * 10000) / (10000 * 1000 + 100 * 997) = 98
		assert_eq!(quoted, U256::from(98), "quoted amount must match expected AMM output");
	});
}

#[test]
fn quote_tokens_for_exact_tokens_works() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;

		setup_pool(provider, 10_000, 10_000);

		let data = IAssetConversion::quoteTokensForExactTokensCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let result = bare_call(provider, data);
		let return_data = result.result.expect("quote must succeed");
		assert!(!return_data.did_revert(), "quote must not revert");

		let quoted =
			IAssetConversion::quoteTokensForExactTokensCall::abi_decode_returns(&return_data.data)
				.expect("return data must decode");
		assert!(quoted > U256::ZERO, "quoted input amount must be positive");
		// For 100 tokens out from a 10000/10000 pool with 0.3% fee:
		// amount_in = (100 * 1000 * 10000) / ((10000 - 100) * 997) + 1 = 102
		assert_eq!(quoted, U256::from(102), "quoted amount must match expected AMM input");
	});
}

#[test]
fn quote_matches_swap() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;
		let swapper = 2u64;

		setup_pool(provider, 10_000, 10_000);
		assert_ok!(Assets::mint(RuntimeOrigin::signed(provider), 1, swapper, 1_000));

		// Get quote.
		let quote_data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: encode_asset(1).into(),
			asset2: encode_native().into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let quote_result = bare_call(provider, quote_data);
		let quoted = IAssetConversion::quoteExactTokensForTokensCall::abi_decode_returns(
			&quote_result.result.unwrap().data,
		)
		.unwrap();

		// Do the actual swap.
		let swap_data = IAssetConversion::swapExactTokensForTokensCall {
			path: vec![encode_asset(1).into(), encode_native().into()],
			amountIn: U256::from(100),
			amountOutMin: U256::from(1),
			sendTo: account_addr(&swapper),
			keepAlive: false,
		}
		.abi_encode();

		let swap_result = bare_call(swapper, swap_data);
		let actual = IAssetConversion::swapExactTokensForTokensCall::abi_decode_returns(
			&swap_result.result.unwrap().data,
		)
		.unwrap();

		assert_eq!(quoted, actual, "quote and swap must return the same amount");
	});
}

#[test]
fn swap_fails_with_insufficient_output() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;
		let swapper = 2u64;

		setup_pool(provider, 10_000, 10_000);
		assert_ok!(Assets::mint(RuntimeOrigin::signed(provider), 1, swapper, 1_000));

		let data = IAssetConversion::swapExactTokensForTokensCall {
			path: vec![encode_asset(1).into(), encode_native().into()],
			amountIn: U256::from(100),
			amountOutMin: U256::from(999_999),
			sendTo: account_addr(&swapper),
			keepAlive: false,
		}
		.abi_encode();

		let result = bare_call(swapper, data);
		assert!(did_fail(&result), "swap with excessive amountOutMin must fail");
	});
}

#[test]
fn quote_fails_for_nonexistent_pool() {
	new_test_ext().execute_with(|| {
		let caller = 1u64;

		let data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: encode_asset(99).into(),
			asset2: encode_native().into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let result = bare_call(caller, data);
		assert!(did_fail(&result), "quote for nonexistent pool must fail");
	});
}

#[test]
fn quote_fails_with_invalid_encoding() {
	new_test_ext().execute_with(|| {
		let caller = 1u64;

		setup_pool(caller, 10_000, 10_000);

		let data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: alloy::primitives::Bytes::from(vec![0xff, 0xff, 0xff]),
			asset2: encode_native().into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let result = bare_call(caller, data);
		assert!(did_fail(&result), "quote with invalid SCALE encoding must fail");
	});
}

#[test]
fn create_pool_works() {
	new_test_ext().execute_with(|| {
		let creator = 1u64;

		// Create asset 1 first.
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 1u32, creator, true, 1));

		let data = IAssetConversion::createPoolCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
		}
		.abi_encode();

		let result = bare_call(creator, data.clone());
		let return_data = result.result.expect("create_pool must succeed");
		assert!(!return_data.did_revert(), "create_pool must not revert");

		// Creating the same pool again should fail.
		let result2 = bare_call(creator, data);
		assert!(did_fail(&result2), "creating duplicate pool must fail");
	});
}

#[test]
fn add_and_remove_liquidity_works() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;

		// Set up asset and pool (without using setup_pool helper since we want to test
		// add_liquidity via the precompile).
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 1u32, provider, true, 1));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(provider), 1, provider, 100_000));

		// Create pool via precompile.
		let create_data = IAssetConversion::createPoolCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
		}
		.abi_encode();
		let create_result = bare_call(provider, create_data);
		assert!(!create_result.result.unwrap().did_revert());

		// Record balances before adding liquidity.
		let native_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &provider);
		let asset1_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &provider);

		// Add liquidity via precompile.
		let add_data = IAssetConversion::addLiquidityCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
			amount1Desired: U256::from(10_000),
			amount2Desired: U256::from(10_000),
			amount1Min: U256::from(0),
			amount2Min: U256::from(0),
			mintTo: account_addr(&provider),
		}
		.abi_encode();

		let add_result = bare_call(provider, add_data);
		let add_return = add_result.result.expect("add_liquidity must succeed");
		assert!(!add_return.did_revert(), "add_liquidity must not revert");

		let lp_tokens = IAssetConversion::addLiquidityCall::abi_decode_returns(&add_return.data)
			.expect("return data must decode");
		// sqrt(10_000 * 10_000) - MintMinLiquidity(100) = 9_900
		assert_eq!(lp_tokens, U256::from(9_900), "LP tokens must match expected amount");

		// Verify provider was debited.
		let native_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &provider);
		let asset1_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &provider);
		// First liquidity provision to an empty pool debits exactly the desired amounts.
		assert_eq!(native_before - native_after, 10_000, "native must be debited exactly");
		assert_eq!(asset1_before - asset1_after, 10_000, "asset1 must be debited exactly");

		// Verify pool has reserves by quoting.
		let quote_data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: encode_asset(1).into(),
			asset2: encode_native().into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();
		let quote_result = bare_call(provider, quote_data);
		assert!(
			!quote_result.result.unwrap().did_revert(),
			"quote must work after adding liquidity"
		);

		// Record balances before removal.
		let native_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &provider);
		let asset1_before =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &provider);

		// Remove liquidity via precompile.
		let remove_data = IAssetConversion::removeLiquidityCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
			lpTokenBurn: lp_tokens,
			amount1MinReceive: U256::from(1),
			amount2MinReceive: U256::from(1),
			withdrawTo: account_addr(&provider),
		}
		.abi_encode();

		let remove_result = bare_call(provider, remove_data);
		let remove_return = remove_result.result.expect("remove_liquidity must succeed");
		assert!(!remove_return.did_revert(), "remove_liquidity must not revert");

		let ret = IAssetConversion::removeLiquidityCall::abi_decode_returns(&remove_return.data)
			.expect("return data must decode");
		assert!(ret.amount1 > U256::ZERO, "must receive asset1 back");
		assert!(ret.amount2 > U256::ZERO, "must receive asset2 back");

		// Verify actual balance changes match return values.
		let native_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::Native, &provider);
		let asset1_after =
			<NativeAndAssets as Inspect<u64>>::balance(NativeOrWithId::WithId(1), &provider);
		assert_eq!(
			U256::from(native_after - native_before),
			ret.amount1,
			"native balance delta must match return value"
		);
		assert_eq!(
			U256::from(asset1_after - asset1_before),
			ret.amount2,
			"asset1 balance delta must match return value"
		);
	});
}

#[test]
fn add_liquidity_fails_for_nonexistent_pool() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;

		let data = IAssetConversion::addLiquidityCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
			amount1Desired: U256::from(1_000),
			amount2Desired: U256::from(1_000),
			amount1Min: U256::from(0),
			amount2Min: U256::from(0),
			mintTo: account_addr(&provider),
		}
		.abi_encode();

		let result = bare_call(provider, data);
		assert!(did_fail(&result), "add_liquidity to nonexistent pool must fail");
	});
}

#[test]
fn remove_liquidity_fails_with_excessive_lp_burn() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;

		setup_pool(provider, 10_000, 10_000);

		// Try to burn far more LP tokens than exist.
		let data = IAssetConversion::removeLiquidityCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
			lpTokenBurn: U256::from(999_999),
			amount1MinReceive: U256::from(1),
			amount2MinReceive: U256::from(1),
			withdrawTo: account_addr(&provider),
		}
		.abi_encode();

		let result = bare_call(provider, data);
		assert!(did_fail(&result), "remove_liquidity with excessive LP burn must fail");
	});
}

#[test]
fn get_reserves_works() {
	new_test_ext().execute_with(|| {
		let provider = 1u64;

		setup_pool(provider, 10_000, 20_000);

		let data = IAssetConversion::getReservesCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
		}
		.abi_encode();

		let result = bare_call(provider, data);
		let return_data = result.result.expect("get_reserves must succeed");
		assert!(!return_data.did_revert(), "get_reserves must not revert");

		let ret = IAssetConversion::getReservesCall::abi_decode_returns(&return_data.data)
			.expect("return data must decode");
		assert_eq!(ret.reserve1, U256::from(10_000), "native reserve must match");
		assert_eq!(ret.reserve2, U256::from(20_000), "asset1 reserve must match");
	});
}

#[test]
fn get_reserves_fails_for_nonexistent_pool() {
	new_test_ext().execute_with(|| {
		let caller = 1u64;

		let data = IAssetConversion::getReservesCall {
			asset1: encode_native().into(),
			asset2: encode_asset(99).into(),
		}
		.abi_encode();

		let result = bare_call(caller, data);
		assert!(did_fail(&result), "get_reserves for nonexistent pool must fail");
	});
}

// --- Read-only guard tests via STATICCALL ---

alloy::sol! {
	interface ICaller {
		function staticCall(address callee, bytes data, uint64 gas) external view returns (bool success, bytes output);
		function delegate(address callee, bytes data, uint64 gas) external returns (bool success, bytes output);
	}
}

/// Dedicated account for deploying test fixture contracts (avoids clobbering test account
/// balances).
const FIXTURE_DEPLOYER: u64 = 555;

/// Deploy the Caller fixture contract and return its address.
/// The FIXTURE_DEPLOYER account is funded in genesis (see mock.rs).
fn deploy_caller() -> sp_core::H160 {
	let (init_code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Caller",
		pallet_revive_fixtures::FixtureType::Solc,
	)
	.expect("Caller fixture must be compiled");

	pallet_revive::Pallet::<Test>::bare_instantiate(
		RuntimeOrigin::signed(FIXTURE_DEPLOYER),
		0u64.into(),
		TransactionLimits::WeightAndDeposit { weight_limit: Weight::MAX, deposit_limit: u64::MAX },
		Code::Upload(init_code),
		vec![],
		None,
		&ExecConfig::new_substrate_tx(),
	)
	.result
	.expect("Caller deployment must succeed")
	.addr
}

/// Encode a forwarding call through the Caller fixture via STATICCALL or DELEGATECALL.
fn encode_static_call(precompile_calldata: Vec<u8>) -> Vec<u8> {
	ICaller::staticCallCall {
		callee: alloy::primitives::Address::from(precompile_address().0),
		data: precompile_calldata.into(),
		gas: u64::MAX,
	}
	.abi_encode()
}

fn encode_delegate_call(precompile_calldata: Vec<u8>) -> Vec<u8> {
	ICaller::delegateCall {
		callee: alloy::primitives::Address::from(precompile_address().0),
		data: precompile_calldata.into(),
		gas: u64::MAX,
	}
	.abi_encode()
}

/// Helper: call the Caller fixture and decode (success, output).
fn call_fixture(caller_contract: sp_core::H160, calldata: Vec<u8>) -> (bool, Vec<u8>) {
	let result = pallet_revive::Pallet::<Test>::bare_call(
		RuntimeOrigin::signed(FIXTURE_DEPLOYER),
		caller_contract,
		0u64.into(),
		TransactionLimits::WeightAndDeposit { weight_limit: Weight::MAX, deposit_limit: u64::MAX },
		calldata,
		&ExecConfig::new_substrate_tx(),
	)
	.result
	.expect("call to Caller must succeed")
	.data;

	// Both staticCall and delegate return (bool, bytes).
	use alloy::sol_types::SolValue;
	let (success, output): (bool, alloy::primitives::Bytes) =
		SolValue::abi_decode_params(&result).expect("return must decode");
	(success, output.into())
}

use test_case::test_case;

#[test_case(encode_static_call ; "staticcall")]
#[test_case(encode_delegate_call ; "delegatecall")]
fn swap_rejected_via(encode: fn(Vec<u8>) -> Vec<u8>) {
	new_test_ext().execute_with(|| {
		let caller_contract = deploy_caller();

		let swap_data = IAssetConversion::swapExactTokensForTokensCall {
			path: vec![encode_asset(1).into(), encode_native().into()],
			amountIn: U256::from(100),
			amountOutMin: U256::from(1),
			sendTo: account_addr(&1u64),
			keepAlive: false,
		}
		.abi_encode();

		let (success, _) = call_fixture(caller_contract, encode(swap_data));
		assert!(!success, "swap must fail in indirect call context");
	});
}

#[test_case(encode_static_call, true ; "staticcall_allowed")]
#[test_case(encode_delegate_call, false ; "delegatecall_rejected")]
fn quote_via(encode: fn(Vec<u8>) -> Vec<u8>, expect_success: bool) {
	new_test_ext().execute_with(|| {
		let provider = 1u64;
		setup_pool(provider, 10_000, 10_000);

		let caller_contract = deploy_caller();

		let quote_data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: encode_asset(1).into(),
			asset2: encode_native().into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let (success, _) = call_fixture(caller_contract, encode(quote_data));
		assert_eq!(success, expect_success);
	});
}

#[test_case(encode_static_call, true ; "staticcall_allowed")]
#[test_case(encode_delegate_call, false ; "delegatecall_rejected")]
fn get_reserves_via(encode: fn(Vec<u8>) -> Vec<u8>, expect_success: bool) {
	new_test_ext().execute_with(|| {
		let provider = 1u64;
		setup_pool(provider, 10_000, 20_000);

		let caller_contract = deploy_caller();

		let data = IAssetConversion::getReservesCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
		}
		.abi_encode();

		let (success, _) = call_fixture(caller_contract, encode(data));
		assert_eq!(success, expect_success);
	});
}

#[test_case(encode_static_call ; "staticcall")]
#[test_case(encode_delegate_call ; "delegatecall")]
fn create_pool_rejected_via(encode: fn(Vec<u8>) -> Vec<u8>) {
	new_test_ext().execute_with(|| {
		let caller_contract = deploy_caller();

		let data = IAssetConversion::createPoolCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
		}
		.abi_encode();

		let (success, _) = call_fixture(caller_contract, encode(data));
		assert!(!success, "create_pool must fail in indirect call context");
	});
}

/// The delegatecall guard rejects all calls via delegatecall and returns empty output.
#[test]
fn delegatecall_is_rejected() {
	new_test_ext().execute_with(|| {
		let caller_contract = deploy_caller();

		let quote_data = IAssetConversion::quoteExactTokensForTokensCall {
			asset1: encode_native().into(),
			asset2: encode_asset(1).into(),
			amount: U256::from(100),
			includeFee: true,
		}
		.abi_encode();

		let (success, output) = call_fixture(caller_contract, encode_delegate_call(quote_data));
		assert!(!success, "DELEGATECALL to asset-conversion precompile must be rejected");
		assert!(
			output.is_empty(),
			"expected empty output from PrecompileDelegateDenied trap, got {} bytes",
			output.len(),
		);
	});
}
