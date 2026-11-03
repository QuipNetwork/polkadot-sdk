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

//! Benchmarks for the revive pallet.

#![cfg(feature = "runtime-benchmarks")]
use crate::{
	Pallet as Contracts,
	access_list::{AccessEntry, AccessList, MAX_ACCESS_LIST_ENTRIES, StorageOp, Warmth},
	call_builder::{CallSetup, Contract, VmBinaryModule, caller_funding, default_deposit_limit},
	evm::{
		TransactionLegacyUnsigned, TransactionSigned, TransactionUnsigned,
		block_hash::EthereumBlockBuilder, block_storage,
	},
	exec::{Ext, Key, Origin as ExecOrigin, PrecompileExt},
	limits::{self, EVM_STACK_LIMIT},
	precompiles::{
		self, BenchmarkStorage, BenchmarkSystem, BuiltinPrecompile,
		alloy::sol_types::{
			SolType,
			sol_data::{Bool, Bytes, FixedBytes, Uint},
		},
		run::builtin as run_builtin_precompile,
	},
	storage::WriteOutcome,
	vm::{
		evm,
		evm::{ExtBytecode, Halt, Interpreter, instructions, instructions::utility::IntoAddress},
		pvm,
	},
	*,
};
use alloc::{vec, vec::Vec};
use alloy_core::sol_types::{SolInterface, SolValue};
use codec::{Encode, MaxEncodedLen};
use core::ops::ControlFlow;
use frame_benchmarking::v2::*;
use frame_support::{
	self, assert_ok,
	migrations::SteppedMigration,
	storage::child,
	traits::{Hooks, fungible::InspectHold},
	weights::{Weight, WeightMeter},
};
use frame_system::RawOrigin;
use k256::ecdsa::SigningKey;
use pallet_revive_uapi::{
	CallFlags, ReturnErrorCode, StorageFlags, pack_hi_lo,
	precompiles::{storage::IStorage, system::ISystem},
};
use rand::{Rng, SeedableRng, seq::SliceRandom};
use rand_pcg::Pcg64;
use revm::bytecode::{
	Bytecode,
	opcode::{ADDMOD, DIV, MOD, MULMOD, POP, SDIV, SMOD},
};
use sp_consensus_aura::AURA_ENGINE_ID;
use sp_consensus_babe::{
	BABE_ENGINE_ID,
	digests::{PreDigest, PrimaryPreDigest},
};
use sp_consensus_slots::Slot;
use sp_runtime::{generic::DigestItem, traits::Zero};

/// How many runs we do per API benchmark.
///
/// This is picked more or less arbitrary. We experimented with different numbers until
/// the results appeared to be stable. Reducing the number would speed up the benchmarks
/// but might make the results less precise.
const API_BENCHMARK_RUNS: u32 = 1600;

macro_rules! memory(
	($($bytes:expr,)*) => {{
		vec![].iter()$(.chain($bytes.iter()))*.cloned().collect::<Vec<_>>()
	}};
);

macro_rules! build_runtime(
	($runtime:ident, $memory:ident: [$($segment:expr,)*]) => {
		build_runtime!($runtime, _contract, $memory: [$($segment,)*]);
	};
	($runtime:ident, $contract:ident, $memory:ident: [$($bytes:expr,)*]) => {
		build_runtime!($runtime, $contract);
		let mut $memory = memory!($($bytes,)*);
	};
	($runtime:ident, $contract:ident) => {
		let mut setup = CallSetup::<T>::default();
		let $contract = setup.contract();
		let input = setup.data();
		let (mut ext, _) = setup.ext();
		let mut $runtime = $crate::vm::pvm::Runtime::<_, [u8]>::new(&mut ext, input);
	};
);

/// Get the pallet account and whitelist it for benchmarking.
/// The account is warmed up `on_initialize` so read should not impact the PoV.
fn whitelisted_pallet_account<T: Config>() -> T::AccountId {
	let pallet_account = Pallet::<T>::account_id();
	whitelist_account!(pallet_account);
	pallet_account
}

/// Pushes the data a benchmark set up out of the L1 and L2 caches by writing unrelated memory.
///
/// Writes 8 MiB, which is larger than the L2 cache of the reference hardware.
fn evict_caches() {
	const EVICTION_SIZE: usize = 8 * 1024 * 1024;
	core::hint::black_box(vec![1u8; EVICTION_SIZE]);
}

/// Pushes `values` onto the interpreter's stack in order, so the last value ends up on top.
fn setup_stack<E: Ext>(interpreter: &mut Interpreter<E>, values: impl IntoIterator<Item = U256>) {
	values
		.into_iter()
		.for_each(|value| interpreter.stack.push(value).continue_value().unwrap());
}

/// Pairs a pseudo-randomly chosen denominator with the shared `2^255` numerator.
fn unsigned_division_operands(rng: &mut impl Rng, denominators: &[U256]) -> [U256; 2] {
	let denominator = denominators.choose(rng).copied().unwrap();
	[denominator, knuth_division_operands::NUMERATOR]
}

/// Like [`unsigned_division_operands`], but negates the divisor pseudo-randomly.
fn signed_division_operands(rng: &mut impl Rng, denominators: &[U256]) -> [U256; 2] {
	let [magnitude, numerator] = unsigned_division_operands(rng, denominators);
	let divisor =
		if rng.gen_bool(0.5) { magnitude } else { U256::zero().overflowing_sub(magnitude).0 };
	[divisor, numerator]
}

/// Operands covering all 17 correction patterns possible when dividing `2^255` by two-limb
/// divisors.
///
/// Knuth's Algorithm D can correct the highest quotient limb zero or one time and each lower limb
/// up to twice. The pattern `1, 0, 2` is impossible for this numerator. The counts were checked
/// against `uint` 0.10.0's division. None of these operands needs the rarer correction that adds
/// the divisor back after subtracting.
///
/// Picking a denominator pseudo-randomly varies the correction branches across divisions. Each
/// denominator also makes the `u128` divisions behind the estimates take as many steps as any we
/// found with its pattern. Correcting a limb twice needs a second denominator limb above one, which
/// shortens the division behind the first estimate.
///
/// The fixed divisors are the denominators that measured slowest for each op-code. The slowest one
/// depends on the op-code: `SDIV` and `SMOD` measured slowest with different patterns than `DIV`
/// and `MOD`. After updating `uint` or the Rust toolchain, search for the denominators again and
/// measure which one is slowest for each op-code.
mod knuth_division_operands {
	use super::{Rng, U256};

	pub fn fixed_signed_operands(rng: &mut impl Rng, magnitude: U256) -> [U256; 2] {
		// Consume the divisor draw to match the baseline's pseudo-random sign sequence.
		let _ = rng.gen_range(0..DENOMINATORS.len() as u32);
		let divisor =
			if rng.gen_bool(0.5) { magnitude } else { U256::zero().overflowing_sub(magnitude).0 };
		[divisor, NUMERATOR]
	}

	/// 2^255 as an unsigned value, or -2^255 as a signed one, so signed and unsigned divisions
	/// divide the same magnitudes.
	pub const NUMERATOR: U256 = U256([0, 0, 0, 1 << 63]);
	/// The denominator that measured slowest for `DIV`.
	pub const DIV_FIXED_DIVISOR: U256 = U256([0x05f0_ac84_84d2_a8f7, 0x1, 0, 0]);
	/// The denominator that measured slowest for `SDIV`.
	pub const SDIV_FIXED_DIVISOR: U256 = U256([0x16d5_ec4a_0e7b_8cdb, 0x1, 0, 0]);
	/// The denominator that measured slowest for `MOD`.
	pub const MOD_FIXED_DIVISOR: U256 = U256([0x0007_f84c_6624_4131, 0x1, 0, 0]);
	/// The denominator that measured slowest for `SMOD`.
	pub const SMOD_FIXED_DIVISOR: U256 = U256([0x5141_1d54_0d50_9ec1, 0x1, 0, 0]);
	/// `ADDMOD` reuses the divisor of `MOD`.
	pub const ADDMOD_FIXED_DIVISOR: U256 = MOD_FIXED_DIVISOR;

	/// Two distinct divisors per pattern, with correction counts listed in limb order 2, 1, 0.
	pub const DENOMINATORS: [U256; 34] = [
		// Corrections: 0, 0, 0.
		U256([0x1ac6_6dea_3270_0980, 0x1, 0, 0]),
		U256([0x1150_77a4_12e2_ccc0, 0x1, 0, 0]),
		// Corrections: 0, 0, 1.
		U256([0x3c9b_ee44_0b3f_dba7, 0x1, 0, 0]),
		U256([0x62aa_fc46_0845_aad9, 0x1, 0, 0]),
		// Corrections: 0, 1, 0.
		U256([0x2806_4b94_70ef_f3d9, 0x1, 0, 0]),
		U256([0x61d1_c4c6_195a_2d43, 0x1, 0, 0]),
		// Corrections: 0, 1, 1.
		SDIV_FIXED_DIVISOR,
		U256([0x3ef8_7550_1dce_eb99, 0x1, 0, 0]),
		// Corrections: 1, 0, 0.
		U256([0xce1e_8ac2_290c_265f, 0x1, 0, 0]),
		U256([0xd736_05ee_66e1_a76b, 0x1, 0, 0]),
		// Corrections: 1, 0, 1.
		U256([0xeb2b_fb00_8fa9_e0f7, 0x1, 0, 0]),
		SMOD_FIXED_DIVISOR,
		// Corrections: 1, 1, 0.
		U256([0x500c_237e_4578_d88b, 0x1, 0, 0]),
		U256([0x0164_a34e_0b1c_a4fd, 0x1, 0, 0]),
		// Corrections: 1, 1, 1.
		MOD_FIXED_DIVISOR,
		DIV_FIXED_DIVISOR,
		// Corrections: 0, 0, 2.
		U256([0x3db2_354e_f8e8_6ca5, 0x96aa_3b25, 0, 0]),
		U256([0x2848_08be_ea22_56a3, 0x9339_ddd9, 0, 0]),
		// Corrections: 0, 1, 2.
		U256([0x2534_a796_ffdf_b85e, 0xc250_8262, 0, 0]),
		U256([0x4726_e939_da3c_08fe, 0xc198_a4ce, 0, 0]),
		// Corrections: 0, 2, 0.
		U256([0x5d5b_2f53_f03d_bec8, 0x909d_ad3b, 0, 0]),
		U256([0x8e31_4add_736d_ee08, 0x40ce_7f0c, 0, 0]),
		// Corrections: 0, 2, 1.
		U256([0x0c62_030f_fabf_0fc7, 0xb51d_e6cb, 0, 0]),
		U256([0x0332_58df_e522_494c, 0xb4db_915d, 0, 0]),
		// Corrections: 0, 2, 2.
		U256([0x0b07_4c7e_fb00_0c1f, 0xa5b9_8f9a, 0, 0]),
		U256([0x26a5_a3c7_fce7_abd6, 0xa4a5_d36a, 0, 0]),
		// Corrections: 1, 1, 2.
		U256([0x6654_d104_1c8e_6943, 0x2, 0, 0]),
		U256([0x64b6_7f9c_0cf1_dbff, 0x2, 0, 0]),
		// Corrections: 1, 2, 0.
		U256([0xeff9_3d22_eeac_89cf, 0x87, 0, 0]),
		U256([0xadb2_0f65_e0ba_c8df, 0x12, 0, 0]),
		// Corrections: 1, 2, 1.
		U256([0xf486_a407_463b_dfde, 0x253, 0, 0]),
		U256([0xaea8_360c_3043_f5f9, 0x1b4, 0, 0]),
		// Corrections: 1, 2, 2.
		U256([0xecbe_4a28_bb14_dbf0, 0x43, 0, 0]),
		U256([0x64e2_acbc_9934_b1f2, 0x43, 0, 0]),
	];
}

/// Operands covering all eight correction patterns in the one-limb division path.
///
/// `div_mod_small` divides one numerator limb at a time. The highest limb cannot require a
/// correction. Each remaining limb can cause one correction in the 128-bit division helper. The
/// patterns below are ordered by numerator limbs 2, 1, 0. The counts were checked with a
/// source-level trace of Rust 1.93 `compiler_builtins` using the shared `2^255` numerator.
///
/// Each pattern has two distinct full-width divisors. Selecting a divisor pseudo-randomly varies
/// the correction branches while preserving the signed minimum-numerator case.
mod one_limb_division_operands {
	use super::U256;

	/// Two distinct divisors per pattern, with correction counts listed in limb order 2, 1, 0.
	pub const DENOMINATORS: [U256; 16] = [
		// Corrections: 0, 0, 0.
		U256([0xfff9_9928_4b39_14e9, 0, 0, 0]),
		U256([0xfff7_2ef6_07d7_0402, 0, 0, 0]),
		// Corrections: 0, 0, 1.
		U256([0xfffc_adff_2ef2_7bf4, 0, 0, 0]),
		U256([0xffc7_1287_6d60_c1f0, 0, 0, 0]),
		// Corrections: 0, 1, 0.
		U256([0xfff8_74b0_648f_da8a, 0, 0, 0]),
		U256([0xfff7_1d06_39ab_3877, 0, 0, 0]),
		// Corrections: 0, 1, 1.
		U256([0xf974_5f46_74c9_5adf, 0, 0, 0]),
		U256([0xf7e1_3204_7bd0_15af, 0, 0, 0]),
		// Corrections: 1, 0, 0.
		U256([0xfff7_ac60_75f1_501f, 0, 0, 0]),
		U256([0xffe8_555d_5f54_a346, 0, 0, 0]),
		// Corrections: 1, 0, 1.
		U256([0xfe7a_ca58_4afc_ed70, 0, 0, 0]),
		U256([0xfd2b_4905_64bd_9797, 0, 0, 0]),
		// Corrections: 1, 1, 0.
		U256([0xfcd0_0e3d_ccbd_7da4, 0, 0, 0]),
		U256([0xee06_4d5d_bfab_b524, 0, 0, 0]),
		// Corrections: 1, 1, 1.
		U256([0xf42a_21ef_e080_fc89, 0, 0, 0]),
		U256([0xf095_aea7_fc96_fbde, 0, 0, 0]),
	];
}

/// Randomly chooses between the Knuth and one-limb correction-pattern operand sets.
mod mixed_division_operands {
	use super::{
		Rng, U256, knuth_division_operands, one_limb_division_operands, signed_division_operands,
		unsigned_division_operands,
	};

	pub fn unsigned_operands(rng: &mut impl Rng) -> [U256; 2] {
		let denominators = denominators(rng);
		unsigned_division_operands(rng, denominators)
	}

	pub fn signed_operands(rng: &mut impl Rng) -> [U256; 2] {
		let denominators = denominators(rng);
		signed_division_operands(rng, denominators)
	}

	fn denominators(rng: &mut impl Rng) -> &'static [U256] {
		if rng.gen_bool(0.5) {
			&knuth_division_operands::DENOMINATORS
		} else {
			&one_limb_division_operands::DENOMINATORS
		}
	}
}

#[benchmarks(
	where
		T: Config,
		<T as Config>::RuntimeCall: From<frame_system::Call<T>>,
		<T as frame_system::Config>::Hash: frame_support::traits::IsType<H256>,
		OriginFor<T>: From<Origin<T>>,
)]
mod benchmarks {
	use super::*;

	/// The base weight consumed on processing contracts deletion queue.
	#[benchmark(pov_mode = Measured)]
	fn deletion_queue_batch() {
		#[block]
		{
			ContractInfo::<T>::process_deletion_queue_batch(&mut WeightMeter::new())
		}
	}

	/// Measures the per-entry cost of `process_deletion_queue_batch`: one `DeletionQueue` read
	/// plus the `DeletionQueue` + `DeletionQueueCounter` writes done by `entry.remove()`.
	#[benchmark(pov_mode = Measured)]
	fn deletion_queue_per_entry() -> Result<(), BenchmarkError> {
		let instance = Contract::<T>::with_storage(VmBinaryModule::dummy(), 0, 0)?;
		ContractInfo::<T>::queue_for_deletion(
			instance.info()?.trie_id,
			instance.account_id.clone(),
		);

		#[block]
		{
			ContractInfo::<T>::process_deletion_queue_batch(&mut WeightMeter::new())
		}

		assert!(<DeletionQueue<T>>::iter().next().is_none(), "deletion queue should be drained",);
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn deletion_queue_per_trie_key(k: Linear<0, 1024>) -> Result<(), BenchmarkError> {
		let instance =
			Contract::<T>::with_storage(VmBinaryModule::dummy(), k, limits::STORAGE_BYTES)?;
		ContractInfo::<T>::queue_for_deletion(
			instance.info()?.trie_id,
			instance.account_id.clone(),
		);

		#[block]
		{
			ContractInfo::<T>::process_deletion_queue_batch(&mut WeightMeter::new())
		}

		assert!(<DeletionQueue<T>>::iter().next().is_none(), "deletion queue should be drained",);
		Ok(())
	}

	/// Measures the cost of clearing one [`NativeDepositOf`] row during
	/// [`ContractInfo::process_deletion_queue_batch`]. Pre-populates the contract with `k`
	/// per-payer rows and queues the contract for deletion with `native_cleared = false` and
	/// an empty trie. The deletion queue then drains all rows in one go.
	#[benchmark(skip_meta, pov_mode = Measured)]
	fn deletion_queue_per_native_deposit_key(k: Linear<0, 1024>) -> Result<(), BenchmarkError> {
		use frame_benchmarking::v2::account;

		// Empty trie: zero items, zero bytes; we only want to measure native-deposit cleanup.
		let instance = Contract::<T>::with_storage(VmBinaryModule::dummy(), 0, 0)?;
		for i in 0..k {
			let payer: T::AccountId = account("payer", i, 0);
			NativeDepositOf::<T>::insert(&instance.account_id, &payer, BalanceOf::<T>::default());
		}
		ContractInfo::<T>::queue_for_deletion(
			instance.info()?.trie_id,
			instance.account_id.clone(),
		);

		#[block]
		{
			ContractInfo::<T>::process_deletion_queue_batch(&mut WeightMeter::new())
		}

		assert!(<DeletionQueue<T>>::iter().next().is_none(), "deletion queue should be drained",);
		Ok(())
	}

	// This benchmarks the overhead of loading a code of size `c` byte from storage and into
	// the execution engine.
	//
	// `call_with_pvm_code_per_byte(c) - call_with_pvm_code_per_byte(0)`
	//
	// This does **not** include the actual execution for which the gas meter
	// is responsible. The code used here will just return on call.
	//
	// We expect the influence of `c` to be none in this benchmark because every instruction that
	// is not in the first basic block is never read. We are primarily interested in the
	// `proof_size` result of this benchmark.
	#[benchmark(pov_mode = Measured)]
	fn call_with_pvm_code_per_byte(c: Linear<0, { 100 * 1024 }>) -> Result<(), BenchmarkError> {
		let instance =
			Contract::<T>::with_caller(whitelisted_caller(), VmBinaryModule::sized(c), vec![])?;
		let value = Pallet::<T>::min_balance();
		let storage_deposit = default_deposit_limit::<T>();

		#[extrinsic_call]
		call(
			RawOrigin::Signed(instance.caller.clone()),
			instance.address,
			value,
			Weight::MAX,
			storage_deposit,
			vec![],
		);

		Ok(())
	}

	// This benchmarks the overhead of loading a code of size `c` byte from storage and into
	// the execution engine.
	/// This is similar to `call_with_pvm_code_per_byte` but for EVM bytecode.
	#[benchmark(pov_mode = Measured)]
	fn call_with_evm_code_per_byte(c: Linear<1, { 10 * 1024 }>) -> Result<(), BenchmarkError> {
		let instance = Contract::<T>::with_caller(
			whitelisted_caller(),
			VmBinaryModule::evm_init_code_for_runtime_size(c),
			vec![],
		)?;
		let value = Pallet::<T>::min_balance();
		let storage_deposit = default_deposit_limit::<T>();

		let code_len = PristineCode::<T>::get(instance.info()?.code_hash)
			.expect("code should be stored")
			.len();
		assert_eq!(
			code_len, c as usize,
			"runtime bytecode should be exactly {c} bytes, got {code_len}"
		);

		#[extrinsic_call]
		call(
			RawOrigin::Signed(instance.caller.clone()),
			instance.address,
			value,
			Weight::MAX,
			storage_deposit,
			vec![],
		);

		Ok(())
	}

	// Measure the amount of time it takes to compile a single basic block.
	//
	// (basic_block_compilation(1) - basic_block_compilation(0)).ref_time()
	//
	// This is needed because the interpreter will always compile a whole basic block at
	// a time. To prevent a contract from triggering compilation without doing any execution
	// we will always charge one max sized block per contract call.
	//
	// We ignore the proof size component when using this benchmark as this is already accounted
	// for in `call_with_pvm_code_per_byte`.
	#[benchmark(pov_mode = Measured)]
	fn basic_block_compilation(b: Linear<0, 1>) -> Result<(), BenchmarkError> {
		let instance = Contract::<T>::with_caller(
			whitelisted_caller(),
			VmBinaryModule::with_num_instructions(limits::code::BASIC_BLOCK_SIZE),
			vec![],
		)?;
		let value = Pallet::<T>::min_balance();
		let storage_deposit = default_deposit_limit::<T>();

		#[block]
		{
			Pallet::<T>::call(
				RawOrigin::Signed(instance.caller.clone()).into(),
				instance.address,
				value,
				Weight::MAX,
				storage_deposit,
				vec![],
			)?;
		}

		Ok(())
	}

	// `c`: Size of the code in bytes.
	// `i`: Size of the input in bytes.
	#[benchmark(pov_mode = Measured)]
	fn instantiate_with_code(
		c: Linear<0, { 100 * 1024 }>,
		i: Linear<0, { limits::CALLDATA_BYTES }>,
	) {
		let pallet_account = whitelisted_pallet_account::<T>();
		let input = vec![42u8; i as usize];
		let salt = [42u8; 32];
		let value = Pallet::<T>::min_balance();
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let VmBinaryModule { code, .. } = VmBinaryModule::sized(c);
		let origin = RawOrigin::Signed(caller.clone());
		if !T::AddressMapper::is_mapped(&caller) {
			T::AddressMapper::map(&caller).unwrap();
		}
		let deployer = T::AddressMapper::to_address(&caller);
		let addr = crate::address::create2(&deployer, &code, &input, &salt);
		let account_id = T::AddressMapper::to_fallback_account_id(&addr);
		let storage_deposit = default_deposit_limit::<T>();
		#[extrinsic_call]
		_(origin, value, Weight::MAX, storage_deposit, code, input, Some(salt));

		let deposit =
			T::Currency::balance_on_hold(&HoldReason::StorageDepositReserve.into(), &account_id);
		// uploading the code reserves some balance in the pallet's account
		let code_deposit = T::Currency::balance_on_hold(
			&HoldReason::CodeUploadDepositReserve.into(),
			&pallet_account,
		);
		let mapping_deposit =
			T::Currency::balance_on_hold(&HoldReason::AddressMapping.into(), &caller);
		assert_eq!(
			T::Currency::balance(&caller),
			caller_funding::<T>() - value - deposit - code_deposit - mapping_deposit,
		);
		// contract has the full value
		assert_eq!(T::Currency::balance(&account_id), value + Pallet::<T>::min_balance());
	}

	// `c`: Size of the code in bytes.
	// `i`: Size of the input in bytes.
	// `d`: with or without dust value to transfer
	#[benchmark(pov_mode = Measured)]
	fn eth_instantiate_with_code(
		c: Linear<0, { 100 * 1024 }>,
		i: Linear<0, { limits::CALLDATA_BYTES }>,
		d: Linear<0, 1>,
	) -> Result<(), BenchmarkError> {
		let input = vec![42u8; i as usize];

		// Use an `effective_gas_price` that is not a multiple of `T::NativeToEthRatio`
		// to hit the code that charge the rounding error so that tx_cost == effective_gas_price *
		// gas_used
		let effective_gas_price = Pallet::<T>::evm_base_fee() + 1;
		let value = Pallet::<T>::min_balance();
		let dust = 42u32 * d;
		let evm_value =
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(value, dust));
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let VmBinaryModule { code, .. } = VmBinaryModule::sized(c);
		let origin = Origin::EthTransaction(caller.clone());
		if !T::AddressMapper::is_mapped(&caller) {
			T::AddressMapper::map(&caller).unwrap();
		}
		let deployer = T::AddressMapper::to_address(&caller);
		let nonce = System::<T>::account_nonce(&caller).try_into().unwrap_or_default();
		let addr = crate::address::create1(&deployer, nonce);

		assert!(AccountInfoOf::<T>::get(&deployer).is_none());

		<T as Config>::FeeInfo::deposit_txfee(
			<T as Config>::Currency::issue(caller_funding::<T>()),
		);

		#[extrinsic_call]
		_(
			origin,
			evm_value,
			Weight::MAX,
			U256::MAX,
			code,
			input,
			TransactionSigned::default().signed_payload(),
			effective_gas_price,
			0,
		);

		// contract has the full value
		assert_eq!(Pallet::<T>::evm_balance(&addr), evm_value);
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn deposit_eth_extrinsic_revert_event() {
		#[block]
		{
			Pallet::<T>::deposit_event(Event::<T>::EthExtrinsicRevert {
				dispatch_error: crate::Error::<T>::BenchmarkingError.into(),
			});
		}
	}

	// `i`: Size of the input in bytes.
	// `s`: Size of e salt in bytes.
	#[benchmark(pov_mode = Measured)]
	fn instantiate(i: Linear<0, { limits::CALLDATA_BYTES }>) -> Result<(), BenchmarkError> {
		let pallet_account = whitelisted_pallet_account::<T>();
		let input = vec![42u8; i as usize];
		let salt = [42u8; 32];
		let value = Pallet::<T>::min_balance();
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let origin = RawOrigin::Signed(caller.clone());
		if !T::AddressMapper::is_mapped(&caller) {
			T::AddressMapper::map(&caller).unwrap();
		}
		let VmBinaryModule { code, .. } = VmBinaryModule::dummy();
		let storage_deposit = default_deposit_limit::<T>();
		let deployer = T::AddressMapper::to_address(&caller);
		let addr = crate::address::create2(&deployer, &code, &input, &salt);
		let hash = Contracts::<T>::bare_upload_code(origin.clone().into(), code, storage_deposit)?
			.code_hash;
		let account_id = T::AddressMapper::to_fallback_account_id(&addr);

		#[extrinsic_call]
		_(origin, value, Weight::MAX, storage_deposit, hash, input, Some(salt));

		let deposit =
			T::Currency::balance_on_hold(&HoldReason::StorageDepositReserve.into(), &account_id);
		let code_deposit = T::Currency::balance_on_hold(
			&HoldReason::CodeUploadDepositReserve.into(),
			&pallet_account,
		);
		let mapping_deposit =
			T::Currency::balance_on_hold(&HoldReason::AddressMapping.into(), &account_id);
		// value was removed from the caller
		assert_eq!(
			T::Currency::total_balance(&caller),
			caller_funding::<T>() - value - deposit - code_deposit - mapping_deposit,
		);
		// contract has the full value
		assert_eq!(T::Currency::balance(&account_id), value + Pallet::<T>::min_balance());

		Ok(())
	}

	// We just call a dummy contract to measure the overhead of the call extrinsic.
	// The size of the data has no influence on the costs of this extrinsic as long as the contract
	// won't call `seal_call_data_copy` in its constructor to copy the data to contract memory.
	// The dummy contract used here does not do this. The costs for the data copy is billed as
	// part of `seal_call_data_copy`. The costs for invoking a contract of a specific size are not
	// part of this benchmark because we cannot know the size of the contract when issuing a call
	// transaction. See `call_with_pvm_code_per_byte` for this.
	#[benchmark(pov_mode = Measured)]
	fn call() -> Result<(), BenchmarkError> {
		let pallet_account = whitelisted_pallet_account::<T>();
		let data = vec![42u8; 1024];
		let instance =
			Contract::<T>::with_caller(whitelisted_caller(), VmBinaryModule::dummy(), vec![])?;
		let value = Pallet::<T>::min_balance();
		let origin = RawOrigin::Signed(instance.caller.clone());
		let before = T::Currency::balance(&instance.account_id);
		let storage_deposit = default_deposit_limit::<T>();
		#[extrinsic_call]
		_(origin, instance.address, value, Weight::MAX, storage_deposit, data);
		let deposit = T::Currency::balance_on_hold(
			&HoldReason::StorageDepositReserve.into(),
			&instance.account_id,
		);
		let code_deposit = T::Currency::balance_on_hold(
			&HoldReason::CodeUploadDepositReserve.into(),
			&pallet_account,
		);
		let mapping_deposit =
			T::Currency::balance_on_hold(&HoldReason::AddressMapping.into(), &instance.caller);
		// value and value transferred via call should be removed from the caller
		assert_eq!(
			T::Currency::balance(&instance.caller),
			caller_funding::<T>() - value - deposit - code_deposit - mapping_deposit,
		);
		// contract should have received the value
		assert_eq!(T::Currency::balance(&instance.account_id), before + value);
		// contract should still exist
		instance.info()?;

		Ok(())
	}

	// `d`: with or without dust value to transfer
	#[benchmark(pov_mode = Measured)]
	fn eth_call(d: Linear<0, 1>) -> Result<(), BenchmarkError> {
		let data = vec![42u8; 1024];
		let instance =
			Contract::<T>::with_caller(whitelisted_caller(), VmBinaryModule::dummy(), vec![])?;

		// Use an `effective_gas_price` that is not a multiple of `T::NativeToEthRatio`
		// to hit the code that charge the rounding error so that tx_cost == effective_gas_price *
		// gas_used
		let effective_gas_price = Pallet::<T>::evm_base_fee() + 1;
		let value = Pallet::<T>::min_balance();
		let dust = 42u32 * d;
		let evm_value =
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(value, dust));

		// need to pass the overdraw check
		<T as Config>::FeeInfo::deposit_txfee(
			<T as Config>::Currency::issue(caller_funding::<T>()),
		);

		let origin = Origin::EthTransaction(instance.caller.clone());
		let before = Pallet::<T>::evm_balance(&instance.address);

		#[extrinsic_call]
		_(
			origin,
			instance.address,
			evm_value,
			Weight::MAX,
			U256::MAX,
			data,
			TransactionSigned::default().signed_payload(),
			effective_gas_price,
			0,
		);

		// contract should have received the value
		assert_eq!(Pallet::<T>::evm_balance(&instance.address), before + evm_value);
		// contract should still exist
		instance.info()?;

		Ok(())
	}

	// `c`: Size of the RLP encoded Ethereum transaction in bytes.
	#[benchmark(pov_mode = Measured)]
	fn eth_substrate_call(c: Linear<0, { 100 * 1024 }>) -> Result<(), BenchmarkError> {
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let origin = Origin::EthTransaction(caller);
		let dispatchable = frame_system::Call::remark { remark: vec![] }.into();
		#[extrinsic_call]
		_(origin, Box::new(dispatchable), vec![42u8; c as usize]);
		Ok(())
	}

	// This constructs a contract that is maximal expensive to instrument.
	// It creates a maximum number of metering blocks per byte.
	// `c`: Size of the code in bytes.
	#[benchmark(pov_mode = Measured)]
	fn upload_code(c: Linear<0, { 100 * 1024 }>) {
		let caller = whitelisted_caller();
		let pallet_account = whitelisted_pallet_account::<T>();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let VmBinaryModule { code, hash, .. } = VmBinaryModule::sized(c);
		let origin = RawOrigin::Signed(caller.clone());
		let storage_deposit = default_deposit_limit::<T>();
		#[extrinsic_call]
		_(origin, code, storage_deposit);
		// uploading the code reserves some balance in the pallet's account
		assert!(T::Currency::total_balance_on_hold(&pallet_account) > 0u32.into());
		assert!(<Contract<T>>::code_exists(&hash));
	}

	// Removing code does not depend on the size of the contract because all the information
	// needed to verify the removal claim (refcount, owner) is stored in a separate storage
	// item (`CodeInfoOf`).
	#[benchmark(pov_mode = Measured)]
	fn remove_code() -> Result<(), BenchmarkError> {
		let caller = whitelisted_caller();
		let pallet_account = whitelisted_pallet_account::<T>();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let VmBinaryModule { code, hash, .. } = VmBinaryModule::dummy();
		let origin = RawOrigin::Signed(caller.clone());
		let storage_deposit = default_deposit_limit::<T>();
		let uploaded =
			<Contracts<T>>::bare_upload_code(origin.clone().into(), code, storage_deposit)?;
		assert_eq!(uploaded.code_hash, hash);
		assert_eq!(uploaded.deposit, T::Currency::total_balance_on_hold(&pallet_account));
		assert!(<Contract<T>>::code_exists(&hash));
		#[extrinsic_call]
		_(origin, hash);
		// removing the code should have unreserved the deposit
		assert_eq!(T::Currency::total_balance_on_hold(&pallet_account), 0u32.into());
		assert!(<Contract<T>>::code_removed(&hash));
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn set_code() -> Result<(), BenchmarkError> {
		let instance =
			<Contract<T>>::with_caller(whitelisted_caller(), VmBinaryModule::dummy(), vec![])?;
		// we just add some bytes so that the code hash is different
		let VmBinaryModule { code, .. } = VmBinaryModule::dummy_unique(128);
		let origin = RawOrigin::Signed(instance.caller.clone());
		let storage_deposit = default_deposit_limit::<T>();
		let hash =
			<Contracts<T>>::bare_upload_code(origin.into(), code, storage_deposit)?.code_hash;
		assert_ne!(instance.info()?.code_hash, hash);
		#[extrinsic_call]
		_(RawOrigin::Root, instance.address, hash);
		assert_eq!(instance.info()?.code_hash, hash);
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn map_account() {
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let origin = RawOrigin::Signed(caller.clone());
		if T::AddressMapper::is_mapped(&caller) {
			T::AddressMapper::unmap(&caller).unwrap();
		}
		assert!(!T::AddressMapper::is_mapped(&caller));
		#[extrinsic_call]
		_(origin);
		assert!(T::AddressMapper::is_mapped(&caller));
	}

	#[benchmark(pov_mode = Measured)]
	fn unmap_account() {
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let origin = RawOrigin::Signed(caller.clone());
		if !T::AddressMapper::is_mapped(&caller) {
			T::AddressMapper::map(&caller).unwrap();
		}
		assert!(T::AddressMapper::is_mapped(&caller));
		#[extrinsic_call]
		_(origin);
		assert!(!T::AddressMapper::is_mapped(&caller));
	}

	/// Worst case: every input account is not eth-derived, not yet mapped, and
	/// already carries an [`HoldReason::AddressMapping`] hold. The per-account
	/// loop body in `batch_map_accounts` then both inserts the [`OriginalAccount`]
	/// entry via `map_no_deposit_unchecked` *and* releases the existing hold.
	#[benchmark(pov_mode = Measured)]
	fn batch_map_accounts(a: Linear<0, 1024>) -> Result<(), BenchmarkError> {
		use frame_benchmarking::v2::account;

		let caller: T::AccountId = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());

		// Matches the deposit that `AccountId32Mapper::map` would normally take.
		let deposit = T::DepositPerByte::get()
			.saturating_mul(52u32.into())
			.saturating_add(T::DepositPerItem::get());

		let mut accounts = Vec::with_capacity(a as usize);
		for i in 0..a {
			let account_id: T::AccountId = account("to_map", i, 0);
			T::Currency::set_balance(&account_id, caller_funding::<T>());
			T::Currency::hold(&HoldReason::AddressMapping.into(), &account_id, deposit)?;
			accounts.push(account_id);
		}

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), accounts.clone());

		for account_id in &accounts {
			assert!(T::AddressMapper::is_mapped(account_id));
			assert_eq!(
				T::Currency::balance_on_hold(&HoldReason::AddressMapping.into(), account_id),
				0u32.into(),
			);
		}

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn dispatch_as_fallback_account() {
		let caller = whitelisted_caller();
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let origin = RawOrigin::Signed(caller.clone());
		let dispatchable = frame_system::Call::remark { remark: vec![] }.into();
		#[extrinsic_call]
		_(origin, Box::new(dispatchable));
	}

	#[benchmark(pov_mode = Measured)]
	fn noop_host_fn(r: Linear<0, API_BENCHMARK_RUNS>) {
		let mut setup = CallSetup::<T>::new(VmBinaryModule::noop());
		let (mut ext, module) = setup.ext();
		let prepared = CallSetup::<T>::prepare_call(&mut ext, module, r.encode(), 0);
		#[block]
		{
			prepared.call().unwrap();
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_caller() {
		let len = H160::len_bytes();
		build_runtime!(runtime, memory: [vec![0u8; len as _], ]);

		let result;
		#[block]
		{
			result = runtime.bench_caller(memory.as_mut_slice(), 0);
		}

		assert_ok!(result);
		assert_eq!(
			<H160 as Decode>::decode(&mut &memory[..]).unwrap(),
			T::AddressMapper::to_address(&runtime.ext().caller().account_id().unwrap())
		);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_origin() {
		let len = H160::len_bytes();
		build_runtime!(runtime, memory: [vec![0u8; len as _], ]);

		let result;
		#[block]
		{
			result = runtime.bench_origin(memory.as_mut_slice(), 0);
		}

		assert_ok!(result);
		assert_eq!(
			<H160 as Decode>::decode(&mut &memory[..]).unwrap(),
			T::AddressMapper::to_address(&runtime.ext().origin().account_id().unwrap())
		);
	}

	#[benchmark(pov_mode = Measured)]
	fn to_account_id() {
		// use a mapped address for the benchmark, to ensure that we bench the worst
		// case (and not the fallback case).
		let account_id = account("precompile_to_account_id", 0, 0);
		let address = {
			T::Currency::set_balance(&account_id, caller_funding::<T>());
			if !T::AddressMapper::is_mapped(&account_id) {
				T::AddressMapper::map(&account_id).unwrap();
			}
			T::AddressMapper::to_address(&account_id)
		};

		let input_bytes = ISystem::ISystemCalls::toAccountId(ISystem::toAccountIdCall {
			input: address.0.into(),
		})
		.abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let raw_data = result.unwrap().data;
		let data = Bytes::abi_decode(&raw_data).expect("decoding failed");
		assert_ne!(
			data.0.as_ref()[20..32],
			[0xEE; 12],
			"fallback suffix found where none should be"
		);
		assert_eq!(T::AccountId::decode(&mut data.as_ref()), Ok(account_id),);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_code_hash() {
		let contract = Contract::<T>::with_index(1, VmBinaryModule::dummy(), vec![]).unwrap();
		let len = <sp_core::H256 as MaxEncodedLen>::max_encoded_len() as u32;
		build_runtime!(runtime, memory: [vec![0u8; len as _], contract.account_id.encode(), ]);

		let result;
		#[block]
		{
			result = runtime.bench_code_hash(memory.as_mut_slice(), len, 0);
		}

		assert_ok!(result);
		assert_eq!(
			<sp_core::H256 as Decode>::decode(&mut &memory[..]).unwrap(),
			contract.info().unwrap().code_hash
		);
	}

	#[benchmark(pov_mode = Measured)]
	fn own_code_hash() {
		let input_bytes =
			ISystem::ISystemCalls::ownCodeHash(ISystem::ownCodeHashCall {}).abi_encode();
		let mut call_setup = CallSetup::<T>::default();
		let contract_acc = call_setup.contract().account_id.clone();
		let caller = call_setup.contract().address;
		call_setup.set_origin(ExecOrigin::from_account_id(contract_acc));
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert!(result.is_ok());
		let caller_code_hash = ext.code_hash(&caller);
		assert_eq!(caller_code_hash.0.to_vec(), result.unwrap().data);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_code_size() {
		let contract = Contract::<T>::with_index(1, VmBinaryModule::dummy(), vec![]).unwrap();
		build_runtime!(runtime, memory: [contract.address.encode(),]);

		let result;
		#[block]
		{
			result = runtime.bench_code_size(memory.as_mut_slice(), 0);
		}

		assert_eq!(result.unwrap(), VmBinaryModule::dummy().code.len() as u64);
	}

	#[benchmark(pov_mode = Measured)]
	fn caller_is_origin() {
		let input_bytes =
			ISystem::ISystemCalls::callerIsOrigin(ISystem::callerIsOriginCall {}).abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let raw_data = result.unwrap().data;
		let is_origin = Bool::abi_decode(&raw_data[..]).expect("decoding failed");
		assert!(is_origin);
	}

	#[benchmark(pov_mode = Measured)]
	fn caller_is_root() {
		let input_bytes =
			ISystem::ISystemCalls::callerIsRoot(ISystem::callerIsRootCall {}).abi_encode();

		let mut setup = CallSetup::<T>::default();
		setup.set_origin(ExecOrigin::Root);
		let (mut ext, _) = setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let raw_data = result.unwrap().data;
		let is_root = Bool::abi_decode(&raw_data).expect("decoding failed");
		assert!(is_root);
	}

	#[benchmark(pov_mode = Measured)]
	fn origin_is_root() {
		let input_bytes =
			ISystem::ISystemCalls::originIsRoot(ISystem::originIsRootCall {}).abi_encode();

		let mut setup = CallSetup::<T>::default();
		setup.set_origin(ExecOrigin::Root);
		let (mut ext, _) = setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let raw_data = result.unwrap().data;
		let is_root = Bool::abi_decode(&raw_data).expect("decoding failed");
		assert!(is_root);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_address() {
		let len = H160::len_bytes();
		build_runtime!(runtime, memory: [vec![0u8; len as _], ]);

		let result;
		#[block]
		{
			result = runtime.bench_address(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);
		assert_eq!(<H160 as Decode>::decode(&mut &memory[..]).unwrap(), runtime.ext().address());
	}

	#[benchmark(pov_mode = Measured)]
	fn weight_left() {
		let input_bytes =
			ISystem::ISystemCalls::weightLeft(ISystem::weightLeftCall {}).abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let weight_left_before = ext.frame_meter().weight_left().unwrap();
		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let weight_left_after = ext.frame_meter().weight_left().unwrap();
		assert_ne!(weight_left_after.ref_time(), 0);
		assert!(weight_left_before.ref_time() > weight_left_after.ref_time());

		let raw_data = result.unwrap().data;
		type MyTy = (Uint<64>, Uint<64>);
		let foo = MyTy::abi_decode(&raw_data[..]).unwrap();
		assert_eq!(weight_left_after.ref_time(), foo.0);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_ref_time_left() {
		build_runtime!(runtime, memory: [vec![], ]);

		let result;
		#[block]
		{
			result = runtime.bench_ref_time_left(memory.as_mut_slice());
		}
		assert_eq!(result.unwrap(), runtime.ext().gas_left());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_balance() {
		build_runtime!(runtime, contract, memory: [[0u8;32], ]);
		contract.set_balance(BalanceWithDust::new_unchecked::<T>(
			Pallet::<T>::min_balance() * 2u32.into(),
			42u32,
		));

		let result;
		#[block]
		{
			result = runtime.bench_balance(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);
		assert_eq!(
			U256::from_little_endian(&memory[..]),
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(
				Pallet::<T>::min_balance(),
				42
			))
		);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_balance_of() {
		let len = <sp_core::U256 as MaxEncodedLen>::max_encoded_len();
		let account = account::<T::AccountId>("target", 0, 0);
		<T as Config>::AddressMapper::map_no_deposit_unchecked(&account).unwrap();

		let address = T::AddressMapper::to_address(&account);
		let balance = Pallet::<T>::min_balance() * 2u32.into();
		T::Currency::set_balance(&account, balance);
		AccountInfoOf::<T>::insert(&address, AccountInfo { dust: 42, ..Default::default() });

		build_runtime!(runtime, memory: [vec![0u8; len], address.0, ]);

		let result;
		#[block]
		{
			result = runtime.bench_balance_of(memory.as_mut_slice(), len as u32, 0);
		}

		assert_ok!(result);
		assert_eq!(
			U256::from_little_endian(&memory[..len]),
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(
				Pallet::<T>::min_balance(),
				42
			))
		);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_get_immutable_data(n: Linear<1, { limits::IMMUTABLE_BYTES }>) {
		let len = n as usize;
		let immutable_data = vec![1u8; len];

		build_runtime!(runtime, contract, memory: [(len as u32).encode(), vec![0u8; len],]);

		<ImmutableDataOf<T>>::insert::<_, BoundedVec<_, _>>(
			contract.address,
			immutable_data.clone().try_into().unwrap(),
		);

		let result;
		#[block]
		{
			result = runtime.bench_get_immutable_data(memory.as_mut_slice(), 4, 0 as u32);
		}

		assert_ok!(result);
		assert_eq!(&memory[0..4], (len as u32).encode());
		assert_eq!(&memory[4..len + 4], &immutable_data);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_set_immutable_data(n: Linear<1, { limits::IMMUTABLE_BYTES }>) {
		let len = n as usize;
		let mut memory = vec![1u8; len];
		let mut setup = CallSetup::<T>::default();
		let input = setup.data();
		let (mut ext, _) = setup.ext();
		ext.override_export(crate::exec::ExportedFunction::Constructor);

		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, input);

		let result;
		#[block]
		{
			result = runtime.bench_set_immutable_data(memory.as_mut_slice(), 0, n);
		}

		assert_ok!(result);
		assert_eq!(&memory[..], &<ImmutableDataOf<T>>::get(setup.contract().address).unwrap()[..]);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_value_transferred() {
		build_runtime!(runtime, memory: [[0u8;32], ]);
		let result;
		#[block]
		{
			result = runtime.bench_value_transferred(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);
		assert_eq!(U256::from_little_endian(&memory[..]), runtime.ext().value_transferred());
	}

	#[benchmark(pov_mode = Measured)]
	fn minimum_balance() {
		let input_bytes =
			ISystem::ISystemCalls::minimumBalance(ISystem::minimumBalanceCall {}).abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let min: U256 = crate::Pallet::<T>::convert_native_to_evm(T::Currency::minimum_balance());
		let min =
			crate::precompiles::alloy::primitives::aliases::U256::abi_decode(&min.to_big_endian())
				.unwrap();

		let raw_data = result.unwrap().data;
		let returned_min =
			crate::precompiles::alloy::primitives::aliases::U256::abi_decode(&raw_data)
				.expect("decoding failed");
		assert_eq!(returned_min, min);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_return_data_size() {
		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::new(&mut ext, vec![]);
		let mut memory = memory!(vec![],);
		*runtime.ext().last_frame_output_mut() =
			ExecReturnValue { data: vec![42; 256], ..Default::default() };
		let result;
		#[block]
		{
			result = runtime.bench_return_data_size(memory.as_mut_slice());
		}
		assert_eq!(result.unwrap(), 256);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_call_data_size() {
		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::new(&mut ext, vec![42u8; 128 as usize]);
		let mut memory = memory!(vec![0u8; 4],);
		let result;
		#[block]
		{
			result = runtime.bench_call_data_size(memory.as_mut_slice());
		}
		assert_eq!(result.unwrap(), 128);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_gas_limit() {
		build_runtime!(runtime, memory: []);
		let result;
		#[block]
		{
			result = runtime.bench_gas_limit(&mut memory);
		}
		assert_eq!(U256::from(result.unwrap()), <Pallet<T>>::evm_block_gas_limit());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_gas_price() {
		build_runtime!(runtime, memory: []);
		let result;
		#[block]
		{
			result = runtime.bench_gas_price(memory.as_mut_slice());
		}
		assert_eq!(U256::from(result.unwrap()), <Pallet<T>>::evm_base_fee());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_base_fee() {
		build_runtime!(runtime, memory: [[1u8;32], ]);
		let result;
		#[block]
		{
			result = runtime.bench_base_fee(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);
		assert_eq!(U256::from_little_endian(&memory[..]), <crate::Pallet<T>>::evm_base_fee());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_block_number() {
		build_runtime!(runtime, memory: [[0u8;32], ]);
		let result;
		#[block]
		{
			result = runtime.bench_block_number(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);
		assert_eq!(U256::from_little_endian(&memory[..]), runtime.ext().block_number());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_block_author() {
		build_runtime!(runtime, memory: [[123u8; 20], ]);

		// The pre-runtime digest log is unbounded; usually around 3 items but it can vary.
		// To get safe benchmark results despite that, populate it with a bunch of random logs to
		// ensure iteration over many items (we just overestimate the cost of the API).
		for i in 0..16 {
			frame_system::Pallet::<T>::deposit_log(DigestItem::PreRuntime(
				[i, i, i, i],
				vec![i; 128],
			));
			frame_system::Pallet::<T>::deposit_log(DigestItem::Consensus(
				[i, i, i, i],
				vec![i; 128],
			));
			frame_system::Pallet::<T>::deposit_log(DigestItem::Seal([i, i, i, i], vec![i; 128]));
			frame_system::Pallet::<T>::deposit_log(DigestItem::Other(vec![i; 128]));
		}

		// The content of the pre-runtime digest log depends on the configured consensus.
		// However, mismatching logs are simply ignored. Thus we construct fixtures which will
		// let the API to return a value in both BABE and AURA consensus.

		// Construct a `Digest` log fixture returning some value in BABE
		let primary_pre_digest = vec![0; <PrimaryPreDigest as MaxEncodedLen>::max_encoded_len()];
		let pre_digest =
			PreDigest::Primary(PrimaryPreDigest::decode(&mut &primary_pre_digest[..]).unwrap());
		frame_system::Pallet::<T>::deposit_log(DigestItem::PreRuntime(
			BABE_ENGINE_ID,
			pre_digest.encode(),
		));
		frame_system::Pallet::<T>::deposit_log(DigestItem::Seal(
			BABE_ENGINE_ID,
			pre_digest.encode(),
		));

		// Construct a `Digest` log fixture returning some value in AURA
		let slot = Slot::default();
		frame_system::Pallet::<T>::deposit_log(DigestItem::PreRuntime(
			AURA_ENGINE_ID,
			slot.encode(),
		));
		frame_system::Pallet::<T>::deposit_log(DigestItem::Seal(AURA_ENGINE_ID, slot.encode()));

		let result;
		#[block]
		{
			result = runtime.bench_block_author(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);

		let block_author = runtime.ext().block_author();
		assert_eq!(&memory[..], block_author.as_bytes());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_block_hash() {
		let mut memory = vec![0u8; 64];
		let mut setup = CallSetup::<T>::default();
		let input = setup.data();
		let (mut ext, _) = setup.ext();
		ext.set_block_number(BlockNumberFor::<T>::from(1u32));

		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, input);

		let block_hash = H256::from([1; 32]);

		// Store block hash in pallet-revive BlockHash mapping
		crate::BlockHash::<T>::insert(crate::BlockNumberFor::<T>::from(0u32), block_hash);

		let result;
		#[block]
		{
			result = runtime.bench_block_hash(memory.as_mut_slice(), 32, 0);
		}
		assert_ok!(result);
		assert_eq!(&memory[..32], &block_hash.0);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_now() {
		build_runtime!(runtime, memory: [[0u8;32], ]);
		let result;
		#[block]
		{
			result = runtime.bench_now(memory.as_mut_slice(), 0);
		}
		assert_ok!(result);
		assert_eq!(U256::from_little_endian(&memory[..]), runtime.ext().now());
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_copy_to_contract(n: Linear<0, { limits::code::BLOB_BYTES - 4 }>) {
		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::new(&mut ext, vec![]);
		let mut memory = memory!(n.encode(), vec![0u8; n as usize],);
		let result;
		#[block]
		{
			result = runtime.write_sandbox_output(
				memory.as_mut_slice(),
				4,
				0,
				&vec![42u8; n as usize],
				false,
				|_| None,
			);
		}
		assert_ok!(result);
		assert_eq!(&memory[..4], &n.encode());
		assert_eq!(&memory[4..], &vec![42u8; n as usize]);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_call_data_load() {
		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::new(&mut ext, vec![42u8; 32]);
		let mut memory = memory!(vec![0u8; 32],);
		let result;
		#[block]
		{
			result = runtime.bench_call_data_load(memory.as_mut_slice(), 0, 0);
		}
		assert_ok!(result);
		assert_eq!(&memory[..], &vec![42u8; 32]);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_call_data_copy(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::new(&mut ext, vec![42u8; n as usize]);
		let mut memory = memory!(vec![0u8; n as usize],);
		let result;
		#[block]
		{
			result = runtime.bench_call_data_copy(memory.as_mut_slice(), 0, n, 0);
		}
		assert_ok!(result);
		assert_eq!(&memory[..], &vec![42u8; n as usize]);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_return(n: Linear<0, { limits::CALLDATA_BYTES }>) {
		build_runtime!(runtime, memory: [n.to_le_bytes(), vec![42u8; n as usize], ]);

		let result;
		#[block]
		{
			result = runtime.bench_seal_return(memory.as_mut_slice(), 0, 0, n);
		}

		assert!(matches!(
			result,
			Err(crate::vm::pvm::TrapReason::Return(crate::vm::pvm::ReturnData { .. }))
		));
	}

	/// Benchmark the ocst of terminating a contract.
	///
	/// `r`: whether the old code will be removed as a result of this operation. (1: yes, 0: no)
	#[benchmark(pov_mode = Measured)]
	fn seal_terminate(r: Linear<0, 1>) -> Result<(), BenchmarkError> {
		let delete_code = r == 1;
		let beneficiary = account::<T::AccountId>("beneficiary", 0, 0);

		build_runtime!(runtime, instance, memory: [beneficiary.encode(),]);
		let code_hash = instance.info()?.code_hash;

		// Increment the refcount of the code hash so that it does not get deleted
		if !delete_code {
			<CodeInfo<T>>::increment_refcount(code_hash).unwrap();
		}

		let result;
		#[block]
		{
			result = runtime.bench_terminate(memory.as_mut_slice(), 0);
		}

		assert!(matches!(result, Err(crate::vm::pvm::TrapReason::Termination)));

		Ok(())
	}

	/// The balance work of `System.terminate`: the storage deposit refund when it is called and
	/// the teardown at the end of the call stack.
	#[benchmark(pov_mode = Measured)]
	fn seal_terminate_logic() -> Result<(), BenchmarkError> {
		let caller = whitelisted_caller();
		let beneficiary = account::<T::AccountId>("beneficiary", 0, 0);
		T::AddressMapper::map_no_deposit_unchecked(&beneficiary)?;

		build_runtime!(_runtime, instance, _memory: [vec![0u8; 0], ]);
		let code_hash = instance.info()?.code_hash;

		assert!(PristineCode::<T>::get(code_hash).is_some());

		T::Currency::set_balance(&instance.account_id, Pallet::<T>::min_balance() * 10u32.into());

		let storage_deposit = T::Currency::balance_on_hold(
			&HoldReason::StorageDepositReserve.into(),
			&instance.account_id,
		);
		NativeDepositOf::<T>::insert(&instance.account_id, &caller, storage_deposit);

		let mut transaction_meter = TransactionMeter::new(TransactionLimits::WeightAndDeposit {
			weight_limit: Default::default(),
			deposit_limit: BalanceOf::<T>::max_value(),
		})
		.unwrap();
		let exec_config = ExecConfig::new_substrate_tx();
		let contract_account = &instance.account_id;
		let origin = &ExecOrigin::from_account_id(caller);
		let beneficiary_clone = beneficiary.clone();
		let only_if_same_tx = false;

		let result;
		#[block]
		{
			result = crate::exec::bench_do_terminate::<T>(
				&mut transaction_meter,
				&exec_config,
				contract_account,
				&origin,
				beneficiary_clone,
				only_if_same_tx,
			);
		}
		result.unwrap();

		// Check that the contract is removed
		assert!(PristineCode::<T>::get(code_hash).is_none());

		// Check that the balance has been transferred away
		let balance = <T as Config>::Currency::total_balance(&instance.account_id);
		assert_eq!(balance, 0u32.into());

		// Check that the beneficiary received the balance
		let balance = <T as Config>::Currency::balance(&beneficiary);
		assert_eq!(balance, Pallet::<T>::min_balance() + Pallet::<T>::min_balance() * 9u32.into());

		Ok(())
	}

	// Benchmark the overhead that topics generate.
	// `t`: Number of topics
	// `n`: Size of event payload in bytes
	#[benchmark(pov_mode = Measured)]
	fn seal_deposit_event(
		t: Linear<0, { limits::NUM_EVENT_TOPICS as u32 }>,
		n: Linear<0, { limits::EVENT_BYTES }>,
	) {
		let num_topic = t as u32;
		let topics = (0..t).map(|i| H256::repeat_byte(i as u8)).collect::<Vec<_>>();
		let topics_data =
			topics.iter().flat_map(|hash| hash.as_bytes().to_vec()).collect::<Vec<u8>>();
		let data = vec![42u8; n as _];
		build_runtime!(runtime, instance, memory: [ topics_data, data, ]);

		let result;
		#[block]
		{
			result = runtime.bench_deposit_event(
				memory.as_mut_slice(),
				0, // topics_ptr
				num_topic,
				topics_data.len() as u32, // data_ptr
				n,                        // data_len
			);
		}
		assert_ok!(result);

		let events = System::<T>::events();
		let record = &events[events.len() - 1];

		assert_eq!(
			record.event,
			crate::Event::ContractEmitted { contract: instance.address, data, topics }.into(),
		);
	}

	enum TrieFill {
		Empty,
		Full,
	}

	enum SlotAccess {
		Cold,
		Hot,
	}

	fn build_storage_contract<T: Config>(
		op: StorageOp,
		fill: TrieFill,
	) -> Result<(ContractInfo<T>, Vec<u8>, Vec<u8>), BenchmarkError> {
		let key = vec![0u8; limits::STORAGE_KEY_BYTES as usize];
		let value = vec![1u8; limits::STORAGE_BYTES as usize];
		let initial_value = match op {
			StorageOp::Read => value.clone(),
			StorageOp::Write => vec![42u8; limits::STORAGE_BYTES as usize],
		};

		let instance = match fill {
			TrieFill::Full => {
				Contract::<T>::with_unbalanced_storage_trie(VmBinaryModule::dummy(), &key)?
			},
			TrieFill::Empty => Contract::<T>::new(VmBinaryModule::dummy(), vec![])?,
		};
		let info = instance.info()?;
		info.bench_write_raw(&key, Some(initial_value), false)
			.map_err(|_| "Failed to write to storage during setup.")?;
		Ok((info, key, value))
	}

	enum StorageCall {
		Clear,
		Contains,
		Take,
	}

	fn setup_precompile_bench<T: Config>(
		op: StorageCall,
		key_byte: u8,
		access: SlotAccess,
	) -> Result<(CallSetup<T>, Key, Vec<u8>), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![key_byte; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let raw_key = vec![key_byte; max_key_len as usize].into();
		let input_bytes = match op {
			StorageCall::Clear => {
				IStorage::IStorageCalls::clearStorage(IStorage::clearStorageCall {
					flags: StorageFlags::empty().bits(),
					key: raw_key,
					isFixedKey: false,
				})
			},
			StorageCall::Contains => {
				IStorage::IStorageCalls::containsStorage(IStorage::containsStorageCall {
					flags: StorageFlags::empty().bits(),
					key: raw_key,
					isFixedKey: false,
				})
			},
			StorageCall::Take => IStorage::IStorageCalls::takeStorage(IStorage::takeStorageCall {
				flags: StorageFlags::empty().bits(),
				key: raw_key,
				isFixedKey: false,
			}),
		}
		.abi_encode();

		let call_setup = CallSetup::<T>::default();
		if matches!(access, SlotAccess::Hot) {
			let info = call_setup.contract().info()?;
			frame_benchmarking::add_to_whitelist_child(
				info.child_trie_info().storage_key().to_vec(),
				key.hash(),
			);
		}
		Ok((call_setup, key, input_bytes))
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn get_storage_empty() -> Result<(), BenchmarkError> {
		let (info, key, value) = build_storage_contract::<T>(StorageOp::Read, TrieFill::Empty)?;
		let child_trie_info = info.child_trie_info();

		let result;
		#[block]
		{
			result = child::get_raw(&child_trie_info, &key);
		}

		assert_eq!(result, Some(value));
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn get_storage_full() -> Result<(), BenchmarkError> {
		let (info, key, value) = build_storage_contract::<T>(StorageOp::Read, TrieFill::Full)?;
		let child_trie_info = info.child_trie_info();

		let result;
		#[block]
		{
			result = child::get_raw(&child_trie_info, &key);
		}

		assert_eq!(result, Some(value));
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn set_storage_empty() -> Result<(), BenchmarkError> {
		let (info, key, value) = build_storage_contract::<T>(StorageOp::Write, TrieFill::Empty)?;

		let val = Some(value.clone());
		let result;
		#[block]
		{
			result = info.bench_write_raw(&key, val, true);
		}

		assert_ok!(result);
		assert_eq!(child::get_raw(&info.child_trie_info(), &key).unwrap(), value);
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn set_storage_full() -> Result<(), BenchmarkError> {
		let (info, key, value) = build_storage_contract::<T>(StorageOp::Write, TrieFill::Full)?;

		let val = Some(value.clone());
		let result;
		#[block]
		{
			result = info.bench_write_raw(&key, val, true);
		}

		assert_ok!(result);
		assert_eq!(child::get_raw(&info.child_trie_info(), &key).unwrap(), value);
		Ok(())
	}

	/// Worst-case lookup keys: differing only at the tail, comparisons walk the whole key.
	fn shared_prefix_keys(count: usize, suffix_of: impl Fn(u64) -> u64) -> Vec<Vec<u8>> {
		(0..count as u64)
			.map(|i| {
				let mut key = vec![0u8; limits::STORAGE_KEY_BYTES as usize];
				key[limits::STORAGE_KEY_BYTES as usize - 8..]
					.copy_from_slice(&suffix_of(i).to_be_bytes());
				key
			})
			.collect()
	}

	/// Probed keys must exist, or reads measure a proof-of-absence walk instead.
	/// Whitelisting keeps them out of the DB counters and pre-warms the cache.
	fn setup_stored_keys<T: Config>(
		count: usize,
		value_byte: u8,
		suffix_of: impl Fn(u64) -> u64,
	) -> Result<(ContractInfo<T>, Vec<Vec<u8>>), BenchmarkError> {
		let instance = Contract::<T>::new(VmBinaryModule::dummy(), vec![])?;
		let info = instance.info()?;
		let child_trie_info = info.child_trie_info();
		let value = vec![value_byte; limits::STORAGE_BYTES as usize];
		let stored_keys = shared_prefix_keys(count, suffix_of);
		for key in &stored_keys {
			info.bench_write_raw(key, Some(value.clone()), false)
				.map_err(|_| "Failed to write to storage during setup.")?;
			frame_benchmarking::add_to_whitelist_child(
				child_trie_info.storage_key().to_vec(),
				key.clone(),
			);
		}
		Ok((info, stored_keys))
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn overlay_probe_full(
		n: Linear<0, { MAX_ACCESS_LIST_ENTRIES as u32 }>,
	) -> Result<(), BenchmarkError> {
		let value_byte = 42;
		// One stored key per probe; odd suffixes, so the even fill never overwrites them.
		let (info, stored_keys) =
			setup_stored_keys::<T>(MAX_ACCESS_LIST_ENTRIES, value_byte, |i| i * 2 + 1)?;
		let child_trie_info = info.child_trie_info();
		// The block's PoV budget bounds the overlay entries like the access list cap.
		let fill_keys = shared_prefix_keys(MAX_ACCESS_LIST_ENTRIES, |i| (i + 1) * 2);

		let mut result = None;
		#[block]
		{
			// The benchmark framework drains the overlay right before the block, so fill here.
			for key in &fill_keys {
				child::put_raw(&child_trie_info, key, &[0u8]);
			}
			for i in 0..n {
				let index = i as usize % stored_keys.len();
				result = child::get_raw(&child_trie_info, &stored_keys[index]);
			}
		}

		if n > 0 {
			let expected = vec![value_byte; limits::STORAGE_BYTES as usize];
			assert_eq!(result, Some(expected), "the stored value must be read back");
		}
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn overlay_probe_empty(
		n: Linear<0, { MAX_ACCESS_LIST_ENTRIES as u32 }>,
	) -> Result<(), BenchmarkError> {
		let value_byte = 42;
		// Reads must cost the same as in `overlay_probe_full`, so their shared cost cancels.
		let (info, stored_keys) =
			setup_stored_keys::<T>(MAX_ACCESS_LIST_ENTRIES, value_byte, |i| i * 2 + 1)?;
		let child_trie_info = info.child_trie_info();

		let mut result = None;
		#[block]
		{
			for i in 0..n {
				let index = i as usize % stored_keys.len();
				result = child::get_raw(&child_trie_info, &stored_keys[index]);
			}
		}

		if n > 0 {
			let expected = vec![value_byte; limits::STORAGE_BYTES as usize];
			assert_eq!(result, Some(expected), "the stored value must be read back");
		}
		Ok(())
	}

	// n: new byte size
	// o: old byte size
	#[benchmark(skip_meta, pov_mode = Measured)]
	fn seal_set_storage(
		n: Linear<0, { limits::STORAGE_BYTES }>,
		o: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let value = vec![1u8; n as usize];

		build_runtime!(runtime, instance, memory: [ key.unhashed(), value.clone(), ]);
		let info = instance.info()?;

		info.write(&key, Some(vec![42u8; o as usize]), None, false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		let result;
		#[block]
		{
			result = runtime.bench_set_storage(
				memory.as_mut_slice(),
				StorageFlags::empty().bits(),
				0,           // key_ptr
				max_key_len, // key_len
				max_key_len, // value_ptr
				n,           // value_len
			);
		}

		assert_ok!(result);
		assert_eq!(info.read(&key).unwrap(), value);
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn seal_set_storage_hot(
		n: Linear<0, { limits::STORAGE_BYTES }>,
		o: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let value = vec![1u8; n as usize];

		build_runtime!(runtime, instance, memory: [ key.unhashed(), value.clone(), ]);
		let info = instance.info()?;

		info.write(&key, Some(vec![42u8; o as usize]), None, false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		frame_benchmarking::add_to_whitelist_child(
			info.child_trie_info().storage_key().to_vec(),
			key.hash(),
		);

		// Add the key to access list so the op's touch is hot.
		runtime.ext().touch_storage_access(false, &key, StorageOp::Write);

		let result;
		#[block]
		{
			result = runtime.bench_set_storage(
				memory.as_mut_slice(),
				StorageFlags::empty().bits(),
				0,           // key_ptr
				max_key_len, // key_len
				max_key_len, // value_ptr
				n,           // value_len
			);
		}

		assert_ok!(result);
		assert_eq!(info.read(&key).unwrap(), value);
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn clear_storage(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let key_byte = 0;
		let (mut call_setup, key, input_bytes) =
			setup_precompile_bench::<T>(StorageCall::Clear, key_byte, SlotAccess::Cold)?;
		let (mut ext, _) = call_setup.ext();
		ext.set_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_storage(&key).is_none());

		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn clear_storage_hot(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let key_byte = 0;
		let (mut call_setup, key, input_bytes) =
			setup_precompile_bench::<T>(StorageCall::Clear, key_byte, SlotAccess::Hot)?;
		let (mut ext, _) = call_setup.ext();
		ext.set_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		ext.touch_storage_access(false, &key, StorageOp::Write);

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_storage(&key).is_none());

		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn seal_get_storage(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		build_runtime!(runtime, instance, memory: [ key.unhashed(), n.to_le_bytes(), vec![0u8; n as _], ]);
		let info = instance.info()?;

		info.write(&key, Some(vec![42u8; n as usize]), None, false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		let out_ptr = max_key_len + 4;
		let result;
		#[block]
		{
			result = runtime.bench_get_storage(
				memory.as_mut_slice(),
				StorageFlags::empty().bits(),
				0,           // key_ptr
				max_key_len, // key_len
				out_ptr,     // out_ptr
				max_key_len, // out_len_ptr
			);
		}

		assert_ok!(result);
		assert_eq!(&info.read(&key).unwrap(), &memory[out_ptr as usize..]);
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn seal_get_storage_hot(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		build_runtime!(runtime, instance, memory: [ key.unhashed(), n.to_le_bytes(), vec![0u8; n as _], ]);
		let info = instance.info()?;

		info.write(&key, Some(vec![42u8; n as usize]), None, false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		frame_benchmarking::add_to_whitelist_child(
			info.child_trie_info().storage_key().to_vec(),
			key.hash(),
		);

		runtime.ext().touch_storage_access(false, &key, StorageOp::Read);

		let out_ptr = max_key_len + 4;
		let result;
		#[block]
		{
			result = runtime.bench_get_storage(
				memory.as_mut_slice(),
				StorageFlags::empty().bits(),
				0,           // key_ptr
				max_key_len, // key_len
				out_ptr,     // out_ptr
				max_key_len, // out_len_ptr
			);
		}

		assert_ok!(result);
		assert_eq!(&info.read(&key).unwrap(), &memory[out_ptr as usize..]);
		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn contains_storage(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let key_byte = 0;
		let (mut call_setup, key, input_bytes) =
			setup_precompile_bench::<T>(StorageCall::Contains, key_byte, SlotAccess::Cold)?;
		let (mut ext, _) = call_setup.ext();
		ext.set_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_storage(&key).is_some());

		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn contains_storage_hot(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let key_byte = 0;
		let (mut call_setup, key, input_bytes) =
			setup_precompile_bench::<T>(StorageCall::Contains, key_byte, SlotAccess::Hot)?;
		let (mut ext, _) = call_setup.ext();
		ext.set_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		ext.touch_storage_access(false, &key, StorageOp::Read);

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_storage(&key).is_some());

		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn take_storage(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let key_byte = 3;
		let (mut call_setup, key, input_bytes) =
			setup_precompile_bench::<T>(StorageCall::Take, key_byte, SlotAccess::Cold)?;
		let (mut ext, _) = call_setup.ext();
		ext.set_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_storage(&key).is_none());

		Ok(())
	}

	#[benchmark(skip_meta, pov_mode = Measured)]
	fn take_storage_hot(n: Linear<0, { limits::STORAGE_BYTES }>) -> Result<(), BenchmarkError> {
		let key_byte = 3;
		let (mut call_setup, key, input_bytes) =
			setup_precompile_bench::<T>(StorageCall::Take, key_byte, SlotAccess::Hot)?;
		let (mut ext, _) = call_setup.ext();
		ext.set_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to storage during setup.")?;

		ext.touch_storage_access(false, &key, StorageOp::Write);

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_storage(&key).is_none());

		Ok(())
	}

	fn worst_case_slot() -> crate::access_list::Slot {
		let key = Key::try_from_var(vec![0xFFu8; limits::STORAGE_KEY_BYTES as usize])
			.expect("key fits STORAGE_KEY_BYTES bound; qed");
		crate::access_list::Slot::from(&key)
	}

	fn near_full_access_list() -> crate::access_list::AccessList {
		let mut al = AccessList::new();
		for i in 0..(MAX_ACCESS_LIST_ENTRIES - 1) {
			al.touch(
				AccessEntry { slot: worst_case_slot(), address: H160::from_low_u64_be(i as u64) },
				StorageOp::Read,
			);
		}
		al
	}

	#[benchmark(pov_mode = Ignored)]
	fn access_list_touch_cold_full() -> Result<(), BenchmarkError> {
		let mut al = near_full_access_list();
		// Insert a new entry (u64::MAX is past the fill range, so the touch is cold).
		let entry =
			AccessEntry { slot: worst_case_slot(), address: H160::from_low_u64_be(u64::MAX) };
		let outcome;
		#[block]
		{
			outcome = al.touch(entry, StorageOp::Read);
		}
		assert!(outcome.is_cold());
		Ok(())
	}

	#[benchmark(pov_mode = Ignored)]
	fn access_list_touch_hot_full() -> Result<(), BenchmarkError> {
		let mut al = near_full_access_list();
		// Worst-case hot touch: the rightmost key and the write upgrades the read-paid entry.
		let entry = AccessEntry {
			slot: worst_case_slot(),
			address: H160::from_low_u64_be(MAX_ACCESS_LIST_ENTRIES as u64 - 2),
		};
		let outcome;
		#[block]
		{
			outcome = al.touch(entry.clone(), StorageOp::Write);
		}
		assert_eq!(
			outcome,
			Warmth::Hot { charged: StorageOp::Read },
			"the fill seeded this entry read-paid"
		);
		assert_eq!(
			al.peek(&entry),
			Warmth::Hot { charged: StorageOp::Write },
			"the write upgraded the entry"
		);
		Ok(())
	}

	#[benchmark(pov_mode = Ignored)]
	fn access_list_touch_cold_empty() -> Result<(), BenchmarkError> {
		let mut al = AccessList::new();
		let entry =
			AccessEntry { slot: worst_case_slot(), address: H160::from_low_u64_be(u64::MAX) };
		let outcome;
		#[block]
		{
			outcome = al.touch(entry, StorageOp::Read);
		}
		assert!(outcome.is_cold());
		Ok(())
	}

	#[benchmark(pov_mode = Ignored)]
	fn access_list_touch_hot_single_element() -> Result<(), BenchmarkError> {
		let mut al = AccessList::new();
		let entry =
			AccessEntry { slot: worst_case_slot(), address: H160::from_low_u64_be(u64::MAX) };
		al.touch(entry.clone(), StorageOp::Read);
		let outcome;
		#[block]
		{
			outcome = al.touch(entry, StorageOp::Read);
		}
		assert!(!outcome.is_cold());
		Ok(())
	}

	// Per-entry rollback cost, prepaid by every cold touch since a frame revert
	// can't charge gas itself. Isolated by reverting a frame with exactly one
	// journaled entry on top of a near-full `AccessList`.
	#[benchmark(pov_mode = Ignored)]
	fn access_list_rollback_amortization() -> Result<(), BenchmarkError> {
		let mut al = near_full_access_list();
		al.enter_frame();
		al.touch(
			AccessEntry { slot: worst_case_slot(), address: H160::from_low_u64_be(u64::MAX) },
			StorageOp::Read,
		);
		#[block]
		{
			al.rollback_frame();
		}
		Ok(())
	}

	// We use both full and empty benchmarks here instead of benchmarking transient_storage
	// (BTreeMap) directly. This approach is necessary because benchmarking this BTreeMap is very
	// slow. Additionally, we use linear regression for our benchmarks, and the BTreeMap's log(n)
	// complexity can introduce approximation errors.
	#[benchmark(pov_mode = Ignored)]
	fn set_transient_storage_empty() -> Result<(), BenchmarkError> {
		let max_value_len = limits::STORAGE_BYTES;
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let value = Some(vec![42u8; max_value_len as _]);
		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		let result;
		#[block]
		{
			result = runtime.ext().set_transient_storage(&key, value, false);
		}

		assert_eq!(result, Ok(WriteOutcome::New));
		assert_eq!(runtime.ext().get_transient_storage(&key), Some(vec![42u8; max_value_len as _]));
		Ok(())
	}

	#[benchmark(pov_mode = Ignored)]
	fn set_transient_storage_full() -> Result<(), BenchmarkError> {
		let max_value_len = limits::STORAGE_BYTES;
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let value = Some(vec![42u8; max_value_len as _]);
		let mut setup = CallSetup::<T>::default();
		setup.set_transient_storage_size(limits::TRANSIENT_STORAGE_BYTES);
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		let result;
		#[block]
		{
			result = runtime.ext().set_transient_storage(&key, value, false);
		}

		assert_eq!(result, Ok(WriteOutcome::New));
		assert_eq!(runtime.ext().get_transient_storage(&key), Some(vec![42u8; max_value_len as _]));
		Ok(())
	}

	#[benchmark(pov_mode = Ignored)]
	fn get_transient_storage_empty() -> Result<(), BenchmarkError> {
		let max_value_len = limits::STORAGE_BYTES;
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;

		let mut setup = CallSetup::<T>::default();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		runtime
			.ext()
			.set_transient_storage(&key, Some(vec![42u8; max_value_len as _]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;
		let result;
		#[block]
		{
			result = runtime.ext().get_transient_storage(&key);
		}

		assert_eq!(result, Some(vec![42u8; max_value_len as _]));
		Ok(())
	}

	#[benchmark(pov_mode = Ignored)]
	fn get_transient_storage_full() -> Result<(), BenchmarkError> {
		let max_value_len = limits::STORAGE_BYTES;
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;

		let mut setup = CallSetup::<T>::default();
		setup.set_transient_storage_size(limits::TRANSIENT_STORAGE_BYTES);
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		runtime
			.ext()
			.set_transient_storage(&key, Some(vec![42u8; max_value_len as _]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;
		let result;
		#[block]
		{
			result = runtime.ext().get_transient_storage(&key);
		}

		assert_eq!(result, Some(vec![42u8; max_value_len as _]));
		Ok(())
	}

	// The weight of journal rollbacks should be taken into account when setting storage.
	#[benchmark(pov_mode = Ignored)]
	fn rollback_transient_storage() -> Result<(), BenchmarkError> {
		let max_value_len = limits::STORAGE_BYTES;
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;

		let mut setup = CallSetup::<T>::default();
		setup.set_transient_storage_size(limits::TRANSIENT_STORAGE_BYTES);
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		runtime.ext().transient_storage().start_transaction();
		runtime
			.ext()
			.set_transient_storage(&key, Some(vec![42u8; max_value_len as _]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;
		#[block]
		{
			runtime.ext().transient_storage().rollback_transaction();
		}

		assert_eq!(runtime.ext().get_transient_storage(&key), None);
		Ok(())
	}

	// n: new byte size
	// o: old byte size
	#[benchmark(pov_mode = Measured)]
	fn seal_set_transient_storage(
		n: Linear<0, { limits::STORAGE_BYTES }>,
		o: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let value = vec![1u8; n as usize];
		build_runtime!(runtime, memory: [ key.unhashed(), value.clone(), ]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		runtime
			.ext()
			.set_transient_storage(&key, Some(vec![42u8; o as usize]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;

		let result;
		#[block]
		{
			result = runtime.bench_set_storage(
				memory.as_mut_slice(),
				StorageFlags::TRANSIENT.bits(),
				0,           // key_ptr
				max_key_len, // key_len
				max_key_len, // value_ptr
				n,           // value_len
			);
		}

		assert_ok!(result);
		assert_eq!(runtime.ext().get_transient_storage(&key).unwrap(), value);
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_clear_transient_storage(
		n: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		let input_bytes = IStorage::IStorageCalls::clearStorage(IStorage::clearStorageCall {
			flags: StorageFlags::TRANSIENT.bits(),
			key: vec![0u8; max_key_len as usize].into(),
			isFixedKey: false,
		})
		.abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();
		ext.set_transient_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert_ok!(result);
		assert!(ext.get_transient_storage(&key).is_none());

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_get_transient_storage(
		n: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;
		build_runtime!(runtime, memory: [ key.unhashed(), n.to_le_bytes(), vec![0u8; n as _], ]);
		runtime.ext().transient_storage().meter().current_mut().limit = u32::MAX;
		runtime
			.ext()
			.set_transient_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;

		let out_ptr = max_key_len + 4;
		let result;
		#[block]
		{
			result = runtime.bench_get_storage(
				memory.as_mut_slice(),
				StorageFlags::TRANSIENT.bits(),
				0,           // key_ptr
				max_key_len, // key_len
				out_ptr,     // out_ptr
				max_key_len, // out_len_ptr
			);
		}

		assert_ok!(result);
		assert_eq!(
			&runtime.ext().get_transient_storage(&key).unwrap(),
			&memory[out_ptr as usize..]
		);
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_contains_transient_storage(
		n: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;

		let input_bytes = IStorage::IStorageCalls::containsStorage(IStorage::containsStorageCall {
			flags: StorageFlags::TRANSIENT.bits(),
			key: vec![0u8; max_key_len as usize].into(),
			isFixedKey: false,
		})
		.abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();
		ext.set_transient_storage(&key, Some(vec![42u8; n as usize]), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert!(result.is_ok());
		assert!(ext.get_transient_storage(&key).is_some());

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_take_transient_storage(
		n: Linear<0, { limits::STORAGE_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let n = limits::STORAGE_BYTES;
		let value = vec![42u8; n as usize];
		let max_key_len = limits::STORAGE_KEY_BYTES;
		let key = Key::try_from_var(vec![0u8; max_key_len as usize])
			.map_err(|_| "Key has wrong length")?;

		let input_bytes = IStorage::IStorageCalls::takeStorage(IStorage::takeStorageCall {
			flags: StorageFlags::TRANSIENT.bits(),
			key: vec![0u8; max_key_len as usize].into(),
			isFixedKey: false,
		})
		.abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();
		ext.set_transient_storage(&key, Some(value), false)
			.map_err(|_| "Failed to write to transient storage during setup.")?;

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkStorage::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		assert!(result.is_ok());
		assert!(ext.get_transient_storage(&key).is_none());

		Ok(())
	}

	// t: with or without some value to transfer
	// d: with or without dust value to transfer
	// i: size of the input data
	#[benchmark(pov_mode = Measured)]
	fn seal_call(t: Linear<0, 1>, d: Linear<0, 1>, i: Linear<0, { limits::code::BLOB_BYTES }>) {
		let Contract { account_id: callee, address: callee_addr, .. } =
			Contract::<T>::with_index(1, VmBinaryModule::dummy(), vec![]).unwrap();

		let callee_bytes = callee.encode();
		let callee_len = callee_bytes.len() as u32;

		let value: BalanceOf<T> = (1_000_000u32 * t).into();
		let dust = 100u32 * d;
		let evm_value =
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(value, dust));
		let value_bytes = evm_value.encode();

		let deposit: BalanceOf<T> = (u32::MAX - 100).into();
		let deposit_bytes = Into::<U256>::into(deposit).encode();
		let deposit_len = deposit_bytes.len() as u32;

		let mut setup = CallSetup::<T>::default();
		setup.set_storage_deposit_limit(deposit);
		// We benchmark the overhead of cloning the input. Not passing it to the contract.
		// This is why we set the input here instead of passig it as pointer to the `bench_call`.
		setup.set_data(vec![42; i as usize]);
		setup.set_origin(ExecOrigin::from_account_id(setup.contract().account_id.clone()));
		setup.set_balance(value + 1u32.into() + Pallet::<T>::min_balance());

		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		let mut memory = memory!(callee_bytes, deposit_bytes, value_bytes,);

		let result;
		#[block]
		{
			result = runtime.bench_call(
				memory.as_mut_slice(),
				pack_hi_lo(CallFlags::CLONE_INPUT.bits(), 0), // flags + callee
				u64::MAX,                                     // ref_time_limit
				u64::MAX,                                     // proof_size_limit
				pack_hi_lo(callee_len, callee_len + deposit_len), // deposit_ptr + value_pr
				pack_hi_lo(0, 0),                             // input len + data ptr
				pack_hi_lo(0, SENTINEL),                      // output len + data ptr
			);
		}

		assert_eq!(result.unwrap(), ReturnErrorCode::Success);
		assert_eq!(
			Pallet::<T>::evm_balance(&callee_addr),
			evm_value,
			"{callee_addr:?} balance should hold {evm_value:?}"
		);
	}

	// d: 1 if the associated pre-compile has a contract info that needs to be loaded
	// i: size of the input data
	#[benchmark(pov_mode = Measured)]
	fn seal_call_precompile(d: Linear<0, 1>, i: Linear<0, { limits::CALLDATA_BYTES - 100 }>) {
		use alloy_core::sol_types::SolInterface;
		use precompiles::{BenchmarkNoInfo, BenchmarkWithInfo, BuiltinPrecompile, IBenchmarking};

		let callee_bytes = if d == 1 {
			BenchmarkWithInfo::<T>::MATCHER.base_address().to_vec()
		} else {
			BenchmarkNoInfo::<T>::MATCHER.base_address().to_vec()
		};
		let callee_len = callee_bytes.len() as u32;

		let deposit: BalanceOf<T> = (u32::MAX - 100).into();
		let deposit_bytes = Into::<U256>::into(deposit).encode();
		let deposit_len = deposit_bytes.len() as u32;

		let value: BalanceOf<T> = Zero::zero();
		let value_bytes = Into::<U256>::into(value).encode();
		let value_len = value_bytes.len() as u32;

		let input_bytes = IBenchmarking::IBenchmarkingCalls::bench(IBenchmarking::benchCall {
			input: vec![42_u8; i as usize].into(),
		})
		.abi_encode();
		let input_len = input_bytes.len() as u32;

		let mut setup = CallSetup::<T>::default();
		setup.set_storage_deposit_limit(deposit);

		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		let mut memory = memory!(callee_bytes, deposit_bytes, value_bytes, input_bytes,);

		let mut do_benchmark = || {
			runtime.bench_call(
				memory.as_mut_slice(),
				pack_hi_lo(0, 0), // flags + callee
				u64::MAX,         // ref_time_limit
				u64::MAX,         // proof_size_limit
				pack_hi_lo(callee_len, callee_len + deposit_len), /* deposit_ptr +
				                   * value_pr */
				pack_hi_lo(input_len, callee_len + deposit_len + value_len), /* input len +
				                                                              * input ptr */
				pack_hi_lo(0, SENTINEL), // output len + output ptr
			)
		};

		// first call of the pre-compile will create its contract info and account
		// so we make sure to create it
		assert_eq!(do_benchmark().unwrap(), ReturnErrorCode::Success);

		let result;
		#[block]
		{
			result = do_benchmark();
		}

		assert_eq!(result.unwrap(), ReturnErrorCode::Success);
	}

	#[benchmark(pov_mode = Measured)]
	fn seal_delegate_call() -> Result<(), BenchmarkError> {
		let Contract { account_id: address, .. } =
			Contract::<T>::with_index(1, VmBinaryModule::dummy(), vec![]).unwrap();

		let address_bytes = address.encode();
		let address_len = address_bytes.len() as u32;

		let deposit: BalanceOf<T> = (u32::MAX - 100).into();
		let deposit_bytes = Into::<U256>::into(deposit).encode();

		let mut setup = CallSetup::<T>::default();
		setup.set_storage_deposit_limit(deposit);
		setup.set_origin(ExecOrigin::from_account_id(setup.contract().account_id.clone()));

		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);
		let mut memory = memory!(address_bytes, deposit_bytes,);

		let result;
		#[block]
		{
			result = runtime.bench_delegate_call(
				memory.as_mut_slice(),
				pack_hi_lo(0, 0),        // flags + address ptr
				u64::MAX,                // ref_time_limit
				u64::MAX,                // proof_size_limit
				address_len,             // deposit_ptr
				pack_hi_lo(0, 0),        // input len + data ptr
				pack_hi_lo(0, SENTINEL), // output len + ptr
			);
		}

		assert_eq!(result.unwrap(), ReturnErrorCode::Success);
		Ok(())
	}

	// t: with or without some value to transfer
	// d: with or without dust value to transfer
	// i: size of the input data
	#[benchmark(pov_mode = Measured)]
	fn seal_instantiate(
		t: Linear<0, 1>,
		d: Linear<0, 1>,
		i: Linear<0, { limits::CALLDATA_BYTES }>,
	) -> Result<(), BenchmarkError> {
		let code = VmBinaryModule::dummy();
		let hash = Contract::<T>::with_index(1, VmBinaryModule::dummy(), vec![])?.info()?.code_hash;
		let hash_bytes = hash.encode();

		let value: BalanceOf<T> = (1_000_000u32 * t).into();
		let dust = 100u32 * d;
		let evm_value =
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(value, dust));
		let value_bytes = evm_value.encode();
		let value_len = value_bytes.len() as u32;

		let deposit: BalanceOf<T> = BalanceOf::<T>::max_value();
		let deposit_bytes = Into::<U256>::into(deposit).encode();
		let deposit_len = deposit_bytes.len() as u32;

		let mut setup = CallSetup::<T>::default();
		setup.set_origin(ExecOrigin::from_account_id(setup.contract().account_id.clone()));
		setup.set_balance(value + 1u32.into() + (Pallet::<T>::min_balance() * 2u32.into()));

		let account_id = &setup.contract().account_id.clone();
		let (mut ext, _) = setup.ext();
		let mut runtime = pvm::Runtime::<_, [u8]>::new(&mut ext, vec![]);

		let input = vec![42u8; i as _];
		let input_len = hash_bytes.len() as u32 + input.len() as u32;
		let salt = [42u8; 32];
		let deployer = T::AddressMapper::to_address(&account_id);
		let addr = crate::address::create2(&deployer, &code.code, &input, &salt);
		let mut memory = memory!(hash_bytes, input, deposit_bytes, value_bytes, salt,);

		let mut offset = {
			let mut current = 0u32;
			move |after: u32| {
				current += after;
				current
			}
		};

		assert!(AccountInfoOf::<T>::get(&addr).is_none());

		let result;
		#[block]
		{
			result = runtime.bench_instantiate(
				memory.as_mut_slice(),
				u64::MAX,                                           // ref_time_limit
				u64::MAX,                                           // proof_size_limit
				pack_hi_lo(offset(input_len), offset(deposit_len)), // deposit_ptr + value_ptr
				pack_hi_lo(input_len, 0),                           // input_data_len + input_data
				pack_hi_lo(0, SENTINEL),                            // output_len_ptr + output_ptr
				pack_hi_lo(SENTINEL, offset(value_len)),            // address_ptr + salt_ptr
			);
		}

		assert_eq!(result.unwrap(), ReturnErrorCode::Success);
		assert!(AccountInfo::<T>::load_contract(&addr).is_some());

		assert_eq!(
			Pallet::<T>::evm_balance(&addr),
			evm_value,
			"{addr:?} balance should hold {evm_value:?}"
		);
		Ok(())
	}

	// t: with or without some value to transfer
	// d: with or without dust value to transfer
	// i: size of the init code (max 49152 bytes per EIP-3860)
	#[benchmark(pov_mode = Measured)]
	fn evm_instantiate(
		t: Linear<0, 1>,
		d: Linear<0, 1>,
		i: Linear<{ 10 * 1024 }, { 48 * 1024 }>,
	) -> Result<(), BenchmarkError> {
		use crate::vm::evm::instructions::BENCH_INIT_CODE;
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_init_code_for_runtime_size(0));
		setup.set_origin(ExecOrigin::from_account_id(setup.contract().account_id.clone()));
		setup.set_balance(caller_funding::<T>());

		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(Default::default(), Default::default(), &mut ext);

		let value = {
			let value: BalanceOf<T> = (1_000_000u32 * t).into();
			let dust = 100u32 * d;
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(value, dust))
		};

		let init_code = vec![BENCH_INIT_CODE; i as usize];
		let _ = interpreter.memory.resize(0, init_code.len());
		let salt = U256::from(42u64);
		interpreter.memory.set_data(0, 0, init_code.len(), &init_code);

		// Setup stack for create instruction [value, offset, size, salt]
		let _ = interpreter.stack.push(salt);
		let _ = interpreter.stack.push(U256::from(init_code.len()));
		let _ = interpreter.stack.push(U256::zero());
		let _ = interpreter.stack.push(value);

		let result;
		#[block]
		{
			result = instructions::contract::create::<true, _>(&mut interpreter);
		}

		assert!(result.is_continue());
		let addr = interpreter.stack.top().unwrap().into_address();
		assert!(AccountInfo::<T>::load_contract(&addr).is_some());
		assert_eq!(Pallet::<T>::code(&addr).len(), revm::primitives::eip170::MAX_CODE_SIZE);
		assert_eq!(Pallet::<T>::evm_balance(&addr), value, "balance should hold {value:?}");
		Ok(())
	}

	// `n`: Input to hash in bytes
	#[benchmark(pov_mode = Measured)]
	fn sha2_256(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		let input = vec![0u8; n as usize];
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160::from_low_u64_be(2).as_fixed_bytes(),
				input.clone(),
			);
		}
		assert_eq!(sp_io::hashing::sha2_256(&input).to_vec(), result.unwrap().data);
	}

	#[benchmark(pov_mode = Measured)]
	fn identity(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		let input = vec![0u8; n as usize];
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160::from_low_u64_be(4).as_fixed_bytes(),
				input.clone(),
			);
		}
		assert_eq!(input, result.unwrap().data);
	}

	// `n`: Input to hash in bytes
	#[benchmark(pov_mode = Measured)]
	fn ripemd_160(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		use ripemd::Digest;
		let input = vec![0u8; n as usize];
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160::from_low_u64_be(3).as_fixed_bytes(),
				input.clone(),
			);
		}
		let mut expected = [0u8; 32];
		expected[12..32].copy_from_slice(&ripemd::Ripemd160::digest(input));

		assert_eq!(expected.to_vec(), result.unwrap().data);
	}

	// `n`: Input to hash in bytes
	#[benchmark(pov_mode = Measured)]
	fn seal_hash_keccak_256(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		build_runtime!(runtime, memory: [[0u8; 32], vec![0u8; n as usize], ]);

		let result;
		#[block]
		{
			result = runtime.bench_hash_keccak_256(memory.as_mut_slice(), 32, n, 0);
		}
		assert_eq!(sp_io::hashing::keccak_256(&memory[32..]), &memory[0..32]);
		assert_ok!(result);
	}

	// `n`: Input to hash in bytes
	#[benchmark(pov_mode = Measured)]
	fn hash_blake2_256(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		let input = vec![0u8; n as usize];
		let input_bytes = ISystem::ISystemCalls::hashBlake256(ISystem::hashBlake256Call {
			input: input.clone().into(),
		})
		.abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let truth: [u8; 32] = sp_io::hashing::blake2_256(&input);
		let truth = FixedBytes::<32>::abi_encode(&truth);
		let truth = FixedBytes::<32>::abi_decode(&truth[..]).expect("decoding failed");

		let raw_data = result.unwrap().data;
		let ret_hash = FixedBytes::<32>::abi_decode(&raw_data[..]).expect("decoding failed");
		assert_eq!(truth, ret_hash);
	}

	// `n`: Input to hash in bytes
	#[benchmark(pov_mode = Measured)]
	fn hash_blake2_128(n: Linear<0, { limits::code::BLOB_BYTES }>) {
		let input = vec![0u8; n as usize];
		let input_bytes = ISystem::ISystemCalls::hashBlake128(ISystem::hashBlake128Call {
			input: input.clone().into(),
		})
		.abi_encode();

		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160(BenchmarkSystem::<T>::MATCHER.base_address()).as_fixed_bytes(),
				input_bytes,
			);
		}
		let truth: [u8; 16] = sp_io::hashing::blake2_128(&input);
		let truth = FixedBytes::<16>::abi_encode(&truth);
		let truth = FixedBytes::<16>::abi_decode(&truth[..]).expect("decoding failed");

		let raw_data = result.unwrap().data;
		let ret_hash = FixedBytes::<16>::abi_decode(&raw_data[..]).expect("decoding failed");
		assert_eq!(truth, ret_hash);
	}

	// `n`: Message input length to verify in bytes.
	// need some buffer so the code size does not exceed the max code size.
	#[benchmark(pov_mode = Measured)]
	fn seal_sr25519_verify(n: Linear<0, { limits::code::BLOB_BYTES - 255 }>) {
		let message = (0..n).zip((32u8..127u8).cycle()).map(|(_, c)| c).collect::<Vec<_>>();
		let message_len = message.len() as u32;

		let key_type = sp_core::crypto::KeyTypeId(*b"code");
		let pub_key = sp_io::crypto::sr25519_generate(key_type, None);
		let sig =
			sp_io::crypto::sr25519_sign(key_type, &pub_key, &message).expect("Generates signature");
		let sig = AsRef::<[u8; 64]>::as_ref(&sig).to_vec();
		let sig_len = sig.len() as u32;

		build_runtime!(runtime, memory: [sig, pub_key.to_vec(), message, ]);

		let result;
		#[block]
		{
			result = runtime.bench_sr25519_verify(
				memory.as_mut_slice(),
				0,                              // signature_ptr
				sig_len,                        // pub_key_ptr
				message_len,                    // message_len
				sig_len + pub_key.len() as u32, // message_ptr
			);
		}

		assert_eq!(result.unwrap(), ReturnErrorCode::Success);
	}

	#[benchmark(pov_mode = Measured)]
	fn ecdsa_recover() {
		use hex_literal::hex;
		let input = hex!("18c547e4f7b0f325ad1e56f57e26c745b09a3e503d86e00e5255ff7f715d3d1c000000000000000000000000000000000000000000000000000000000000001c73b1693892219d736caba55bdb67216e485557ea6b6af75f37096c9aa6a5a75feeb940b1d03b21e36b0e47e79769f095fe2ab855bd91e3a38756b7d75a9c4549").to_vec();
		let expected = hex!("000000000000000000000000a94f5374fce5edbc8e2a8697c15331677e6ebf0b");
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;

		#[block]
		{
			result =
				run_builtin_precompile(&mut ext, H160::from_low_u64_be(1).as_fixed_bytes(), input);
		}

		assert_eq!(result.unwrap().data, expected);
	}

	#[benchmark(pov_mode = Measured)]
	fn p256_verify() {
		use hex_literal::hex;
		let input = hex!("4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4da73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff37618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e").to_vec();
		let expected = U256::one().to_big_endian();
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;

		#[block]
		{
			result = run_builtin_precompile(
				&mut ext,
				H160::from_low_u64_be(0x100).as_fixed_bytes(),
				input,
			);
		}

		assert_eq!(result.unwrap().data, expected);
	}

	#[benchmark(pov_mode = Measured)]
	fn bn128_add() {
		use hex_literal::hex;
		let input = hex!("089142debb13c461f61523586a60732d8b69c5b38a3380a74da7b2961d867dbf2d5fc7bbc013c16d7945f190b232eacc25da675c0eb093fe6b9f1b4b4e107b3625f8c89ea3437f44f8fc8b6bfbb6312074dc6f983809a5e809ff4e1d076dd5850b38c7ced6e4daef9c4347f370d6d8b58f4b1d8dc61a3c59d651a0644a2a27cf").to_vec();
		let expected = hex!(
			"0a6678fd675aa4d8f0d03a1feb921a27f38ebdcb860cc083653519655acd6d79172fd5b3b2bfdd44e43bcec3eace9347608f9f0a16f1e184cb3f52e6f259cbeb"
		);
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result =
				run_builtin_precompile(&mut ext, H160::from_low_u64_be(6).as_fixed_bytes(), input);
		}

		assert_eq!(result.unwrap().data, expected);
	}

	#[benchmark(pov_mode = Measured)]
	fn bn128_mul() {
		use hex_literal::hex;
		let input = hex!("089142debb13c461f61523586a60732d8b69c5b38a3380a74da7b2961d867dbf2d5fc7bbc013c16d7945f190b232eacc25da675c0eb093fe6b9f1b4b4e107b36ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff").to_vec();
		let expected = hex!(
			"0bf982b98a2757878c051bfe7eee228b12bc69274b918f08d9fcb21e9184ddc10b17c77cbf3c19d5d27e18cbd4a8c336afb488d0e92c18d56e64dd4ea5c437e6"
		);
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result =
				run_builtin_precompile(&mut ext, H160::from_low_u64_be(7).as_fixed_bytes(), input);
		}

		assert_eq!(result.unwrap().data, expected);
	}

	// `n`: pairings to perform
	#[benchmark(pov_mode = Measured)]
	fn bn128_pairing(n: Linear<0, { 20 }>) {
		fn generate_random_ecpairs(n: usize) -> Vec<u8> {
			use bn::{AffineG1, AffineG2, Fr, G1, G2, Group};
			use rand::SeedableRng;
			use rand_pcg::Pcg64;
			let mut rng = Pcg64::seed_from_u64(1);

			let mut buffer = vec![0u8; n * 192];

			let mut write = |element: &bn::Fq, offset: &mut usize| {
				element.to_big_endian(&mut buffer[*offset..*offset + 32]).unwrap();
				*offset += 32
			};

			for i in 0..n {
				let mut offset = i * 192;
				let scalar = Fr::random(&mut rng);

				let g1 = G1::one() * scalar;
				let g2 = G2::one() * scalar;
				let a = AffineG1::from_jacobian(g1).expect("G1 point should be on curve");
				let b = AffineG2::from_jacobian(g2).expect("G2 point should be on curve");

				write(&a.x(), &mut offset);
				write(&a.y(), &mut offset);
				write(&b.x().imaginary(), &mut offset);
				write(&b.x().real(), &mut offset);
				write(&b.y().imaginary(), &mut offset);
				write(&b.y().real(), &mut offset);
			}

			buffer
		}

		let input = generate_random_ecpairs(n as usize);
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result =
				run_builtin_precompile(&mut ext, H160::from_low_u64_be(8).as_fixed_bytes(), input);
		}
		assert_ok!(result);
	}

	// `n`: number of rounds to perform
	#[benchmark(pov_mode = Measured)]
	fn blake2f(n: Linear<0, 1200>) {
		use hex_literal::hex;
		let input = hex!(
			"48c9bdf267e6096a3ba7ca8485ae67bb2bf894fe72f36e3cf1361d5f3af54fa5d182e6ad7f520e511f6c3e2b8c68059b6bbd41fbabd9831f79217e1319cde05b61626300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000300000000000000000000000000000001"
		);
		let input = n.to_be_bytes().to_vec().into_iter().chain(input.to_vec()).collect::<Vec<_>>();
		let mut call_setup = CallSetup::<T>::default();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result =
				run_builtin_precompile(&mut ext, H160::from_low_u64_be(9).as_fixed_bytes(), input);
		}
		assert_ok!(result);
	}

	// Only calling the function itself for the list of
	// generated different ECDSA keys.
	// This is a slow call: We reduce the number of runs.
	#[benchmark(pov_mode = Measured)]
	fn seal_ecdsa_to_eth_address() {
		let key_type = sp_core::crypto::KeyTypeId(*b"code");
		let pub_key_bytes = sp_io::crypto::ecdsa_generate(key_type, None).0;
		build_runtime!(runtime, memory: [[0u8; 20], pub_key_bytes,]);

		let result;
		#[block]
		{
			result = runtime.bench_ecdsa_to_eth_address(
				memory.as_mut_slice(),
				20, // key_ptr
				0,  // output_ptr
			);
		}

		assert_ok!(result);
		assert_eq!(&memory[..20], runtime.ext().ecdsa_to_eth_address(&pub_key_bytes).unwrap());
	}

	/// Benchmark the cost of executing `r` noop (JUMPDEST) instructions.
	#[benchmark(pov_mode = Measured)]
	fn evm_opcode(r: Linear<0, 10_000>) -> Result<(), BenchmarkError> {
		let module = VmBinaryModule::evm_noop(r);
		let inputs = vec![];

		let code = Bytecode::new_raw(revm::primitives::Bytes::from(module.code.clone()));
		let mut setup = CallSetup::<T>::new(module);
		let (mut ext, _) = setup.ext();

		let result;
		#[block]
		{
			result = evm::call(code, &mut ext, inputs);
		}

		assert!(result.is_ok());
		Ok(())
	}

	/// Benchmarks `r` EVM `POP` op-codes.
	#[benchmark(pov_mode = Measured)]
	fn evm_pop_opcode(r: Linear<0, EVM_STACK_LIMIT>) {
		let code = Bytecode::new_raw(vec![POP; r as usize].into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, vec![U256::MAX; r as usize]);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `DIV` op-codes.
	///
	/// # Added Overheads
	///
	/// * Cold reads, since the L1 and L2 caches are evicted before the benchmark runs.
	/// * A branch misprediction on how many times each quotient limb's estimate is corrected, since
	///   the divisor is picked at random from [`knuth_division_operands`].
	#[benchmark(pov_mode = Measured)]
	fn evm_div_opcode(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| unsigned_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([DIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `DIV` op-codes with pseudo-random one-limb correction patterns.
	#[benchmark(pov_mode = Measured)]
	fn evm_div_opcode_one_limb_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use one_limb_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| unsigned_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([DIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `DIV` op-codes with pseudo-random mixed one-limb and Knuth divisors.
	#[benchmark(pov_mode = Measured)]
	fn evm_div_opcode_mixed_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| mixed_division_operands::unsigned_operands(&mut rng));

		let code = Bytecode::new_raw([DIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `DIV` op-codes with the selected fixed Knuth divisor.
	#[benchmark(pov_mode = Measured)]
	fn evm_div_opcode_fixed_knuth_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::{DIV_FIXED_DIVISOR, NUMERATOR};

		let operands = (0..r).flat_map(|_| [DIV_FIXED_DIVISOR, NUMERATOR]);
		let code = Bytecode::new_raw([DIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SDIV` op-codes.
	///
	/// # Added Overheads
	///
	/// * Cold reads, since the L1 and L2 caches are evicted before the benchmark runs.
	/// * A carry through every limb to negate the numerator, since it's the most negative value.
	/// * No early exit from the check against the most negative value, since the numerator is that
	///   value.
	/// * A branch misprediction on how many times each quotient limb's estimate is corrected, since
	///   the divisor is picked at random from [`knuth_division_operands`].
	/// * A branch misprediction on whether the divisor and the quotient are negated, since the
	///   divisor's sign is pseudo-random.
	#[benchmark(pov_mode = Measured)]
	fn evm_sdiv_opcode(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| signed_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([SDIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SDIV` op-codes with pseudo-random one-limb correction patterns.
	#[benchmark(pov_mode = Measured)]
	fn evm_sdiv_opcode_one_limb_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use one_limb_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| signed_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([SDIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SDIV` op-codes with pseudo-random mixed one-limb and Knuth divisors.
	#[benchmark(pov_mode = Measured)]
	fn evm_sdiv_opcode_mixed_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| mixed_division_operands::signed_operands(&mut rng));

		let code = Bytecode::new_raw([SDIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SDIV` op-codes with the selected fixed Knuth divisor.
	#[benchmark(pov_mode = Measured)]
	fn evm_sdiv_opcode_fixed_knuth_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::{SDIV_FIXED_DIVISOR, fixed_signed_operands};

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| fixed_signed_operands(&mut rng, SDIV_FIXED_DIVISOR));

		let code = Bytecode::new_raw([SDIV, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `MOD` op-codes.
	///
	/// # Added Overheads
	///
	/// * Cold reads, since the L1 and L2 caches are evicted before the benchmark runs.
	/// * A branch misprediction on how many times each quotient limb's estimate is corrected, since
	///   the divisor is picked at random from [`knuth_division_operands`].
	#[benchmark(pov_mode = Measured)]
	fn evm_mod_opcode(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| unsigned_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([MOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `MOD` op-codes with pseudo-random one-limb correction patterns.
	#[benchmark(pov_mode = Measured)]
	fn evm_mod_opcode_one_limb_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use one_limb_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| unsigned_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([MOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `MOD` op-codes with pseudo-random mixed one-limb and Knuth divisors.
	#[benchmark(pov_mode = Measured)]
	fn evm_mod_opcode_mixed_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| mixed_division_operands::unsigned_operands(&mut rng));

		let code = Bytecode::new_raw([MOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `MOD` op-codes with the selected fixed Knuth divisor.
	#[benchmark(pov_mode = Measured)]
	fn evm_mod_opcode_fixed_knuth_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::{MOD_FIXED_DIVISOR, NUMERATOR};

		let operands = (0..r).flat_map(|_| [MOD_FIXED_DIVISOR, NUMERATOR]);
		let code = Bytecode::new_raw([MOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SMOD` op-codes.
	///
	/// # Added Overheads
	///
	/// * Cold reads, since the L1 and L2 caches are evicted before the benchmark runs.
	/// * A carry through every limb to negate the numerator, since it's the most negative value.
	/// * A branch misprediction on how many times each quotient limb's estimate is corrected, since
	///   the divisor is picked at random from [`knuth_division_operands`].
	/// * A branch misprediction on whether the divisor is negated, since its sign is pseudo-random.
	#[benchmark(pov_mode = Measured)]
	fn evm_smod_opcode(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| signed_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([SMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SMOD` op-codes with pseudo-random one-limb correction patterns.
	#[benchmark(pov_mode = Measured)]
	fn evm_smod_opcode_one_limb_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use one_limb_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| signed_division_operands(&mut rng, &DENOMINATORS));

		let code = Bytecode::new_raw([SMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SMOD` op-codes with pseudo-random mixed one-limb and Knuth divisors.
	#[benchmark(pov_mode = Measured)]
	fn evm_smod_opcode_mixed_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| mixed_division_operands::signed_operands(&mut rng));

		let code = Bytecode::new_raw([SMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `SMOD` op-codes with the selected fixed Knuth divisor.
	#[benchmark(pov_mode = Measured)]
	fn evm_smod_opcode_fixed_knuth_variant(r: Linear<0, { EVM_STACK_LIMIT / 2 }>) {
		use knuth_division_operands::{SMOD_FIXED_DIVISOR, fixed_signed_operands};

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| fixed_signed_operands(&mut rng, SMOD_FIXED_DIVISOR));

		let code = Bytecode::new_raw([SMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `ADDMOD` op-codes.
	///
	/// # Added Overheads
	///
	/// * Cold reads, since the L1 and L2 caches are evicted before the benchmark runs.
	/// * A division for each addend, since both are above the modulus.
	/// * A branch misprediction on how many times each quotient limb's estimate is corrected, since
	///   the modulus is picked at random from [`knuth_division_operands`].
	/// * A branch misprediction on whether the modulus is subtracted from the sum, since that
	///   depends on the modulus picked.
	#[benchmark(pov_mode = Measured)]
	fn evm_addmod_opcode(r: Linear<0, { EVM_STACK_LIMIT / 3 }>) {
		use knuth_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| {
			let [modulus, numerator] = unsigned_division_operands(&mut rng, &DENOMINATORS);
			[modulus, numerator, numerator]
		});

		let code = Bytecode::new_raw([ADDMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `ADDMOD` op-codes with pseudo-random one-limb correction patterns.
	#[benchmark(pov_mode = Measured)]
	fn evm_addmod_opcode_one_limb_variant(r: Linear<0, { EVM_STACK_LIMIT / 3 }>) {
		use one_limb_division_operands::DENOMINATORS;

		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| {
			let [modulus, numerator] = unsigned_division_operands(&mut rng, &DENOMINATORS);
			[modulus, numerator, numerator]
		});

		let code = Bytecode::new_raw([ADDMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `ADDMOD` op-codes with pseudo-random mixed one-limb and Knuth divisors.
	#[benchmark(pov_mode = Measured)]
	fn evm_addmod_opcode_mixed_variant(r: Linear<0, { EVM_STACK_LIMIT / 3 }>) {
		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| {
			let [modulus, numerator] = mixed_division_operands::unsigned_operands(&mut rng);
			[modulus, numerator, numerator]
		});

		let code = Bytecode::new_raw([ADDMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `ADDMOD` op-codes with the selected fixed Knuth divisor.
	#[benchmark(pov_mode = Measured)]
	fn evm_addmod_opcode_fixed_knuth_variant(r: Linear<0, { EVM_STACK_LIMIT / 3 }>) {
		use knuth_division_operands::{ADDMOD_FIXED_DIVISOR, NUMERATOR};

		let operands = (0..r).flat_map(|_| [ADDMOD_FIXED_DIVISOR, NUMERATOR, NUMERATOR]);
		let code = Bytecode::new_raw([ADDMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	/// Benchmarks `r` EVM `MULMOD` op-codes.
	///
	/// # Added Overheads
	///
	/// * Cold reads, since the L1 and L2 caches are evicted before the benchmark runs.
	/// * A product that fills every limb, since the top and lowest bits of both operands are set.
	/// * A multiply-subtract over all divisor limbs instead of all but the top two, since the top
	///   bits of the modulus are clear and division requires a nonzero normalization shift.
	/// * A branch misprediction on how many times the division loops, since the size of the modulus
	///   is pseudo-random.
	/// * A branch misprediction on each adjustment of the modulus's reciprocal, since whether it
	///   happens is pseudo-random.
	/// * A branch misprediction on the carry steps of the multiplication, since which limbs of one
	///   operand are zero is pseudo-random.
	#[benchmark(pov_mode = Measured)]
	fn evm_mulmod_opcode(r: Linear<0, { EVM_STACK_LIMIT / 3 }>) {
		let top_bit = U256::one() << 255;
		// The division computes the reciprocal of the top two limbs of the shifted modulus from the
		// top limb first, and then adjusts it twice for the second limb. These say whether each
		// adjustment happens.
		let reciprocal_adjustments = |modulus: U256| {
			let top = (modulus << modulus.leading_zeros() as usize) >> 128;
			let (high, low) = (top.low_u128() >> 64, u128::from(top.low_u64()));
			[low > u128::MAX % high, (U256::MAX >> 64) / top < U256::from((u128::MAX - low) / high)]
		};
		let mut rng = Pcg64::seed_from_u64(1337);
		let operands = (0..r).flat_map(|_| {
			let limbs = if rng.gen_bool(0.5) { 3 } else { 4 };
			let adjustments = [rng.gen_bool(0.5), rng.gen_bool(0.5)];
			let modulus = loop {
				let modulus = (U256(rng.r#gen()) | top_bit) >> (2 + 64 * (4 - limbs));
				if reciprocal_adjustments(modulus) == adjustments {
					break modulus;
				}
			};
			let a = U256(rng.r#gen()) | top_bit | U256::one();
			let mut b = U256(rng.r#gen()) | top_bit | U256::one();
			for limb in 1..=2 {
				if rng.gen_bool(0.5) {
					b.0[limb] = 0;
				}
			}
			[modulus, b, a]
		});

		let code = Bytecode::new_raw([MULMOD, POP].repeat(r as usize).into());
		let mut setup = CallSetup::<T>::new(VmBinaryModule::evm_noop(0));
		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(ExtBytecode::new(code), Vec::new(), &mut ext);
		setup_stack(&mut interpreter, operands);

		evict_caches();
		let result;
		#[block]
		{
			result = evm::run_plain(&mut interpreter);
		}

		let ControlFlow::Break(halt) = result;
		assert!(matches!(halt, Halt::Stop));
		assert_eq!(interpreter.stack.len(), 0);
	}

	// Benchmark the execution of instructions.
	//
	// It benchmarks the absolute worst case by allocating a lot of memory
	// and then accessing it so that each instruction generates two cache misses.
	#[benchmark(pov_mode = Ignored)]
	fn instr(r: Linear<0, 10_000>) {
		use rand::{SeedableRng, seq::SliceRandom};
		use rand_pcg::Pcg64;

		// Ideally, this needs to be bigger than the cache.
		const MEMORY_SIZE: u64 = sp_core::MAX_POSSIBLE_ALLOCATION as u64;

		// This is benchmarked for x86-64.
		const CACHE_LINE_SIZE: u64 = 64;

		// An 8 byte load from this misalignment will reach into the subsequent line.
		const MISALIGNMENT: u64 = 60;

		// We only need one address per cache line.
		// -1 because we skip the first address
		const NUM_ADDRESSES: u64 = (MEMORY_SIZE - MISALIGNMENT) / CACHE_LINE_SIZE - 1;

		assert!(
			u64::from(r) <= NUM_ADDRESSES / 2,
			"If we do too many iterations we run into the risk of loading from warm cache lines",
		);

		let mut setup = CallSetup::<T>::new(VmBinaryModule::instr(true));
		let (mut ext, module) = setup.ext();
		let mut prepared =
			CallSetup::<T>::prepare_call(&mut ext, module, Vec::new(), MEMORY_SIZE as u32);

		assert!(
			u64::from(prepared.aux_data_base()) & (CACHE_LINE_SIZE - 1) == 0,
			"aux data base must be cache aligned"
		);

		// Addresses data will be located inside the aux data.
		let misaligned_base = u64::from(prepared.aux_data_base()) + MISALIGNMENT;

		// Create all possible addresses and shuffle them. This makes sure
		// the accesses are random but no address is accessed more than once.
		// we skip the first address since it is our entry point
		let mut addresses = Vec::with_capacity(NUM_ADDRESSES as usize);
		for i in 1..NUM_ADDRESSES {
			let addr = (misaligned_base + i * CACHE_LINE_SIZE).to_le_bytes();
			addresses.push(addr);
		}
		let mut rng = Pcg64::seed_from_u64(1337);
		addresses.shuffle(&mut rng);

		// The addresses need to be padded to be one cache line apart.
		let mut memory = Vec::with_capacity((NUM_ADDRESSES * CACHE_LINE_SIZE) as usize);
		for address in addresses {
			memory.extend_from_slice(&address);
			memory.resize(memory.len() + CACHE_LINE_SIZE as usize - address.len(), 0);
		}

		// Copies `memory` to `aux_data_base + MISALIGNMENT`.
		// Sets `a0 = MISALIGNMENT` and `a1 = r`.
		prepared
			.setup_aux_data(memory.as_slice(), MISALIGNMENT as u32, r.into())
			.unwrap();

		#[block]
		{
			prepared.call().unwrap();
		}
	}

	#[benchmark(pov_mode = Ignored)]
	fn instr_empty_loop(r: Linear<0, 10_000>) {
		let mut setup = CallSetup::<T>::new(VmBinaryModule::instr(false));
		let (mut ext, module) = setup.ext();
		let mut prepared = CallSetup::<T>::prepare_call(&mut ext, module, Vec::new(), 0);
		prepared.setup_aux_data(&[], 0, r.into()).unwrap();

		#[block]
		{
			prepared.call().unwrap();
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn extcodecopy(n: Linear<1_000, { 100 * 1024 }>) -> Result<(), BenchmarkError> {
		// The caller contract; `CallSetup` whitelists its `AccountInfoOf`.
		let mut setup = CallSetup::<T>::new(VmBinaryModule::dummy());
		// Copy a contract other than the caller, so its `AccountInfoOf` read is counted.
		let target = Contract::<T>::with_index(1, VmBinaryModule::sized(n), vec![])?;

		let (mut ext, _) = setup.ext();
		let mut interpreter = Interpreter::new(Default::default(), Default::default(), &mut ext);

		// Setup stack for extcodecopy instruction: [address, dest_offset, offset, size]
		let _ = interpreter.stack.push(U256::from(n));
		let _ = interpreter.stack.push(U256::from(0u32));
		let _ = interpreter.stack.push(U256::from(0u32));
		let _ = interpreter.stack.push(target.address);

		let result;
		#[block]
		{
			result = instructions::host::extcodecopy(&mut interpreter);
		}

		assert!(result.is_continue());
		assert_eq!(
			*interpreter.memory.slice(0..n as usize),
			PristineCode::<T>::get(target.info()?.code_hash).unwrap()[0..n as usize],
			"Memory should contain the target contract's code after extcodecopy"
		);

		Ok(())
	}

	#[benchmark]
	fn v1_migration_step() {
		use crate::migrations::v1;
		let addr = H160::from([1u8; 20]);
		let contract_info = ContractInfo::new(&addr, 1u32.into(), Default::default()).unwrap();

		v1::old::ContractInfoOf::<T>::insert(addr, contract_info.clone());
		let mut meter = WeightMeter::new();
		assert_eq!(AccountInfo::<T>::load_contract(&addr), None);

		#[block]
		{
			v1::Migration::<T>::step(None, &mut meter).unwrap();
		}

		assert_eq!(v1::old::ContractInfoOf::<T>::get(&addr), None);
		assert_eq!(AccountInfo::<T>::load_contract(&addr).unwrap(), contract_info);

		// uses twice the weight once for migration and then for checking if there is another key.
		assert_eq!(meter.consumed(), <T as Config>::WeightInfo::v1_migration_step() * 2);
	}

	#[benchmark]
	fn v2_migration_step() {
		use crate::migrations::v2;
		let code_hash = H256::from([0; 32]);
		let old_code_info = v2::Migration::<T>::create_old_code_info(
			whitelisted_caller(),
			1000u32.into(),
			1,
			100,
			0,
		);
		v2::Migration::<T>::insert_old_code_info(code_hash, old_code_info.clone());
		let mut meter = WeightMeter::new();

		#[block]
		{
			v2::Migration::<T>::step(None, &mut meter).unwrap();
		}

		v2::Migration::<T>::assert_migrated_code_info(code_hash, &old_code_info);

		// uses twice the weight once for migration and then for checking if there is another key.
		assert_eq!(meter.consumed(), <T as Config>::WeightInfo::v2_migration_step() * 2);
	}

	#[benchmark]
	fn v3_migration_step() {
		use crate::migrations::v3;
		// Remove all pre-existing accounts
		let _ = frame_system::Account::<T>::clear(u32::MAX, None);

		let account = account::<T::AccountId>("target", 0, 0);
		T::Currency::mint_into(&account, Pallet::<T>::min_balance())
			.expect("should mint into account");

		// clear the mapping so the migration has work to do
		let addr = T::AddressMapper::to_address(&account);
		crate::OriginalAccount::<T>::remove(addr);

		assert!(!T::AddressMapper::is_mapped(&account));
		let mut meter = WeightMeter::new();

		#[block]
		{
			v3::Migration::<T>::step(None, &mut meter).unwrap();
		}

		assert!(T::AddressMapper::is_mapped(&account));

		// uses twice the weight: once for migration and then for checking if there is another key.
		assert_eq!(meter.consumed(), <T as Config>::WeightInfo::v3_migration_step() * 2);
	}

	/// One iteration of v4 phase 1: credit the uploader's [`NativeDepositOf`] bucket.
	///
	/// Seeds two codes and primes the cursor with the first stored entry so the benched
	/// iteration exercises the `iter_from` path that dominates phase 1 in production.
	#[benchmark]
	fn v4_code_upload_step() {
		use crate::migrations::v4;

		let _ = CodeInfoOf::<T>::clear(u32::MAX, None);

		let owner: T::AccountId = whitelisted_caller();
		let deposit: BalanceOf<T> = 1_000u32.into();

		let pallet_account = Pallet::<T>::account_id();
		T::Currency::mint_into(&pallet_account, Pallet::<T>::min_balance()).unwrap();
		T::Currency::mint_into(&pallet_account, deposit).unwrap();
		T::Currency::hold(&HoldReason::CodeUploadDepositReserve.into(), &pallet_account, deposit)
			.unwrap();

		CodeInfoOf::<T>::insert(
			H256::from([1u8; 32]),
			CodeInfo::<T>::new_with_deposit(owner.clone(), deposit),
		);
		CodeInfoOf::<T>::insert(
			H256::from([2u8; 32]),
			CodeInfo::<T>::new_with_deposit(owner.clone(), deposit),
		);

		let first = match v4::Migration::<T>::step_once(None) {
			Some(v4::Cursor::CodeUpload(h)) => h,
			other => panic!("expected CodeUpload cursor, got {other:?}"),
		};
		let cursor = Some(v4::Cursor::CodeUpload(first));

		#[block]
		{
			let _ = v4::Migration::<T>::step_once(cursor);
		}

		assert_eq!(
			NativeDepositOf::<T>::get(&pallet_account, &owner),
			deposit + deposit,
			"both code uploads credited to owner",
		);
	}

	/// One iteration of v4 phase 2: burn native hold, mint and hold PGAS for a single contract.
	///
	/// Seeds two contracts and primes the cursor with the first stored entry so the benched
	/// iteration exercises the `iter_from` path that dominates phase 2 in production.
	#[benchmark]
	fn v4_contract_step() {
		use crate::migrations::v4;

		let _ = AccountInfoOf::<T>::clear(u32::MAX, None);

		let code_hash = H256::from([0u8; 32]);
		let deposit: BalanceOf<T> = 1_000u32.into();

		for byte in [0x41u8, 0x42u8] {
			let addr = H160::from([byte; 20]);
			let contract_account = T::AddressMapper::to_account_id(&addr);
			let info =
				ContractInfo::<T>::new(&addr, 1u32.into(), code_hash).expect("fresh contract info");
			AccountInfoOf::<T>::insert(
				addr,
				crate::storage::AccountInfo::<T> {
					account_type: crate::storage::AccountType::Contract(info),
					dust: 0,
				},
			);
			T::Currency::mint_into(&contract_account, Pallet::<T>::min_balance()).unwrap();
			T::Currency::mint_into(&contract_account, deposit).unwrap();
			T::Currency::hold(
				&HoldReason::StorageDepositReserve.into(),
				&contract_account,
				deposit,
			)
			.unwrap();
		}

		let first = match v4::Migration::<T>::step_once(Some(v4::Cursor::Contract(None))) {
			Some(v4::Cursor::Contract(Some(addr))) => addr,
			other => panic!("expected Contract cursor, got {other:?}"),
		};
		let cursor = Some(v4::Cursor::Contract(Some(first)));

		#[block]
		{
			let _ = v4::Migration::<T>::step_once(cursor);
		}

		// `migrate_native_to_pgas` is a no-op for `Deposit = ()`, so the hold only clears on
		// PGAS-backed runtimes. On non-PGAS runtimes the benchmark still measures the iter cost.
		if T::Deposit::SUPPORTS_PGAS {
			for byte in [0x41u8, 0x42u8] {
				let addr = H160::from([byte; 20]);
				let contract_account = T::AddressMapper::to_account_id(&addr);
				assert_eq!(
					T::Currency::balance_on_hold(
						&HoldReason::StorageDepositReserve.into(),
						&contract_account,
					),
					0u32.into(),
					"native storage deposit burned for {addr:?}",
				);
			}
		}
	}

	/// One iteration of v4 phase 3: rewrite a legacy [`v4::old::DeletionQueue`] entry into the
	/// new [`DeletionQueue`] format.
	///
	/// Seeds two legacy entries and primes the cursor with the first stored entry so the benched
	/// iteration exercises the `iter_from` path.
	#[benchmark]
	fn v4_deletion_queue_step() {
		use crate::migrations::v4;

		let _ = v4::old::DeletionQueue::<T>::clear(u32::MAX, None);

		let trie_a: TrieId = vec![0xAAu8; 16].try_into().unwrap();
		let trie_b: TrieId = vec![0xBBu8; 24].try_into().unwrap();
		v4::old::DeletionQueue::<T>::insert(0u32, trie_a);
		v4::old::DeletionQueue::<T>::insert(1u32, trie_b);

		let first = match v4::Migration::<T>::step_once(Some(v4::Cursor::DeletionQueue(None))) {
			Some(v4::Cursor::DeletionQueue(Some(key))) => key,
			other => panic!("expected DeletionQueue cursor, got {other:?}"),
		};
		let cursor = Some(v4::Cursor::DeletionQueue(Some(first)));

		#[block]
		{
			let _ = v4::Migration::<T>::step_once(cursor);
		}

		assert!(
			DeletionQueue::<T>::get(0u32).is_some() && DeletionQueue::<T>::get(1u32).is_some(),
			"both legacy entries rewritten into the new format",
		);
	}

	/// Helper function to create a test signer for finalize_block benchmark
	fn create_test_signer<T: Config>() -> (T::AccountId, SigningKey, H160) {
		use hex_literal::hex;
		// dev::alith()
		let signer_account_id = hex!("f24FF3a9CF04c71Dbc94D0b566f7A27B94566cac");
		let signer_priv_key =
			hex!("5fb92d6e98884f76de468fa3f6278f8807c48bebc13595d45af5bdc4da702133");

		let signer_key = SigningKey::from_bytes(&signer_priv_key.into()).expect("valid key");

		let signer_address = H160::from_slice(&signer_account_id);
		let signer_caller = T::AddressMapper::to_fallback_account_id(&signer_address);

		(signer_caller, signer_key, signer_address)
	}

	/// Helper function to create and sign a transaction for finalize_block benchmark
	fn create_signed_transaction<T: Config>(
		signer_key: &SigningKey,
		target_address: H160,
		value: U256,
		input_data: Vec<u8>,
	) -> Vec<u8> {
		let unsigned_tx: TransactionUnsigned = TransactionLegacyUnsigned {
			to: Some(target_address),
			value,
			chain_id: Some(T::ChainId::get().into()),
			input: input_data.into(),
			..Default::default()
		}
		.into();

		let hashed_payload = sp_io::hashing::keccak_256(&unsigned_tx.unsigned_payload());
		let (signature, recovery_id) =
			signer_key.sign_prehash_recoverable(&hashed_payload).expect("signing success");

		let mut sig_bytes = [0u8; 65];
		sig_bytes[..64].copy_from_slice(&signature.to_bytes());
		sig_bytes[64] = recovery_id.to_byte();

		let signed_tx = unsigned_tx.with_signature(sig_bytes);

		signed_tx.signed_payload()
	}

	/// Helper function to generate common finalize_block benchmark setup
	fn setup_finalize_block_benchmark<T>()
	-> Result<(Contract<T>, BalanceOf<T>, U256, SigningKey, BlockNumberFor<T>), BenchmarkError>
	where
		BalanceOf<T>: Into<U256> + TryFrom<U256>,
		T: Config,
		MomentOf<T>: Into<U256>,
		<T as frame_system::Config>::Hash: frame_support::traits::IsType<H256>,
	{
		// Setup test signer
		let (signer_caller, signer_key, _signer_address) = create_test_signer::<T>();
		whitelist_account!(signer_caller);

		// Setup contract instance
		let instance =
			Contract::<T>::with_caller(signer_caller.clone(), VmBinaryModule::dummy(), vec![])?;
		let storage_deposit = default_deposit_limit::<T>();
		let value = Pallet::<T>::min_balance();
		let evm_value =
			Pallet::<T>::convert_native_to_evm(BalanceWithDust::new_unchecked::<T>(value, 0));

		// Setup block
		let current_block = BlockNumberFor::<T>::from(1u32);
		frame_system::Pallet::<T>::set_block_number(current_block);

		Ok((instance, storage_deposit, evm_value, signer_key, current_block))
	}

	/// Benchmark the `on_finalize` hook scaling with number of transactions.
	///
	/// This benchmark measures the marginal computational cost of adding transactions
	/// to a block during finalization, with fixed payload size to isolate transaction
	/// count scaling effects.
	///
	/// ## Parameters:
	/// - `n`: Number of transactions in the block (0-200)
	///
	/// ## Test Setup:
	/// - Creates `n` transactions with fixed 100-byte payloads
	/// - Pre-populates block builder storage with test data
	///
	/// ## Usage:
	/// Use this with `on_finalize_per_byte` to calculate total cost:
	/// `total_cost = base + (n × per_tx_cost) + (total_bytes × per_byte_cost)`
	#[benchmark(pov_mode = Measured)]
	fn on_finalize_per_transaction(n: Linear<0, 200>) -> Result<(), BenchmarkError> {
		let (instance, _storage_deposit, evm_value, signer_key, current_block) =
			setup_finalize_block_benchmark::<T>()?;

		// Fixed payload size to isolate transaction count effects
		let fixed_payload_size = 100usize;

		// Pre-populate InflightTransactions with n transactions of fixed size
		if n > 0 {
			// Initialize block
			let _ = Pallet::<T>::on_initialize(current_block);

			// Create input data of fixed size for consistent transaction payloads
			let input_data = vec![0x42u8; fixed_payload_size];
			let receipt_gas_info = ReceiptGasInfo {
				gas_used: U256::from(1_000_000),
				effective_gas_price: Pallet::<T>::evm_base_fee(),
			};

			for _ in 0..n {
				// Create real signed transaction with fixed-size input data
				let signed_transaction = create_signed_transaction::<T>(
					&signer_key,
					instance.address,
					evm_value,
					input_data.clone(),
				);

				// Store transaction
				let _ = block_storage::bench_with_ethereum_context(|| {
					let (encoded_logs, bloom) =
						block_storage::get_receipt_details().unwrap_or_default();

					let block_builder_ir = EthBlockBuilderIR::<T>::get();
					let mut block_builder = EthereumBlockBuilder::<T>::from_ir(block_builder_ir);

					block_builder.process_transaction(
						signed_transaction,
						true,
						receipt_gas_info.clone(),
						encoded_logs,
						bloom,
					);

					EthBlockBuilderIR::<T>::put(block_builder.to_ir());
				});
			}
		}

		#[block]
		{
			// Measure only the finalization cost with n transactions of fixed size
			let _ = Pallet::<T>::on_finalize(current_block);
		}

		// Verify transaction count
		assert_eq!(Pallet::<T>::eth_block().transactions.len(), n as usize);

		Ok(())
	}

	/// Benchmark the `on_finalize` hook scaling with transaction payload size.
	///
	/// This benchmark measures the marginal computational cost of processing
	/// larger transaction payloads during finalization, with fixed transaction count
	/// to isolate payload size scaling effects.
	///
	/// ## Parameters:
	/// - `d`: Payload size per transaction in bytes (0-1000)
	///
	/// ## Test Setup:
	/// - Creates 10 transactions with payload size `d`
	/// - Pre-populates block builder storage with test data
	///
	/// ## Usage:
	/// Use this with `on_finalize_per_transaction` to calculate total cost:
	/// `total_cost = base + (n × per_tx_cost) + (total_bytes × per_byte_cost)`
	#[benchmark(pov_mode = Measured)]
	fn on_finalize_per_transaction_data(d: Linear<0, 1000>) -> Result<(), BenchmarkError> {
		let (instance, _storage_deposit, evm_value, signer_key, current_block) =
			setup_finalize_block_benchmark::<T>()?;

		// Fixed transaction count to isolate payload size effects
		let fixed_tx_count = 10u32;

		// Initialize block
		let _ = Pallet::<T>::on_initialize(current_block);

		// Create input data of variable size p for realistic transaction payloads
		let input_data = vec![0x42u8; d as usize];
		let receipt_gas_info = ReceiptGasInfo {
			gas_used: U256::from(1_000_000),
			effective_gas_price: Pallet::<T>::evm_base_fee(),
		};

		for _ in 0..fixed_tx_count {
			// Create real signed transaction with variable-size input data
			let signed_transaction = create_signed_transaction::<T>(
				&signer_key,
				instance.address,
				evm_value,
				input_data.clone(),
			);

			// Store transaction
			let _ = block_storage::bench_with_ethereum_context(|| {
				let (encoded_logs, bloom) =
					block_storage::get_receipt_details().unwrap_or_default();

				let block_builder_ir = EthBlockBuilderIR::<T>::get();
				let mut block_builder = EthereumBlockBuilder::<T>::from_ir(block_builder_ir);

				block_builder.process_transaction(
					signed_transaction,
					true,
					receipt_gas_info.clone(),
					encoded_logs,
					bloom,
				);

				EthBlockBuilderIR::<T>::put(block_builder.to_ir());
			});
		}

		#[block]
		{
			// Measure only the finalization cost with fixed count, variable payload size
			let _ = Pallet::<T>::on_finalize(current_block);
		}

		// Verify transaction count
		assert_eq!(Pallet::<T>::eth_block().transactions.len(), fixed_tx_count as usize);

		Ok(())
	}

	/// Benchmark the `on_finalize` per-event costs.
	///
	/// This benchmark measures the computational cost of processing events
	/// within the finalization process, isolating the overhead of event count.
	/// Uses a single transaction with varying numbers of minimal events.
	///
	/// ## Parameters:
	/// - `e`: Number of events per transaction
	///
	/// ## Test Setup:
	/// - Creates 1 transaction with `e` ContractEmitted events
	/// - Each event contains minimal data (no topics, empty data field)
	///
	/// ## Usage:
	/// Measures the per-event processing overhead during finalization
	/// - Fixed cost: `on_finalize_per_event(0)` - baseline finalization cost
	/// - Per event: `on_finalize_per_event(e)` - linear scaling with event count
	#[benchmark(pov_mode = Measured)]
	fn on_finalize_per_event(e: Linear<0, 100>) -> Result<(), BenchmarkError> {
		let (instance, _storage_deposit, evm_value, signer_key, current_block) =
			setup_finalize_block_benchmark::<T>()?;

		// Create a single transaction with e events, each with minimal data
		let input_data = vec![0x42u8; 100];
		let signed_transaction = create_signed_transaction::<T>(
			&signer_key,
			instance.address,
			evm_value,
			input_data.clone(),
		);

		let receipt_gas_info = ReceiptGasInfo {
			gas_used: U256::from(1_000_000),
			effective_gas_price: Pallet::<T>::evm_base_fee(),
		};

		// Store transaction
		let _ = block_storage::bench_with_ethereum_context(|| {
			let (encoded_logs, bloom) = block_storage::get_receipt_details().unwrap_or_default();

			let block_builder_ir = EthBlockBuilderIR::<T>::get();
			let mut block_builder = EthereumBlockBuilder::<T>::from_ir(block_builder_ir);

			block_builder.process_transaction(
				signed_transaction,
				true,
				receipt_gas_info.clone(),
				encoded_logs,
				bloom,
			);

			EthBlockBuilderIR::<T>::put(block_builder.to_ir());
		});

		// Create e events with minimal data to isolate event count overhead
		for _ in 0..e {
			block_storage::capture_ethereum_log(&instance.address, &vec![], &vec![]);
		}

		#[block]
		{
			// Initialize block
			let _ = Pallet::<T>::on_initialize(current_block);

			// Measure the finalization cost with e events
			let _ = Pallet::<T>::on_finalize(current_block);
		}

		// Verify transaction count
		assert_eq!(Pallet::<T>::eth_block().transactions.len(), 1);

		Ok(())
	}

	/// ## Test Setup:
	/// - Creates 1 transaction with 1 ContractEmitted event
	/// - Event contains `d` total bytes of data across data field and topics
	///
	/// ## Usage:
	/// Measures the per-byte event data processing overhead during finalization
	/// - Fixed cost: `on_finalize_per_event_data(0)` - baseline cost with empty event
	/// - Per byte: `on_finalize_per_event_data(d)` - linear scaling with data size
	#[benchmark(pov_mode = Measured)]
	fn on_finalize_per_event_data(d: Linear<0, 16384>) -> Result<(), BenchmarkError> {
		let (instance, _storage_deposit, evm_value, signer_key, current_block) =
			setup_finalize_block_benchmark::<T>()?;

		// Create a single transaction with one event containing d bytes of data
		let input_data = vec![0x42u8; 100];
		let signed_transaction = create_signed_transaction::<T>(
			&signer_key,
			instance.address,
			evm_value,
			input_data.clone(),
		);

		let receipt_gas_info = ReceiptGasInfo {
			gas_used: U256::from(1_000_000),
			effective_gas_price: Pallet::<T>::evm_base_fee(),
		};

		// Store transaction
		let _ = block_storage::bench_with_ethereum_context(|| {
			let (encoded_logs, bloom) = block_storage::get_receipt_details().unwrap_or_default();

			let block_builder_ir = EthBlockBuilderIR::<T>::get();
			let mut block_builder = EthereumBlockBuilder::<T>::from_ir(block_builder_ir);

			block_builder.process_transaction(
				signed_transaction,
				true,
				receipt_gas_info,
				encoded_logs,
				bloom,
			);

			EthBlockBuilderIR::<T>::put(block_builder.to_ir());
		});

		// Create one event with d bytes of data distributed across topics and data field
		let (event_data, topics) = if d < 32 {
			// If total data is less than 32 bytes, put all in data field
			(vec![0x42u8; d as usize], vec![])
		} else {
			// Fill topics first, then put remaining bytes in data field
			let num_topics = core::cmp::min(limits::NUM_EVENT_TOPICS, d / 32);
			let topic_bytes_used = num_topics * 32;
			let data_bytes_remaining = d - topic_bytes_used;

			// Create topics filled with sequential data
			let mut topics = Vec::new();
			for topic_index in 0..num_topics {
				let topic_data = [topic_index as u8; 32];
				topics.push(H256::from(topic_data));
			}

			// Remaining bytes go to data field
			let event_data = vec![0x42u8; data_bytes_remaining as usize];

			(event_data, topics)
		};

		block_storage::capture_ethereum_log(&instance.address, &event_data, &topics);

		#[block]
		{
			// Initialize block
			let _ = Pallet::<T>::on_initialize(current_block);

			// Measure the finalization cost with d bytes of event data
			let _ = Pallet::<T>::on_finalize(current_block);
		}

		// Verify transaction count
		assert_eq!(Pallet::<T>::eth_block().transactions.len(), 1);

		Ok(())
	}

	impl_benchmark_test_suite!(
		Contracts,
		crate::tests::ExtBuilder::default().build(),
		crate::tests::Test,
	);
}
