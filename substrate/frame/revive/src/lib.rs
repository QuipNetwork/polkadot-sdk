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

#![doc = include_str!("../README.md")]
#![allow(rustdoc::private_intra_doc_links)]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(feature = "runtime-benchmarks", recursion_limit = "1024")]

extern crate alloc;

mod access_list;
mod address;
mod benchmarking;
#[cfg(any(feature = "runtime-benchmarks", test))]
pub mod call_builder;
mod debug;
mod deposit_payment;
mod exec;
mod impl_fungibles;
mod limits;
mod metering;
mod primitives;
#[doc(hidden)]
pub mod runtime_api;
#[doc(hidden)]
pub mod state_overrides;
mod storage;
#[cfg(test)]
mod tests;
mod transient_storage;
mod vm;
mod weightinfo_extension;

pub mod evm;
pub mod migrations;
pub mod mock;
pub mod precompiles;
pub mod test_utils;
pub mod tracing;
pub mod weights;

use crate::{
	access_list::{StorageAccessKind, Warmth},
	evm::{
		CallTracer, CreateCallMode, ExecutionTracer, GenericTransaction, PrestateTracer,
		StateOverrideSet, TYPE_EIP1559, Tracer, TracerType, block_hash::EthereumBlockBuilderIR,
		block_storage, fees::InfoT as FeeInfo, runtime::SetWeightLimit,
	},
	exec::{AccountIdOf, ExecError, Stack as ExecStack},
	sp_runtime::TransactionOutcome,
	storage::{AccountType, DeletionQueueManager},
	tracing::if_tracing,
	vm::{CodeInfo, RuntimeCosts, pvm::extract_code_and_data},
	weightinfo_extension::OnFinalizeBlockParts,
};
use alloc::{boxed::Box, format, vec};
use codec::{Codec, Decode, Encode};
use environmental::*;
use frame_support::{
	BoundedVec,
	dispatch::{
		DispatchErrorWithPostInfo, DispatchResult, DispatchResultWithPostInfo, GetDispatchInfo,
		Pays, PostDispatchInfo, RawOrigin,
	},
	ensure,
	pallet_prelude::DispatchClass,
	storage::with_transaction,
	traits::{
		ConstU32, ConstU64, DefensiveResult, EnsureOrigin, Get, IsSubType, IsType, OnUnbalanced,
		OriginTrait,
		fungible::{Balanced, Credit, Inspect, Mutate, MutateHold},
		tokens::Balance,
	},
	weights::WeightMeter,
};
use frame_system::{
	Pallet as System, ensure_signed,
	pallet_prelude::{BlockNumberFor, OriginFor},
};
use pallet_revive_types::runtime_api::*;
use scale_info::TypeInfo;
use sp_runtime::{
	AccountId32, DispatchError, FixedPointNumber, FixedU128, SaturatedConversion,
	traits::{
		BadOrigin, Bounded, Convert, Dispatchable, Saturating, UniqueSaturatedFrom,
		UniqueSaturatedInto, Zero,
	},
};

pub use crate::{
	address::{AccountId32Mapper, AddressMapper, AutoMapper, TestAccountMapper, create1, create2},
	debug::DebugSettings,
	deposit_payment::{Deposit, PGasDeposit},
	evm::{Address as EthAddress, Block as EthBlock, block_hash::ReceiptGasInfo},
	exec::{
		CallResources, DelegateInfo, Executable, Key, MomentOf, Origin as ExecOrigin,
		ReentrancyProtection,
	},
	limits::TRANSIENT_STORAGE_BYTES as TRANSIENT_STORAGE_LIMIT,
	metering::{
		EthTxInfo, FrameMeter, ResourceMeter, Token as WeightToken, TransactionLimits,
		TransactionMeter,
	},
	pallet::{genesis, *},
	storage::{AccountInfo, ContractInfo},
	transient_storage::{MeterEntry, StorageMeter as TransientStorageMeter, TransientStorage},
	vm::{BytecodeType, ContractBlob},
};
pub use codec;
use frame_support::traits::tokens::Precision;
pub use frame_support::{self, dispatch::DispatchInfo, traits::Time, weights::Weight};
pub use frame_system::{self, limits::BlockWeights};
pub use primitives::*;
pub use sp_core::{H160, H256, U256};
pub use sp_crypto_hashing::keccak_256;
pub use sp_runtime;
pub use weights::WeightInfo;

// Types re-export, needed to make it easier for runtimes to implement the pallet-revive runtime API
pub extern crate pallet_revive_types;

#[cfg(doc)]
pub use crate::vm::pvm::SyscallDoc;

pub type BalanceOf<T> = <T as Config>::Balance;
pub type CreditOf<T> = Credit<<T as frame_system::Config>::AccountId, <T as Config>::Currency>;
type TrieId = BoundedVec<u8, ConstU32<128>>;
type ImmutableData = BoundedVec<u8, ConstU32<{ limits::IMMUTABLE_BYTES }>>;
type CallOf<T> = <T as Config>::RuntimeCall;

/// Used as a sentinel value when reading and writing contract memory.
///
/// It is usually used to signal `None` to a contract when only a primitive is allowed
/// and we don't want to go through encoding a full Rust type. Using `u32::Max` is a safe
/// sentinel because contracts are never allowed to use such a large amount of resources
/// that this value makes sense for a memory location or length.
const SENTINEL: u32 = u32::MAX;

/// The target that is used for the log output emitted by this crate.
///
/// Hence you can use this target to selectively increase the log level for this crate.
///
/// Example: `RUST_LOG=runtime::revive=debug my_code --dev`
const LOG_TARGET: &str = "runtime::revive";

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::{pallet_prelude::*, traits::FindAuthor};
	use frame_system::pallet_prelude::*;
	use sp_core::U256;
	use sp_runtime::Perbill;

	/// The in-code storage version.
	pub(crate) const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config(with_default)]
	pub trait Config: frame_system::Config {
		/// The time implementation used to supply timestamps to contracts through `seal_now`.
		type Time: Time<Moment: Into<U256>>;

		/// The balance type of [`Self::Currency`].
		///
		/// Just added here to add additional trait bounds.
		#[pallet::no_default]
		type Balance: Balance
			+ TryFrom<U256>
			+ Into<U256>
			+ Bounded
			+ UniqueSaturatedInto<u64>
			+ UniqueSaturatedFrom<u64>
			+ UniqueSaturatedInto<u128>;

		/// The fungible in which fees are paid and contract balances are held.
		#[pallet::no_default]
		type Currency: Inspect<Self::AccountId, Balance = Self::Balance>
			+ Mutate<Self::AccountId>
			+ MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>
			+ Balanced<Self::AccountId>;

		/// Handler for burned native currency (e.g. gas rounding).
		///
		/// When EVM gas accounting rounds up the transaction cost, the small rounding
		/// difference is withdrawn from the caller and forwarded to this handler.
		/// Use this to redirect burned value to a treasury or DAP instead of silently
		/// destroying it.
		#[pallet::no_default_bounds]
		type OnBurn: OnUnbalanced<CreditOf<Self>>;

		/// The overarching event type.
		#[pallet::no_default_bounds]
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching call type.
		#[pallet::no_default_bounds]
		type RuntimeCall: Parameter
			+ Dispatchable<
				RuntimeOrigin = OriginFor<Self>,
				Info = DispatchInfo,
				PostInfo = PostDispatchInfo,
			> + IsType<<Self as frame_system::Config>::RuntimeCall>
			+ From<Call<Self>>
			+ IsSubType<Call<Self>>
			+ GetDispatchInfo;

		/// The overarching origin type.
		#[pallet::no_default_bounds]
		type RuntimeOrigin: IsType<OriginFor<Self>>
			+ From<Origin<Self>>
			+ Into<Result<Origin<Self>, OriginFor<Self>>>;

		/// Overarching hold reason.
		#[pallet::no_default_bounds]
		type RuntimeHoldReason: From<HoldReason>;

		/// Describes the weights of the dispatchables of this module and is also used to
		/// construct a default cost schedule.
		type WeightInfo: WeightInfo;

		/// Type that allows the runtime authors to add new host functions for a contract to call.
		///
		/// Pass in a tuple of types that implement [`precompiles::Precompile`].
		#[pallet::no_default_bounds]
		#[allow(private_bounds)]
		type Precompiles: precompiles::Precompiles<Self>;

		/// Find the author of the current block.
		type FindAuthor: FindAuthor<Self::AccountId>;

		/// The amount of balance a caller has to pay for each byte of storage.
		///
		/// # Note
		///
		/// It is safe to change this value on a live chain as all refunds are pro rata.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type DepositPerByte: Get<BalanceOf<Self>>;

		/// The amount of balance a caller has to pay for each storage item.
		///
		/// # Note
		///
		/// It is safe to change this value on a live chain as all refunds are pro rata.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type DepositPerItem: Get<BalanceOf<Self>>;

		/// The amount of balance a caller has to pay for each child trie storage item.
		///
		/// Those are the items created by a contract. In Solidity each value is a single
		/// storage item. This is why we need to set a lower value here than for the main
		/// trie items. Otherwise the storage deposit is too high.
		///
		/// # Note
		///
		/// It is safe to change this value on a live chain as all refunds are pro rata.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type DepositPerChildTrieItem: Get<BalanceOf<Self>>;

		/// The percentage of the storage deposit that should be held for using a code hash.
		/// Instantiating a contract, protects the code from being removed. In order to prevent
		/// abuse these actions are protected with a percentage of the code deposit.
		#[pallet::constant]
		type CodeHashLockupDepositPercent: Get<Perbill>;

		/// Use either valid type is [`address::AccountId32Mapper`] or [`address::H160Mapper`].
		#[pallet::no_default]
		type AddressMapper: AddressMapper<Self>;

		/// Allow EVM bytecode to be uploaded and instantiated.
		#[pallet::constant]
		type AllowEVMBytecode: Get<bool>;

		/// Origin allowed to upload code.
		///
		/// By default, it is safe to set this to `EnsureSigned`, allowing anyone to upload contract
		/// code.
		#[pallet::no_default_bounds]
		type UploadOrigin: EnsureOrigin<OriginFor<Self>, Success = Self::AccountId>;

		/// Origin allowed to instantiate code.
		///
		/// # Note
		///
		/// This is not enforced when a contract instantiates another contract. The
		/// [`Self::UploadOrigin`] should make sure that no code is deployed that does unwanted
		/// instantiations.
		///
		/// By default, it is safe to set this to `EnsureSigned`, allowing anyone to instantiate
		/// contract code.
		#[pallet::no_default_bounds]
		type InstantiateOrigin: EnsureOrigin<OriginFor<Self>, Success = Self::AccountId>;

		/// The amount of memory in bytes that parachain nodes a lot to the runtime.
		///
		/// This is used in [`Pallet::integrity_test`] to make sure that the runtime has enough
		/// memory to support this pallet if set to the correct value.
		type RuntimeMemory: Get<u32>;

		/// The amount of memory in bytes that relay chain validators a lot to the PoV.
		///
		/// This is used in [`Pallet::integrity_test`] to make sure that the runtime has enough
		/// memory to support this pallet if set to the correct value.
		///
		/// This value is usually higher than [`Self::RuntimeMemory`] to account for the fact
		/// that validators have to hold all storage items in PvF memory.
		type PVFMemory: Get<u32>;

		/// The [EIP-155](https://eips.ethereum.org/EIPS/eip-155) chain ID.
		///
		/// This is a unique identifier assigned to each blockchain network,
		/// preventing replay attacks.
		#[pallet::constant]
		type ChainId: Get<u64>;

		/// The ratio between the decimal representation of the native token and the ETH token.
		#[pallet::constant]
		type NativeToEthRatio: Get<u32>;

		/// Set to [`crate::evm::fees::Info`] for a production runtime.
		///
		/// For mock runtimes that do not need to interact with any eth compat functionality
		/// the default value of `()` will suffice.
		#[pallet::no_default_bounds]
		type FeeInfo: FeeInfo<Self>;

		/// Payment backend used to charge storage deposits.
		/// The default `()` binding always uses the native currency.
		#[pallet::no_default_bounds]
		type Deposit: Deposit<Self>;

		/// The fraction the maximum extrinsic weight `eth_transact` extrinsics are capped to.
		///
		/// This is not a security measure but a requirement due to how we map gas to `(Weight,
		/// StorageDeposit)`. The mapping might derive a `Weight` that is too large to fit into an
		/// extrinsic. In this case we cap it to the limit specified here.
		///
		/// `eth_transact` transactions that use more weight than specified will fail with an out of
		/// gas error during execution. Larger fractions will allow more transactions to run.
		/// Smaller values waste less block space: Choose as small as possible and as large as
		/// necessary.
		///
		///  Default: `0.5`.
		#[pallet::constant]
		type MaxEthExtrinsicWeight: Get<FixedU128>;

		/// Allows debug-mode configuration, such as enabling unlimited contract size.
		#[pallet::constant]
		type DebugEnabled: Get<bool>;

		/// When enabled, accounts are automatically mapped on creation and unmapped on
		/// kill via [`AutoMapper`]. This removes the need for explicit `map_account` calls.
		///
		/// Requires `frame_system::Config::OnNewAccount` and `OnKilledAccount` to be set
		/// to [`AutoMapper`]. When enabled, the `map_account` and `unmap_account`
		/// dispatchables are disabled.
		#[pallet::constant]
		type AutoMap: Get<bool>;

		/// This determines the relative scale of our gas price and gas estimates.
		///
		/// By default, the gas price (in wei) is `FeeInfo::next_fee_multiplier()` multiplied by
		/// `NativeToEthRatio`. `GasScale` allows to scale this value: the actual gas price is the
		/// default gas price multiplied by `GasScale`.
		///
		/// As a consequence, gas cost (gas estimates and actual gas usage during transaction) is
		/// scaled down by the same factor. Thus, the total transaction cost is not affected by
		/// `GasScale` – apart from rounding differences: the transaction cost is always a multiple
		/// of the gas price and is derived by rounded up, so that with higher `GasScales` this can
		/// lead to higher gas cost as the rounding difference would be larger.
		///
		/// The main purpose of changing the `GasScale` is to tune the gas cost so that it is closer
		/// to standard EVM gas cost and contracts will not run out of gas when tools or code
		/// assume hard coded gas limits.
		///
		/// Requirement: `GasScale` must not be 0
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type GasScale: Get<u32>;
	}

	/// Container for different types that implement [`DefaultConfig`]` of this pallet.
	pub mod config_preludes {
		use super::*;
		use frame_support::{
			derive_impl,
			traits::{ConstBool, ConstU32},
		};
		use frame_system::EnsureSigned;
		use sp_core::parameter_types;

		type Balance = u64;

		pub const DOLLARS: Balance = 1_000_000_000_000;
		pub const CENTS: Balance = DOLLARS / 100;
		pub const MILLICENTS: Balance = CENTS / 1_000;

		pub const fn deposit(items: u32, bytes: u32) -> Balance {
			items as Balance * 20 * CENTS + (bytes as Balance) * MILLICENTS
		}

		parameter_types! {
			pub const DepositPerItem: Balance = deposit(1, 0);
			pub const DepositPerChildTrieItem: Balance = deposit(1, 0) / 100;
			pub const DepositPerByte: Balance = deposit(0, 1);
			pub const CodeHashLockupDepositPercent: Perbill = Perbill::from_percent(0);
			pub const MaxEthExtrinsicWeight: FixedU128 = FixedU128::from_rational(9, 10);
			pub const GasScale: u32 = 10u32;
		}

		/// A type providing default configurations for this pallet in testing environment.
		pub struct TestDefaultConfig;

		impl Time for TestDefaultConfig {
			type Moment = u64;
			fn now() -> Self::Moment {
				0u64
			}
		}

		impl<T: From<u64>> Convert<Weight, T> for TestDefaultConfig {
			fn convert(w: Weight) -> T {
				w.ref_time().into()
			}
		}

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeEvent = ();

			#[inject_runtime_type]
			type RuntimeHoldReason = ();

			#[inject_runtime_type]
			type RuntimeCall = ();

			#[inject_runtime_type]
			type RuntimeOrigin = ();

			type Precompiles = ();
			type CodeHashLockupDepositPercent = CodeHashLockupDepositPercent;
			type DepositPerByte = DepositPerByte;
			type DepositPerItem = DepositPerItem;
			type DepositPerChildTrieItem = DepositPerChildTrieItem;
			type Time = Self;
			type AllowEVMBytecode = ConstBool<true>;
			type UploadOrigin = EnsureSigned<Self::AccountId>;
			type InstantiateOrigin = EnsureSigned<Self::AccountId>;
			type WeightInfo = ();
			type RuntimeMemory = ConstU32<{ 128 * 1024 * 1024 }>;
			type PVFMemory = ConstU32<{ 512 * 1024 * 1024 }>;
			type ChainId = ConstU64<42>;
			type NativeToEthRatio = ConstU32<1_000_000>;
			type FindAuthor = ();
			type FeeInfo = ();
			type Deposit = ();
			type MaxEthExtrinsicWeight = MaxEthExtrinsicWeight;
			type DebugEnabled = ConstBool<false>;
			type AutoMap = ConstBool<false>;
			type GasScale = GasScale;
			type OnBurn = ();
		}
	}

	#[pallet::event]
	pub enum Event<T: Config> {
		/// A custom event emitted by the contract.
		ContractEmitted {
			/// The contract that emitted the event.
			contract: H160,
			/// Data supplied by the contract. Metadata generated during contract compilation
			/// is needed to decode it.
			data: Vec<u8>,
			/// A list of topics used to index the event.
			/// Number of topics is capped by [`limits::NUM_EVENT_TOPICS`].
			topics: Vec<H256>,
		},

		/// Contract deployed by deployer at the specified address.
		Instantiated { deployer: H160, contract: H160 },

		/// Emitted when an Ethereum transaction reverts.
		///
		/// Ethereum transactions always complete successfully at the extrinsic level,
		/// as even reverted calls must store their `ReceiptInfo`.
		/// To distinguish reverted calls from successful ones, this event is emitted
		/// for failed Ethereum transactions.
		EthExtrinsicRevert { dispatch_error: DispatchError },
	}

	#[pallet::error]
	#[repr(u8)]
	pub enum Error<T> {
		/// Invalid schedule supplied, e.g. with zero weight of a basic operation.
		InvalidSchedule = 0x01,
		/// Invalid combination of flags supplied to `seal_call` or `seal_delegate_call`.
		InvalidCallFlags = 0x02,
		/// The executed contract exhausted its gas limit.
		OutOfGas = 0x03,
		/// Performing the requested transfer failed. Probably because there isn't enough
		/// free balance in the sender's account.
		TransferFailed = 0x04,
		/// Performing a call was denied because the calling depth reached the limit
		/// of what is specified in the schedule.
		MaxCallDepthReached = 0x05,
		/// No contract was found at the specified address.
		ContractNotFound = 0x06,
		/// No code could be found at the supplied code hash.
		CodeNotFound = 0x07,
		/// No code info could be found at the supplied code hash.
		CodeInfoNotFound = 0x08,
		/// A buffer outside of sandbox memory was passed to a contract API function.
		OutOfBounds = 0x09,
		/// Input passed to a contract API function failed to decode as expected type.
		DecodingFailed = 0x0A,
		/// Contract trapped during execution.
		ContractTrapped = 0x0B,
		/// Event body or storage item exceeds [`limits::STORAGE_BYTES`].
		ValueTooLarge = 0x0C,
		/// Termination of a contract is not allowed while the contract is already
		/// on the call stack. Can be triggered by `seal_terminate`.
		TerminatedWhileReentrant = 0x0D,
		/// `seal_call` forwarded this contracts input. It therefore is no longer available.
		InputForwarded = 0x0E,
		/// The amount of topics passed to `seal_deposit_events` exceeds the limit.
		TooManyTopics = 0x0F,
		/// A contract with the same AccountId already exists.
		DuplicateContract = 0x12,
		/// A contract self destructed in its constructor.
		///
		/// This can be triggered by a call to `seal_terminate`.
		TerminatedInConstructor = 0x13,
		/// A call tried to invoke a contract that is flagged as non-reentrant.
		ReentranceDenied = 0x14,
		/// A contract called into the runtime which then called back into this pallet.
		ReenteredPallet = 0x15,
		/// A contract attempted to invoke a state modifying API while being in read-only mode.
		StateChangeDenied = 0x16,
		/// Origin doesn't have enough balance to pay the required storage deposits.
		StorageDepositNotEnoughFunds = 0x17,
		/// More storage was created than allowed by the storage deposit limit.
		StorageDepositLimitExhausted = 0x18,
		/// Code removal was denied because the code is still in use by at least one contract.
		CodeInUse = 0x19,
		/// The contract ran to completion but decided to revert its storage changes.
		/// Please note that this error is only returned from extrinsics. When called directly
		/// or via RPC an `Ok` will be returned. In this case the caller needs to inspect the flags
		/// to determine whether a reversion has taken place.
		ContractReverted = 0x1A,
		/// The contract failed to compile or is missing the correct entry points.
		///
		/// A more detailed error can be found on the node console if debug messages are enabled
		/// by supplying `-lruntime::revive=debug`.
		CodeRejected = 0x1B,
		/// The code blob supplied is larger than [`limits::code::BLOB_BYTES`].
		BlobTooLarge = 0x1C,
		/// The contract declares too much memory (ro + rw + stack).
		StaticMemoryTooLarge = 0x1D,
		/// The program contains a basic block that is larger than allowed.
		BasicBlockTooLarge = 0x1E,
		/// The program contains an invalid instruction.
		InvalidInstruction = 0x1F,
		/// The contract has reached its maximum number of delegate dependencies.
		MaxDelegateDependenciesReached = 0x20,
		/// The dependency was not found in the contract's delegate dependencies.
		DelegateDependencyNotFound = 0x21,
		/// The contract already depends on the given delegate dependency.
		DelegateDependencyAlreadyExists = 0x22,
		/// Can not add a delegate dependency to the code hash of the contract itself.
		CannotAddSelfAsDelegateDependency = 0x23,
		/// Can not add more data to transient storage.
		OutOfTransientStorage = 0x24,
		/// The contract tried to call a syscall which does not exist (at its current api level).
		InvalidSyscall = 0x25,
		/// Invalid storage flags were passed to one of the storage syscalls.
		InvalidStorageFlags = 0x26,
		/// PolkaVM failed during code execution. Probably due to a malformed program.
		ExecutionFailed = 0x27,
		/// Failed to convert a U256 to a Balance.
		BalanceConversionFailed = 0x28,
		/// Immutable data can only be set during deploys and only be read during calls.
		/// Additionally, it is only valid to set the data once and it must not be empty.
		InvalidImmutableAccess = 0x2A,
		/// An `AccountID32` account tried to interact with the pallet without having a mapping.
		///
		/// Call [`Pallet::map_account`] in order to create a mapping for the account.
		AccountUnmapped = 0x2B,
		/// Tried to map an account that is already mapped.
		AccountAlreadyMapped = 0x2C,
		/// The transaction used to dry-run a contract is invalid.
		InvalidGenericTransaction = 0x2D,
		/// The refcount of a code either over or underflowed.
		RefcountOverOrUnderflow = 0x2E,
		/// Unsupported precompile address.
		UnsupportedPrecompileAddress = 0x2F,
		/// The calldata exceeds [`limits::CALLDATA_BYTES`].
		CallDataTooLarge = 0x30,
		/// The return data exceeds [`limits::CALLDATA_BYTES`].
		ReturnDataTooLarge = 0x31,
		/// Invalid jump destination. Dynamic jumps points to invalid not jumpdest opcode.
		InvalidJump = 0x32,
		/// Attempting to pop a value from an empty stack.
		StackUnderflow = 0x33,
		/// Attempting to push a value onto a full stack.
		StackOverflow = 0x34,
		/// Too much deposit was drawn from the shared txfee and deposit credit.
		///
		/// This happens if the passed `gas` inside the ethereum transaction is too low.
		TxFeeOverdraw = 0x35,
		/// When calling an EVM constructor `data` has to be empty.
		///
		/// EVM constructors do not accept data. Their input data is part of the code blob itself.
		EvmConstructorNonEmptyData = 0x36,
		/// Tried to construct an EVM contract via code hash.
		///
		/// EVM contracts can only be instantiated via code upload as no initcode is
		/// stored on-chain.
		EvmConstructedFromHash = 0x37,
		/// The contract does not have enough balance to refund the storage deposit.
		///
		/// This is a bug and should never happen. It means the accounting got out of sync.
		StorageRefundNotEnoughFunds = 0x38,
		/// This means there are locks on the contracts storage deposit that prevents refunding it.
		///
		/// This would be the case if the contract used its storage deposits for governance
		/// or other pallets that allow creating locks over held balance.
		StorageRefundLocked = 0x39,
		/// Called a pre-compile that is not allowed to be delegate called.
		///
		/// Some pre-compile functions will trap the caller context if being delegate
		/// called or if their caller was being delegate called.
		PrecompileDelegateDenied = 0x40,
		/// ECDSA public key recovery failed. Most probably wrong recovery id or signature.
		EcdsaRecoveryFailed = 0x41,
		/// Manual mapping is disabled when auto-mapping is enabled.
		AutoMappingEnabled = 0x42,
		/// A contract cannot be created at this address: it still has uncleared
		/// [`NativeDepositOf`] entries from a previously terminated contract that the deletion
		/// queue has not yet drained.
		PendingDepositCleanup = 0x43,
		/// Benchmarking only error.
		#[cfg(feature = "runtime-benchmarks")]
		BenchmarkingError = 0xFF,
	}

	/// A reason for the pallet revive placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// The Pallet has reserved it for storing code on-chain.
		CodeUploadDepositReserve,
		/// The Pallet has reserved it for storage deposit.
		StorageDepositReserve,
		/// Deposit for creating an address mapping in [`OriginalAccount`].
		AddressMapping,
	}

	/// A reason for the pallet revive placing a freeze on PGAS funds.
	#[pallet::composite_enum]
	pub enum FreezeReason {
		/// Pins the PGAS existential deposit minted into a contract account so it cannot be
		/// transferred or burned by the contract while it is alive. Without this freeze, a
		/// contract could call the PGAS ERC20 precompile with `Preservation::Expendable` and
		/// drain its own ED.
		PGasMinBalance,
	}

	#[derive(
		PartialEq, Eq, Clone, MaxEncodedLen, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug,
	)]
	#[pallet::origin]
	pub enum Origin<T: Config> {
		EthTransaction(T::AccountId),
	}

	/// A mapping from a contract's code hash to its code.
	/// The code's size is bounded by [`crate::limits::BLOB_BYTES`] for PVM and
	/// [`revm::primitives::eip170::MAX_CODE_SIZE`] for EVM bytecode.
	#[pallet::storage]
	#[pallet::unbounded]
	pub(crate) type PristineCode<T: Config> = StorageMap<_, Identity, H256, Vec<u8>>;

	/// A mapping from a contract's code hash to its code info.
	#[pallet::storage]
	pub(crate) type CodeInfoOf<T: Config> = StorageMap<_, Identity, H256, CodeInfo<T>>;

	/// The data associated to a contract or externally owned account.
	#[pallet::storage]
	pub(crate) type AccountInfoOf<T: Config> = StorageMap<_, Identity, H160, AccountInfo<T>>;

	/// Native currency storage deposit contributed by a user into a contract.
	///
	/// Bounds how much native value the user can receive back from that contract's
	/// storage deposit.
	///
	/// Keys: `(holder, contributor) -> amount`
	/// - `holder`: account on which the deposit is held (a contract, or the pallet's own account
	///   for code-upload deposits).
	/// - `contributor`: user that funded the deposit. Receives the native portion on refund, capped
	///   at this entry's `amount`.
	#[pallet::storage]
	pub(crate) type NativeDepositOf<T: Config> = StorageDoubleMap<
		_,
		Identity,
		T::AccountId,
		Identity,
		T::AccountId,
		BalanceOf<T>,
		ValueQuery,
	>;

	/// The immutable data associated with a given account.
	#[pallet::storage]
	pub(crate) type ImmutableDataOf<T: Config> = StorageMap<_, Identity, H160, ImmutableData>;

	/// Terminated contracts that await lazy cleanup.
	///
	/// Each entry pairs a child trie ID with the contract account so that `on_idle` can
	/// drain both the child trie and any [`NativeDepositOf`] entries that named the contract
	/// as `holder`. Both can be arbitrarily large, so cleanup runs lazily in `on_idle`.
	#[pallet::storage]
	pub(crate) type DeletionQueue<T: Config> =
		StorageMap<_, Twox64Concat, u32, crate::storage::DeletionQueueItem<T>>;

	/// A pair of monotonic counters used to track the latest contract marked for deletion
	/// and the latest deleted contract in queue.
	#[pallet::storage]
	pub(crate) type DeletionQueueCounter<T: Config> =
		StorageValue<_, DeletionQueueManager<T>, ValueQuery>;

	/// Map a Ethereum address to its original `AccountId32`.
	///
	/// When deriving a `H160` from an `AccountId32` we use a hash function. In order to
	/// reconstruct the original account we need to store the reverse mapping here.
	/// Register your `AccountId32` using [`Pallet::map_account`] in order to
	/// use it with this pallet.
	#[pallet::storage]
	pub(crate) type OriginalAccount<T: Config> = StorageMap<_, Identity, H160, AccountId32>;

	/// The current Ethereum block that is stored in the `on_finalize` method.
	///
	/// # Note
	///
	/// This could be further optimized into the future to store only the minimum
	/// information needed to reconstruct the Ethereum block at the RPC level.
	///
	/// Since the block is convenient to have around, and the extra details are capped
	/// by a few hashes and the vector of transaction hashes, we store the block here.
	#[pallet::storage]
	#[pallet::unbounded]
	pub(crate) type EthereumBlock<T> = StorageValue<_, EthBlock, ValueQuery>;

	/// Mapping for block number and hashes.
	///
	/// The maximum number of elements stored is capped by the block hash count `BLOCK_HASH_COUNT`.
	#[pallet::storage]
	pub(crate) type BlockHash<T: Config> =
		StorageMap<_, Identity, BlockNumberFor<T>, H256, ValueQuery>;

	/// The details needed to reconstruct the receipt info offchain.
	///
	/// This contains valuable information about the gas used by the transaction.
	///
	/// NOTE: The item is unbound and should therefore never be read on chain.
	/// It could otherwise inflate the PoV size of a block.
	#[pallet::storage]
	#[pallet::unbounded]
	pub(crate) type ReceiptInfoData<T: Config> = StorageValue<_, Vec<ReceiptGasInfo>, ValueQuery>;

	/// Incremental ethereum block builder.
	#[pallet::storage]
	#[pallet::unbounded]
	pub(crate) type EthBlockBuilderIR<T: Config> =
		StorageValue<_, EthereumBlockBuilderIR<T>, ValueQuery>;

	/// The first transaction and receipt of the ethereum block.
	///
	/// These values are moved out of the `EthBlockBuilderIR` to avoid serializing and
	/// deserializing them on every transaction. Instead, they are loaded when needed.
	#[pallet::storage]
	#[pallet::unbounded]
	pub(crate) type EthBlockBuilderFirstValues<T: Config> =
		StorageValue<_, Option<(Vec<u8>, Vec<u8>)>, ValueQuery>;

	/// Debugging settings that can be configured when DebugEnabled config is true.
	#[pallet::storage]
	pub(crate) type DebugSettingsOf<T: Config> = StorageValue<_, DebugSettings, ValueQuery>;

	pub mod genesis {
		use super::*;
		use crate::evm::Bytes32;

		/// Genesis configuration for contract-specific data.
		#[derive(Clone, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
		pub struct ContractData {
			/// Contract code.
			pub code: crate::evm::Bytes,
			/// Initial storage entries as 32-byte key/value pairs.
			pub storage: alloc::collections::BTreeMap<Bytes32, Bytes32>,
		}

		/// Genesis configuration for a contract account.
		#[derive(PartialEq, Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
		pub struct Account<T: Config> {
			/// Contract address.
			pub address: H160,
			/// Contract balance.
			#[serde(default)]
			pub balance: U256,
			/// Account nonce
			#[serde(default)]
			pub nonce: T::Nonce,
			/// Contract-specific data (code and storage). None for EOAs.
			#[serde(flatten, skip_serializing_if = "Option::is_none")]
			pub contract_data: Option<ContractData>,
		}
	}

	#[pallet::genesis_config]
	#[derive(Debug, PartialEq, frame_support::DefaultNoBound)]
	pub struct GenesisConfig<T: Config> {
		/// List of native Substrate accounts (typically `AccountId32`) to be mapped at genesis
		/// block, enabling them to interact with smart contracts.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		pub mapped_accounts: Vec<T::AccountId>,

		/// Account entries (both EOAs and contracts)
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		pub accounts: Vec<genesis::Account<T>>,

		/// Optional debugging settings applied at genesis.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		pub debug_settings: Option<DebugSettings>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			use crate::{exec::Key, vm::ContractBlob};
			use frame_support::traits::fungible::Mutate;

			if !System::<T>::account_exists(&Pallet::<T>::account_id()) {
				let _ = T::Currency::mint_into(
					&Pallet::<T>::account_id(),
					T::Currency::minimum_balance(),
				);
			}

			for id in &self.mapped_accounts {
				if let Err(err) = T::AddressMapper::map_no_deposit_unchecked(id) {
					log::error!(target: LOG_TARGET, "Failed to map account {id:?}: {err:?}");
				}
			}

			let owner = Pallet::<T>::account_id();

			for genesis::Account { address, balance, nonce, contract_data } in &self.accounts {
				let account_id = T::AddressMapper::to_account_id(address);

				if !System::<T>::account_exists(&account_id) {
					let _ = T::Currency::mint_into(&account_id, T::Currency::minimum_balance());
				}

				frame_system::Account::<T>::mutate(&account_id, |info| {
					info.nonce = (*nonce).into();
				});

				match contract_data {
					None => {
						AccountInfoOf::<T>::insert(
							address,
							AccountInfo { account_type: AccountType::EOA, dust: 0 },
						);
					},
					Some(genesis::ContractData { code, storage }) => {
						let blob = if code.0.starts_with(&polkavm_common::program::BLOB_MAGIC) {
							ContractBlob::<T>::from_pvm_code(code.0.clone(), owner.clone())
								.inspect_err(|err| {
									log::error!(target: LOG_TARGET, "Failed to create PVM ContractBlob for {address:?}: {err:?}");
								})
						} else {
							ContractBlob::<T>::from_evm_runtime_code(code.0.clone(), account_id)
								.inspect_err(|err| {
									log::error!(target: LOG_TARGET, "Failed to create EVM ContractBlob for {address:?}: {err:?}");
								})
						};

						let Ok(blob) = blob else {
							continue;
						};

						let code_hash = *blob.code_hash();
						let Ok(info) = <ContractInfo<T>>::new(&address, 0u32.into(), code_hash)
							.inspect_err(|err| {
								log::error!(target: LOG_TARGET, "Failed to create ContractInfo for {address:?}: {err:?}");
							})
						else {
							continue;
						};

						AccountInfoOf::<T>::insert(
							address,
							AccountInfo { account_type: info.clone().into(), dust: 0 },
						);

						<PristineCode<T>>::insert(blob.code_hash(), code.0.clone());
						<CodeInfoOf<T>>::insert(blob.code_hash(), blob.code_info().clone());
						for (k, v) in storage {
							let _ = info.write(&Key::from_fixed(k.0), Some(v.0.to_vec()), None, false).inspect_err(|err| {
								log::error!(target: LOG_TARGET, "Failed to write genesis storage for {address:?} at key {k:?}: {err:?}");
							});
						}
					},
				}

				let _ = Pallet::<T>::set_evm_balance(address, *balance).inspect_err(|err| {
					log::error!(target: LOG_TARGET, "Failed to set EVM balance for {address:?}: {err:?}");
				});
			}

			// Build genesis block
			block_storage::on_finalize_build_eth_block::<T>(
				// Make sure to use the block number from storage instead of the hardcoded 0.
				// This enables testing tools like anvil to customise the genesis block number.
				frame_system::Pallet::<T>::block_number(),
			);

			// Set debug settings.
			if let Some(settings) = self.debug_settings.as_ref() {
				settings.write_to_storage::<T>()
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_idle(_block: BlockNumberFor<T>, limit: Weight) -> Weight {
			let mut meter = WeightMeter::with_limit(limit);
			ContractInfo::<T>::process_deletion_queue_batch(&mut meter);
			meter.consumed()
		}

		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			// Kill related ethereum block storage items.
			block_storage::on_initialize::<T>();

			// Warm up the pallet account.
			System::<T>::account_exists(&Pallet::<T>::account_id());
			// Account for the fixed part of the costs incurred in `on_finalize`.
			<T as Config>::WeightInfo::on_finalize_block_fixed()
		}

		fn on_finalize(block_number: BlockNumberFor<T>) {
			// Build the ethereum block and place it in storage.
			block_storage::on_finalize_build_eth_block::<T>(block_number);
		}

		fn integrity_test() {
			assert!(T::ChainId::get() > 0, "ChainId must be greater than 0");

			assert!(T::GasScale::get() > 0u32.into(), "GasScale must not be 0");

			T::FeeInfo::integrity_test();

			// The memory available in the block building runtime
			let max_runtime_mem: u64 = T::RuntimeMemory::get().into();

			// We only allow 50% of the runtime memory to be utilized by the contracts call
			// stack, keeping the rest for other facilities, such as PoV, etc.
			const TOTAL_MEMORY_DEVIDER: u64 = 2;

			// Validators are configured to be able to use more memory than block builders. This is
			// because in addition to `max_runtime_mem` they need to hold additional data in
			// memory: PoV in multiple copies (1x encoded + 2x decoded) and all storage which
			// includes emitted events. The assumption is that storage/events size
			// can be a maximum of half of the validator runtime memory - max_runtime_mem.
			let max_block_weight = T::BlockWeights::get()
				.get(DispatchClass::Normal)
				.max_total
				.unwrap_or_else(|| T::BlockWeights::get().max_block);
			let max_key_size: u64 =
				Key::try_from_var(alloc::vec![0u8; limits::STORAGE_KEY_BYTES as usize])
					.expect("Key of maximal size shall be created")
					.hash()
					.len()
					.try_into()
					.unwrap();

			let max_immutable_key_size: u64 = T::AccountId::max_encoded_len().try_into().unwrap();
			let max_immutable_size: u64 = max_block_weight
				.checked_div_per_component(&<RuntimeCosts as WeightToken<T>>::weight(
					&RuntimeCosts::SetImmutableData(limits::IMMUTABLE_BYTES),
				))
				.unwrap()
				.saturating_mul(
					u64::from(limits::IMMUTABLE_BYTES)
						.saturating_add(max_immutable_key_size)
						.into(),
				);

			let max_pvf_mem: u64 = T::PVFMemory::get().into();
			let storage_size_limit = max_pvf_mem.saturating_sub(max_runtime_mem) / 2;

			// We can use storage to store events using the available block ref_time with the
			// `deposit_event` host function. The overhead of stored events, which is around 100B,
			// is not taken into account to simplify calculations, as it does not change much.
			let max_events_size = max_block_weight
				.checked_div_per_component(
					&(<RuntimeCosts as WeightToken<T>>::weight(&RuntimeCosts::DepositEvent {
						num_topic: 0,
						len: limits::EVENT_BYTES,
					})
					.saturating_add(<RuntimeCosts as WeightToken<T>>::weight(
						&RuntimeCosts::HostFn,
					))),
				)
				.unwrap()
				.saturating_mul(limits::EVENT_BYTES.into());

			assert!(
				max_events_size <= storage_size_limit,
				"Maximal events size {} exceeds the events limit {}",
				max_events_size,
				storage_size_limit
			);

			// The incremental block builder uses 3 x maximum entry size for receipts and
			// for transactions. Transactions are bounded to `MAX_TRANSACTION_PAYLOAD_SIZE`.
			//
			// To determine the maximum size of the receipts, we know the following:
			// - (I) first receipt is stored into pallet storage and not given to the hasher until
			//   finalization.
			// - (II) the hasher will not consume more memory than the receipts we are giving it.
			// - (III) the hasher is capped by 3 x maximum entry for 3 or more transactions.
			//
			// # Case 1. One transaction with maximum receipts
			//
			// The worst case scenario for having one single transaction is for the transaction
			// to emit the maximum receipt size (ie `max_events_size`). In this case,
			// the maximum storage (and memory) consumed is bounded by `max_events_size` (II). The
			// receipt is stored in pallet storage, and loaded from storage in the
			// `on_finalize` hook (I).
			//
			// # Case 2. Two transactions
			//
			// The sum of the receipt size of both transactions cannot exceed `max_events_size`,
			// otherwise one transaction will be reverted. From (II), the bytes utilized
			// by the builder are capped to `max_events_size`.
			//
			// # Case 3. Three or more transactions
			//
			// Similar to the above case, the sum of all receipt size is bounded to
			// `max_events_size`. Therefore, the bytes are capped to `max_events_size`.
			//
			// On average, a transaction could emit `max_events_size / num_tx`. The would
			// consume `max_events_size / num_tx * 3` bytes, which is lower than
			// `max_events_size` for more than 3 transactions.
			//
			// In practice, the builder will consume even lower amounts considering
			// it is unlikely for a transaction to utilize all the weight of the block for events.
			let max_eth_block_builder_bytes =
				block_storage::block_builder_bytes_usage(max_events_size.try_into().unwrap());

			log::debug!(
				target: LOG_TARGET,
				"Integrity check: max_eth_block_builder_bytes={} KB using max_events_size={} KB",
				max_eth_block_builder_bytes / 1024,
				max_events_size / 1024,
			);

			// Check that the configured memory limits fit into runtime memory.
			//
			// Dynamic allocations are not available, yet. Hence they are not taken into
			// consideration here.
			let memory_left = i128::from(max_runtime_mem)
				.saturating_div(TOTAL_MEMORY_DEVIDER.into())
				.saturating_sub(limits::MEMORY_REQUIRED.into())
				.saturating_sub(max_eth_block_builder_bytes.into());

			log::debug!(target: LOG_TARGET, "Integrity check: memory_left={} KB", memory_left / 1024);

			assert!(
				memory_left >= 0,
				"Runtime does not have enough memory for current limits. Additional runtime memory required: {} KB",
				memory_left.saturating_mul(TOTAL_MEMORY_DEVIDER.into()).abs() / 1024
			);

			// We can use storage to store items using the available block ref_time with the
			// `set_storage` host function. A revertible cold access is the worst case.
			let max_storage_size = max_block_weight
				.checked_div_per_component(
					&<RuntimeCosts as WeightToken<T>>::weight(&RuntimeCosts::SetStorage {
						new_bytes: limits::STORAGE_BYTES,
						old_bytes: 0,
						kind: StorageAccessKind::Persistent(Warmth::Cold { revertible: true }),
					})
					.saturating_mul(u64::from(limits::STORAGE_BYTES).saturating_add(max_key_size)),
				)
				.unwrap()
				.saturating_add(max_immutable_size.into())
				.saturating_add(max_eth_block_builder_bytes.into());

			assert!(
				max_storage_size <= storage_size_limit,
				"Maximal storage size {} exceeds the storage limit {}",
				max_storage_size,
				storage_size_limit
			);
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// A raw EVM transaction, typically dispatched by an Ethereum JSON-RPC server.
		///
		/// # Parameters
		///
		/// * `payload`: The encoded [`crate::evm::TransactionSigned`].
		///
		/// # Note
		///
		/// This call cannot be dispatched directly; attempting to do so will result in a failed
		/// transaction. It serves as a wrapper for an Ethereum transaction. When submitted, the
		/// runtime converts it into a [`sp_runtime::generic::CheckedExtrinsic`] by recovering the
		/// signer and validating the transaction.
		#[allow(unused_variables)]
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::MAX)]
		pub fn eth_transact(origin: OriginFor<T>, payload: Vec<u8>) -> DispatchResultWithPostInfo {
			Err(frame_system::Error::CallFiltered::<T>.into())
		}

		/// Makes a call to an account, optionally transferring some balance.
		///
		/// # Parameters
		///
		/// * `dest`: Address of the contract to call.
		/// * `value`: The balance to transfer from the `origin` to `dest`.
		/// * `weight_limit`: The weight limit enforced when executing the constructor.
		/// * `storage_deposit_limit`: The maximum amount of balance that can be charged from the
		///   caller to pay for the storage consumed.
		/// * `data`: The input data to pass to the contract.
		///
		/// * If the account is a smart-contract account, the associated code will be
		/// executed and any value will be transferred.
		/// * If the account is a regular account, any value will be transferred.
		/// * If no account exists and the call value is not less than `existential_deposit`,
		/// a regular account will be created and any value will be transferred.
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config>::WeightInfo::call().saturating_add(*weight_limit))]
		pub fn call(
			origin: OriginFor<T>,
			dest: H160,
			#[pallet::compact] value: BalanceOf<T>,
			weight_limit: Weight,
			#[pallet::compact] storage_deposit_limit: BalanceOf<T>,
			data: Vec<u8>,
		) -> DispatchResultWithPostInfo {
			Self::ensure_non_contract_if_signed(&origin)?;
			let mut output = Self::bare_call(
				origin,
				dest,
				Pallet::<T>::convert_native_to_evm(value),
				TransactionLimits::WeightAndDeposit {
					weight_limit,
					deposit_limit: storage_deposit_limit,
				},
				data,
				&ExecConfig::new_substrate_tx(),
			);

			if let Ok(return_value) = &output.result &&
				return_value.did_revert()
			{
				output.result = Err(<Error<T>>::ContractReverted.into());
			}
			dispatch_result(
				output.result,
				output.weight_consumed,
				<T as Config>::WeightInfo::call(),
			)
		}

		/// Instantiates a contract from a previously deployed vm binary.
		///
		/// This function is identical to [`Self::instantiate_with_code`] but without the
		/// code deployment step. Instead, the `code_hash` of an on-chain deployed vm binary
		/// must be supplied.
		#[pallet::call_index(2)]
		#[pallet::weight(
			<T as Config>::WeightInfo::instantiate(data.len() as u32).saturating_add(*weight_limit)
		)]
		pub fn instantiate(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T>,
			weight_limit: Weight,
			#[pallet::compact] storage_deposit_limit: BalanceOf<T>,
			code_hash: sp_core::H256,
			data: Vec<u8>,
			salt: Option<[u8; 32]>,
		) -> DispatchResultWithPostInfo {
			Self::ensure_non_contract_if_signed(&origin)?;
			let data_len = data.len() as u32;
			let mut output = Self::bare_instantiate(
				origin,
				Pallet::<T>::convert_native_to_evm(value),
				TransactionLimits::WeightAndDeposit {
					weight_limit,
					deposit_limit: storage_deposit_limit,
				},
				Code::Existing(code_hash),
				data,
				salt,
				&ExecConfig::new_substrate_tx(),
			);
			if let Ok(retval) = &output.result &&
				retval.result.did_revert()
			{
				output.result = Err(<Error<T>>::ContractReverted.into());
			}
			dispatch_result(
				output.result.map(|result| result.result),
				output.weight_consumed,
				<T as Config>::WeightInfo::instantiate(data_len),
			)
		}

		/// Instantiates a new contract from the supplied `code` optionally transferring
		/// some balance.
		///
		/// This dispatchable has the same effect as calling [`Self::upload_code`] +
		/// [`Self::instantiate`]. Bundling them together provides efficiency gains. Please
		/// also check the documentation of [`Self::upload_code`].
		///
		/// # Parameters
		///
		/// * `value`: The balance to transfer from the `origin` to the newly created contract.
		/// * `weight_limit`: The weight limit enforced when executing the constructor.
		/// * `storage_deposit_limit`: The maximum amount of balance that can be charged/reserved
		///   from the caller to pay for the storage consumed.
		/// * `code`: The contract code to deploy in raw bytes.
		/// * `data`: The input data to pass to the contract constructor.
		/// * `salt`: Used for the address derivation. If `Some` is supplied then `CREATE2`
		/// 	semantics are used. If `None` then `CRATE1` is used.
		///
		///
		/// Instantiation is executed as follows:
		///
		/// - The supplied `code` is deployed, and a `code_hash` is created for that code.
		/// - If the `code_hash` already exists on the chain the underlying `code` will be shared.
		/// - The destination address is computed based on the sender, code_hash and the salt.
		/// - The smart-contract account is created at the computed address.
		/// - The `value` is transferred to the new account.
		/// - The `deploy` function is executed in the context of the newly-created account.
		#[pallet::call_index(3)]
		#[pallet::weight(
			<T as Config>::WeightInfo::instantiate_with_code(code.len() as u32, data.len() as u32)
			.saturating_add(*weight_limit)
		)]
		pub fn instantiate_with_code(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T>,
			weight_limit: Weight,
			#[pallet::compact] storage_deposit_limit: BalanceOf<T>,
			code: Vec<u8>,
			data: Vec<u8>,
			salt: Option<[u8; 32]>,
		) -> DispatchResultWithPostInfo {
			Self::ensure_non_contract_if_signed(&origin)?;
			let code_len = code.len() as u32;
			let data_len = data.len() as u32;
			let mut output = Self::bare_instantiate(
				origin,
				Pallet::<T>::convert_native_to_evm(value),
				TransactionLimits::WeightAndDeposit {
					weight_limit,
					deposit_limit: storage_deposit_limit,
				},
				Code::Upload(code),
				data,
				salt,
				&ExecConfig::new_substrate_tx(),
			);
			if let Ok(retval) = &output.result &&
				retval.result.did_revert()
			{
				output.result = Err(<Error<T>>::ContractReverted.into());
			}
			dispatch_result(
				output.result.map(|result| result.result),
				output.weight_consumed,
				<T as Config>::WeightInfo::instantiate_with_code(code_len, data_len),
			)
		}

		/// Same as [`Self::instantiate_with_code`], but intended to be dispatched **only**
		/// by an EVM transaction through the EVM compatibility layer.
		///
		/// # Parameters
		///
		/// * `value`: The balance to transfer from the `origin` to the newly created contract.
		/// * `weight_limit`: The gas limit used to derive the transaction weight for transaction
		///   payment
		/// * `eth_gas_limit`: The Ethereum gas limit governing the resource usage of the execution
		/// * `code`: The contract code to deploy in raw bytes.
		/// * `data`: The input data to pass to the contract constructor.
		/// * `transaction_encoded`: The RLP encoding of the signed Ethereum transaction,
		///   represented as [crate::evm::TransactionSigned], provided by the Ethereum wallet. This
		///   is used for building the Ethereum transaction root.
		/// * effective_gas_price: the price of a unit of gas
		/// * encoded len: the byte code size of the `eth_transact` extrinsic
		///
		/// Calling this dispatchable ensures that the origin's nonce is bumped only once,
		/// via the `CheckNonce` transaction extension. In contrast, [`Self::instantiate_with_code`]
		/// also bumps the nonce after contract instantiation, since it may be invoked multiple
		/// times within a batch call transaction.
		#[pallet::call_index(10)]
		#[pallet::weight(
			<T as Config>::WeightInfo::eth_instantiate_with_code(code.len() as u32, data.len() as u32, Pallet::<T>::has_dust(*value).into())
			.saturating_add(*weight_limit)
			.saturating_add(T::WeightInfo::on_finalize_block_per_tx(transaction_encoded.len() as u32))
		)]
		pub fn eth_instantiate_with_code(
			origin: OriginFor<T>,
			value: U256,
			weight_limit: Weight,
			eth_gas_limit: U256,
			code: Vec<u8>,
			data: Vec<u8>,
			transaction_encoded: Vec<u8>,
			effective_gas_price: U256,
			encoded_len: u32,
		) -> DispatchResultWithPostInfo {
			let signer = Self::ensure_eth_signed(origin)?;
			let origin = OriginFor::<T>::signed(signer.clone());
			Self::ensure_non_contract_if_signed(&origin)?;
			let mut call = Call::<T>::eth_instantiate_with_code {
				value,
				weight_limit,
				eth_gas_limit,
				code: code.clone(),
				data: data.clone(),
				transaction_encoded: transaction_encoded.clone(),
				effective_gas_price,
				encoded_len,
			}
			.into();
			let info = T::FeeInfo::dispatch_info(&call);
			let base_info = T::FeeInfo::base_dispatch_info(&mut call);
			drop(call);

			block_storage::with_ethereum_context::<T>(transaction_encoded, || {
				let extra_weight = base_info.total_weight();
				let output = Self::bare_instantiate(
					origin,
					value,
					TransactionLimits::EthereumGas {
						eth_gas_limit: eth_gas_limit.saturated_into(),
						weight_limit,
						eth_tx_info: EthTxInfo::new(encoded_len, extra_weight),
					},
					Code::Upload(code),
					data,
					None,
					&ExecConfig::new_eth_tx(effective_gas_price, encoded_len, extra_weight),
				);

				block_storage::EthereumCallResult::new::<T>(
					signer,
					output.map_result(|r| r.result),
					base_info.call_weight,
					encoded_len,
					&info,
					effective_gas_price,
				)
			})
		}

		/// Same as [`Self::call`], but intended to be dispatched **only**
		/// by an EVM transaction through the EVM compatibility layer.
		///
		/// # Parameters
		///
		/// * `dest`: The Ethereum address of the account to be called
		/// * `value`: The balance to transfer from the `origin` to the newly created contract.
		/// * `weight_limit`: The gas limit used to derive the transaction weight for transaction
		///   payment
		/// * `eth_gas_limit`: The Ethereum gas limit governing the resource usage of the execution
		/// * `data`: The input data to pass to the contract constructor.
		/// * `transaction_encoded`: The RLP encoding of the signed Ethereum transaction,
		///   represented as [crate::evm::TransactionSigned], provided by the Ethereum wallet. This
		///   is used for building the Ethereum transaction root.
		/// * effective_gas_price: the price of a unit of gas
		/// * encoded len: the byte code size of the `eth_transact` extrinsic
		#[pallet::call_index(11)]
		#[pallet::weight(
			T::WeightInfo::eth_call(Pallet::<T>::has_dust(*value).into())
			.saturating_add(*weight_limit)
			.saturating_add(T::WeightInfo::on_finalize_block_per_tx(transaction_encoded.len() as u32))
		)]
		pub fn eth_call(
			origin: OriginFor<T>,
			dest: H160,
			value: U256,
			weight_limit: Weight,
			eth_gas_limit: U256,
			data: Vec<u8>,
			transaction_encoded: Vec<u8>,
			effective_gas_price: U256,
			encoded_len: u32,
		) -> DispatchResultWithPostInfo {
			let signer = Self::ensure_eth_signed(origin)?;
			let origin = OriginFor::<T>::signed(signer.clone());

			Self::ensure_non_contract_if_signed(&origin)?;
			let mut call = Call::<T>::eth_call {
				dest,
				value,
				weight_limit,
				eth_gas_limit,
				data: data.clone(),
				transaction_encoded: transaction_encoded.clone(),
				effective_gas_price,
				encoded_len,
			}
			.into();
			let info = T::FeeInfo::dispatch_info(&call);
			let base_info = T::FeeInfo::base_dispatch_info(&mut call);
			drop(call);

			block_storage::with_ethereum_context::<T>(transaction_encoded, || {
				let extra_weight = base_info.total_weight();
				let output = Self::bare_call(
					origin,
					dest,
					value,
					TransactionLimits::EthereumGas {
						eth_gas_limit: eth_gas_limit.saturated_into(),
						weight_limit,
						eth_tx_info: EthTxInfo::new(encoded_len, extra_weight),
					},
					data,
					&ExecConfig::new_eth_tx(effective_gas_price, encoded_len, extra_weight),
				);

				block_storage::EthereumCallResult::new::<T>(
					signer,
					output,
					base_info.call_weight,
					encoded_len,
					&info,
					effective_gas_price,
				)
			})
		}

		/// Executes a Substrate runtime call from an Ethereum transaction.
		///
		/// This dispatchable is intended to be called **only** through the EVM compatibility
		/// layer. The provided call will be dispatched using `RawOrigin::Signed`.
		///
		/// # Parameters
		///
		/// * `origin`: Must be an [`Origin::EthTransaction`] origin.
		/// * `call`: The Substrate runtime call to execute.
		/// * `transaction_encoded`: The RLP encoding of the Ethereum transaction,
		#[pallet::call_index(12)]
		#[pallet::weight(
			T::WeightInfo::eth_substrate_call(transaction_encoded.len() as u32)
			.saturating_add(call.get_dispatch_info().call_weight)
			.saturating_add(T::WeightInfo::on_finalize_block_per_tx(transaction_encoded.len() as u32))
		)]
		pub fn eth_substrate_call(
			origin: OriginFor<T>,
			call: Box<<T as Config>::RuntimeCall>,
			transaction_encoded: Vec<u8>,
		) -> DispatchResultWithPostInfo {
			// Note that the inner dispatch uses `RawOrigin::Signed`, which cannot
			// re-enter `eth_substrate_call` (which requires `Origin::EthTransaction`).
			let signer = Self::ensure_eth_signed(origin)?;
			Self::ensure_non_contract_if_signed(&OriginFor::<T>::signed(signer.clone()))?;
			let tx_len = transaction_encoded.len() as u32;
			let weight_overhead = T::WeightInfo::eth_substrate_call(tx_len)
				.saturating_add(T::WeightInfo::on_finalize_block_per_tx(tx_len));

			block_storage::with_ethereum_context::<T>(transaction_encoded, || {
				let call_weight = call.get_dispatch_info().call_weight;
				let mut call_result = call.dispatch(RawOrigin::Signed(signer).into());

				// Add extrinsic_overhead to the actual weight in PostDispatchInfo
				match &mut call_result {
					Ok(post_info) | Err(DispatchErrorWithPostInfo { post_info, .. }) => {
						post_info.actual_weight = Some(
							post_info
								.actual_weight
								.unwrap_or_else(|| call_weight)
								.saturating_add(weight_overhead),
						);
					},
				}

				// Return zero EVM gas (Substrate dispatch, not EVM contract call).
				// Actual weight is in `post_info.actual_weight`.
				block_storage::EthereumCallResult {
					receipt_gas_info: ReceiptGasInfo::default(),
					result: call_result,
				}
			})
		}

		/// Upload new `code` without instantiating a contract from it.
		///
		/// If the code does not already exist a deposit is reserved from the caller
		/// The size of the reserve depends on the size of the supplied `code`.
		///
		/// # Note
		///
		/// Anyone can instantiate a contract from any uploaded code and thus prevent its removal.
		/// To avoid this situation a constructor could employ access control so that it can
		/// only be instantiated by permissioned entities. The same is true when uploading
		/// through [`Self::instantiate_with_code`].
		///
		/// If the refcount of the code reaches zero after terminating the last contract that
		/// references this code, the code will be removed automatically.
		#[pallet::call_index(4)]
		#[pallet::weight(<T as Config>::WeightInfo::upload_code(code.len() as u32))]
		pub fn upload_code(
			origin: OriginFor<T>,
			code: Vec<u8>,
			#[pallet::compact] storage_deposit_limit: BalanceOf<T>,
		) -> DispatchResult {
			Self::ensure_non_contract_if_signed(&origin)?;
			Self::bare_upload_code(origin, code, storage_deposit_limit).map(|_| ())
		}

		/// Remove the code stored under `code_hash` and refund the deposit to its owner.
		///
		/// A code can only be removed by its original uploader (its owner) and only if it is
		/// not used by any contract.
		#[pallet::call_index(5)]
		#[pallet::weight(<T as Config>::WeightInfo::remove_code())]
		pub fn remove_code(
			origin: OriginFor<T>,
			code_hash: sp_core::H256,
		) -> DispatchResultWithPostInfo {
			let origin = ensure_signed(origin)?;
			<ContractBlob<T>>::remove(&origin, code_hash)?;
			// we waive the fee because removing unused code is beneficial
			Ok(Pays::No.into())
		}

		/// Privileged function that changes the code of an existing contract.
		///
		/// This takes care of updating refcounts and all other necessary operations. Returns
		/// an error if either the `code_hash` or `dest` do not exist.
		///
		/// # Note
		///
		/// This does **not** change the address of the contract in question. This means
		/// that the contract address is no longer derived from its code hash after calling
		/// this dispatchable.
		#[pallet::call_index(6)]
		#[pallet::weight(<T as Config>::WeightInfo::set_code())]
		pub fn set_code(
			origin: OriginFor<T>,
			dest: H160,
			code_hash: sp_core::H256,
		) -> DispatchResult {
			ensure_root(origin)?;
			<AccountInfoOf<T>>::try_mutate(&dest, |account| {
				let Some(account) = account else {
					return Err(<Error<T>>::ContractNotFound.into());
				};

				let AccountType::Contract(ref mut contract) = account.account_type else {
					return Err(<Error<T>>::ContractNotFound.into());
				};

				<CodeInfo<T>>::increment_refcount(code_hash)?;
				let _ = <CodeInfo<T>>::decrement_refcount(contract.code_hash)?;
				contract.code_hash = code_hash;

				Ok(())
			})
		}

		/// Register the callers account id so that it can be used in contract interactions.
		///
		/// This will error if the origin is already mapped or is a eth native `Address20`. It will
		/// take a deposit that can be released by calling [`Self::unmap_account`].
		///
		/// Noop when [`Config::AutoMap`] is enabled, as accounts are automatically mapped
		/// on creation via [`AutoMapper`].
		#[pallet::call_index(7)]
		#[pallet::weight(<T as Config>::WeightInfo::map_account())]
		pub fn map_account(origin: OriginFor<T>) -> DispatchResult {
			#[cfg(not(feature = "runtime-benchmarks"))]
			if T::AutoMap::get() {
				return Ok(());
			}

			Self::ensure_non_contract_if_signed(&origin)?;
			let origin = ensure_signed(origin)?;
			T::AddressMapper::map(&origin)
		}

		/// Map many accounts and make the TX free if at least 90% were unmapped or held deposits.
		#[pallet::call_index(13)]
		#[pallet::weight(<T as Config>::WeightInfo::batch_map_accounts(accounts.len().saturated_into::<u32>()))]
		pub fn batch_map_accounts(
			origin: OriginFor<T>,
			accounts: Vec<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin.clone())?;
			Self::ensure_non_contract_if_signed(&origin)?;

			let total: u32 = accounts.len().saturated_into();
			let mut mapped = 0;

			for account_id in accounts
				.iter()
				// Eth-derived accounts are stateless mapped, nothing to do.
				.filter(|&a| !T::AddressMapper::is_eth_derived(a))
				// Skip non-existent accounts: otherwise any caller could permanently
				// insert mappings for arbitrary AccountIds at no cost.
				.filter(|&a| frame_system::Pallet::<T>::account_exists(a))
			{
				let mut useful = false;

				match T::AddressMapper::map_no_deposit_unchecked(account_id) {
					Ok(()) => {
						useful = true;
					},
					Err(err) => log::debug!(
						target: LOG_TARGET,
						"Failed to map account {account_id:?}: {err:?}",
					),
				}

				match T::Currency::release_all(
					&HoldReason::AddressMapping.into(),
					account_id,
					Precision::BestEffort,
				) {
					// `release_all` returns `Ok(0)` when there is no hold to release,
					// which is not useful work and must not earn a fee refund.
					Ok(released) if !released.is_zero() => {
						useful = true;
					},
					Ok(_) => {},
					Err(err) => log::debug!(
						target: LOG_TARGET,
						"Failed to release mapping deposit for {account_id:?}: {err:?}",
					),
				}

				if useful {
					mapped = mapped.saturating_add(1);
				}
			}

			// guard against 0 division below
			if total == 0 || mapped == 0 {
				return Ok(Pays::Yes.into());
			}

			let proportion_mapped = Perbill::from_rational(mapped, total);
			if proportion_mapped >= Perbill::from_percent(90) {
				Ok(Pays::No.into())
			} else {
				Ok(Pays::Yes.into())
			}
		}

		/// Unregister the callers account id in order to free the deposit.
		///
		/// There is no reason to ever call this function other than freeing up the deposit.
		/// This is only useful when the account should no longer be used.
		///
		/// Disabled when [`Config::AutoMap`] is enabled, as accounts are automatically unmapped
		/// on kill via [`AutoMapper`].
		#[pallet::call_index(8)]
		#[pallet::weight(<T as Config>::WeightInfo::unmap_account())]
		pub fn unmap_account(origin: OriginFor<T>) -> DispatchResult {
			#[cfg(not(feature = "runtime-benchmarks"))]
			ensure!(!T::AutoMap::get(), <Error<T>>::AutoMappingEnabled);
			let origin = ensure_signed(origin)?;
			T::AddressMapper::unmap(&origin)
		}

		/// Dispatch an `call` with the origin set to the callers fallback address.
		///
		/// Every `AccountId32` can control its corresponding fallback account. The fallback account
		/// is the `AccountId20` with the last 12 bytes set to `0xEE`. This is essentially a
		/// recovery function in case an `AccountId20` was used without creating a mapping first.
		#[pallet::call_index(9)]
		#[pallet::weight({
			let dispatch_info = call.get_dispatch_info();
			(
				<T as Config>::WeightInfo::dispatch_as_fallback_account().saturating_add(dispatch_info.call_weight),
				dispatch_info.class
			)
		})]
		pub fn dispatch_as_fallback_account(
			mut origin: OriginFor<T>,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResultWithPostInfo {
			Self::ensure_non_contract_if_signed(&origin)?;
			let account_id = origin.as_signer().ok_or(DispatchError::BadOrigin)?;
			let unmapped_account = T::AddressMapper::to_fallback_account_id(
				&T::AddressMapper::to_address(&account_id),
			);
			origin.set_caller_from(RawOrigin::Signed(unmapped_account));
			call.dispatch(origin)
		}
	}
}

/// Create a dispatch result reflecting the amount of consumed weight.
fn dispatch_result<R>(
	result: Result<R, DispatchError>,
	weight_consumed: Weight,
	base_weight: Weight,
) -> DispatchResultWithPostInfo {
	let post_info = PostDispatchInfo {
		actual_weight: Some(weight_consumed.saturating_add(base_weight)),
		pays_fee: Default::default(),
	};

	result
		.map(|_| post_info)
		.map_err(|e| DispatchErrorWithPostInfo { post_info, error: e })
}

impl<T: Config> Pallet<T> {
	/// A generalized version of [`Self::call`].
	///
	/// Identical to [`Self::call`] but tailored towards being called by other code within the
	/// runtime as opposed to from an extrinsic. It returns more information and allows the
	/// enablement of features that are not suitable for an extrinsic (debugging, event
	/// collection).
	pub fn bare_call(
		origin: OriginFor<T>,
		dest: H160,
		evm_value: U256,
		transaction_limits: TransactionLimits<T>,
		data: Vec<u8>,
		exec_config: &ExecConfig<T>,
	) -> ContractResult<ExecReturnValue, BalanceOf<T>> {
		let mut transaction_meter = match TransactionMeter::new(transaction_limits) {
			Ok(transaction_meter) => transaction_meter,
			Err(error) => return ContractResult { result: Err(error), ..Default::default() },
		};
		let mut storage_deposit = Default::default();

		let try_call = || {
			let origin = ExecOrigin::from_runtime_origin(origin)?;
			let result = ExecStack::<T, ContractBlob<T>>::run_call(
				origin.clone(),
				dest,
				&mut transaction_meter,
				evm_value,
				data,
				&exec_config,
			)?;

			storage_deposit = transaction_meter
				.execute_postponed_deposits(&origin, &exec_config)
				.inspect_err(|err| {
				log::debug!(target: LOG_TARGET, "Failed to transfer deposit: {err:?}");
			})?;

			Ok(result)
		};
		let result = Self::run_guarded(try_call);

		log::trace!(target: LOG_TARGET, "Bare call ends: \
			result={result:?}, \
			weight_consumed={:?}, \
			weight_required={:?}, \
			storage_deposit={:?}, \
			gas_consumed={:?}, \
			max_storage_deposit={:?}",
			transaction_meter.weight_consumed(),
			transaction_meter.weight_required(),
			storage_deposit,
			transaction_meter.total_consumed_gas(),
			transaction_meter.deposit_required()
		);

		ContractResult {
			result: result.map_err(|r| r.error),
			weight_consumed: transaction_meter.weight_consumed(),
			weight_required: transaction_meter.weight_required(),
			storage_deposit,
			gas_consumed: transaction_meter.total_consumed_gas(),
			max_storage_deposit: transaction_meter.deposit_required(),
		}
	}

	/// Prepare a dry run for the given account.
	///
	///
	/// This function is public because it is called by the runtime API implementation
	/// (see `impl_runtime_apis_plus_revive`).
	pub fn prepare_dry_run(account: &T::AccountId) {
		// Bump the  nonce to simulate what would happen
		// `pre-dispatch` if the transaction was executed.
		frame_system::Pallet::<T>::inc_account_nonce(account);

		// Map the account if it is not mapped already so we don't hit
		// `AccountUnmapped` from the origin when dry-running.
		if !T::AddressMapper::is_mapped(account) {
			let _ = T::AddressMapper::map_no_deposit_unchecked(account);
		}
	}

	/// A generalized version of [`Self::instantiate`] or [`Self::instantiate_with_code`].
	///
	/// Identical to [`Self::instantiate`] or [`Self::instantiate_with_code`] but tailored towards
	/// being called by other code within the runtime as opposed to from an extrinsic. It returns
	/// more information to the caller useful to estimate the cost of the operation.
	pub fn bare_instantiate(
		origin: OriginFor<T>,
		evm_value: U256,
		transaction_limits: TransactionLimits<T>,
		code: Code,
		data: Vec<u8>,
		salt: Option<[u8; 32]>,
		exec_config: &ExecConfig<T>,
	) -> ContractResult<InstantiateReturnValue, BalanceOf<T>> {
		let mut transaction_meter = match TransactionMeter::new(transaction_limits) {
			Ok(transaction_meter) => transaction_meter,
			Err(error) => return ContractResult { result: Err(error), ..Default::default() },
		};

		let mut storage_deposit = Default::default();

		let try_instantiate = || {
			let instantiate_account = T::InstantiateOrigin::ensure_origin(origin.clone())?;

			if_tracing(|t| t.instantiate_code(&code, salt.as_ref()));
			let executable = match code {
				Code::Upload(code) if code.starts_with(&polkavm_common::program::BLOB_MAGIC) => {
					let upload_account = T::UploadOrigin::ensure_origin(origin)?;
					let executable = Self::try_upload_code(
						upload_account,
						code,
						BytecodeType::Pvm,
						&mut transaction_meter,
						&exec_config,
					)?;
					executable
				},
				Code::Upload(code) => {
					if T::AllowEVMBytecode::get() {
						ensure!(data.is_empty(), <Error<T>>::EvmConstructorNonEmptyData);
						let origin = T::UploadOrigin::ensure_origin(origin)?;
						let executable = ContractBlob::from_evm_init_code(code, origin)?;
						executable
					} else {
						return Err(<Error<T>>::CodeRejected.into());
					}
				},
				Code::Existing(code_hash) => {
					let executable = ContractBlob::from_storage(code_hash, &mut transaction_meter)?;
					ensure!(executable.code_info().is_pvm(), <Error<T>>::EvmConstructedFromHash);
					executable
				},
			};
			let instantiate_origin = ExecOrigin::from_account_id(instantiate_account.clone());
			let result = ExecStack::<T, ContractBlob<T>>::run_instantiate(
				instantiate_account,
				executable,
				&mut transaction_meter,
				evm_value,
				data,
				salt.as_ref(),
				&exec_config,
			);

			storage_deposit = transaction_meter
				.execute_postponed_deposits(&instantiate_origin, &exec_config)
				.inspect_err(|err| {
					log::debug!(target: LOG_TARGET, "Failed to transfer deposit: {err:?}");
				})?;
			result
		};
		let output = Self::run_guarded(try_instantiate);

		log::trace!(target: LOG_TARGET, "Bare instantiate ends: weight_consumed={:?}\
			weight_required={:?} \
			storage_deposit={:?} \
			gas_consumed={:?} \
			max_storage_deposit={:?}",
			transaction_meter.weight_consumed(),
			transaction_meter.weight_required(),
			storage_deposit,
			transaction_meter.total_consumed_gas(),
			transaction_meter.deposit_required()
		);

		ContractResult {
			result: output
				.map(|(addr, result)| InstantiateReturnValue { result, addr })
				.map_err(|e| e.error),
			weight_consumed: transaction_meter.weight_consumed(),
			weight_required: transaction_meter.weight_required(),
			storage_deposit,
			gas_consumed: transaction_meter.total_consumed_gas(),
			max_storage_deposit: transaction_meter.deposit_required(),
		}
	}

	/// Estimates the amount of gas that a transactions requires.
	///
	/// This function estimates the gas of the transaction according to the same binary search
	/// algorithm that's implemented in Geth. It stops when with an acceptable error ratio of
	/// 1.5% so that the algorithm terminates early.
	///
	/// # Note
	///
	/// All calls to [`Self::dry_run_eth_transact`] need to happen inside of a [`with_transaction`]
	/// with state rollback to ensure that dry runs subsequent to the first one preserve the correct
	/// amount of storage deposits needed without any kind of caching from the previous dry runs.
	pub fn eth_estimate_gas(
		tx: GenericTransaction,
		timestamp_override: Option<MomentOf<T>>,
		state_overrides: Option<StateOverrideSet>,
	) -> Result<U256, EthTransactError>
	where
		T::Nonce: Into<U256> + TryFrom<U256>,
		CallOf<T>: SetWeightLimit,
	{
		log::debug!(target: LOG_TARGET, "eth_estimate_gas: {tx:?}");

		let mut low = U256::zero();
		let mut high = Self::evm_block_gas_limit();

		log::trace!(target: LOG_TARGET, "eth_estimate_gas starting with low={low}, high={high}");

		// If the user has specified a gas limit then this is the limit we use as the high bound for
		// the binary search. Also, if the user didn't specify a gas limit then we need to skip the
		// balance checks.
		let perform_balance_checks = if let Some(gas_limit) = tx.gas {
			high = gas_limit;
			log::trace!(target: LOG_TARGET, "eth_estimate_gas high limited by the gas limit high={high}");
			true
		} else {
			false
		};

		// Cap the high bound of the binary search based on the account's balance if it can be done.
		let fee_cap = tx.max_fee_per_gas.or(tx.gas_price);
		if let (Some(fee_cap), Some(from), true) = (fee_cap, tx.from, perform_balance_checks) {
			let mut available_balance = Self::evm_balance(&from);
			if let Some(value) = tx.value {
				available_balance = available_balance.checked_sub(value).ok_or_else(|| {
					EthTransactError::Message("insufficient funds for value transfer".into())
				})?;
			}
			if let Some(allowance) = available_balance.checked_div(fee_cap) {
				if high > allowance && allowance != U256::zero() {
					log::trace!(target: LOG_TARGET, "eth_estimate_gas high limited by the user's allowance high={high} allowance={allowance}");
					high = allowance
				}
			}
		}

		// Run one gas probe in a rolled-back transaction. Overrides are passed along so that
		// `dry_run_eth_transact` applies them *after* `prepare_dry_run` bumps the nonce, keeping a
		// nonce override at the exact value it sets.
		let dry_run_at = |gas: U256| {
			let mut transaction = tx.clone();
			transaction.gas = Some(gas);
			with_transaction(|| {
				TransactionOutcome::Rollback(Ok::<_, DispatchError>(Self::dry_run_eth_transact(
					transaction,
					timestamp_override,
					perform_balance_checks,
					state_overrides.clone(),
				)))
			})
			.expect("Rollback shouldn't error out")
		};

		// Classify against post-override state (a code override can make the destination a
		// contract) in a rolled-back probe, so the overrides don't leak into the dry runs.
		let is_simple_transfer = with_transaction(|| {
			let probe = state_overrides
				.clone()
				.map_or(Ok(()), state_overrides::apply_state_overrides::<T>)
				.map(|()| Self::is_simple_transfer(&tx));
			TransactionOutcome::Rollback(Ok::<_, DispatchError>(probe))
		})
		.expect("Rollback shouldn't error out")?;

		if is_simple_transfer {
			let dry_run_result = dry_run_at(high)?;
			log::trace!(
				target: LOG_TARGET,
				"eth_estimate_gas short-circuited simple transfer to {:?} with eth_gas={}",
				tx.to,
				dry_run_result.eth_gas,
			);
			return Ok(dry_run_result.eth_gas);
		}

		// Perform the first dry run with the gas limit of the binary search's high bound. If it
		// fails then we attempt again with the max extrinsic weight in gas which we do since some
		// transactions fail the dry run with the highest gas limit. If both of these fail then we
		// return early as it means that the transaction simply can't succeed.
		let dry_run_results = [high, Self::evm_max_extrinsic_weight_in_gas()]
			.map(|gas_limit| (gas_limit, dry_run_at(gas_limit)));
		let (gas_limit, first_dry_run_result) = match dry_run_results {
			[(gas_limit1, Ok(dry_run_result1)), (gas_limit2, Ok(dry_run_result2))] => {
				if dry_run_result2.eth_gas >= gas_limit2 {
					(gas_limit1, dry_run_result1)
				} else {
					(gas_limit2, dry_run_result2)
				}
			},
			[(gas_limit, Ok(dry_run_result)), (_, Err(_))] |
			[(_, Err(_)), (gas_limit, Ok(dry_run_result))] => (gas_limit, dry_run_result),
			[(_, Err(err)), (_, Err(..))] => return Err(err),
		};
		log::trace!(
			target: LOG_TARGET,
			"eth_estimate_gas first dry run succeeded with gas_limit={} consumed={}",
			gas_limit,
			first_dry_run_result.eth_gas
		);
		low = first_dry_run_result.eth_gas;
		high = gas_limit;

		while low + U256::one() < high {
			log::trace!(target: LOG_TARGET, "eth_estimate_gas estimation iteration with low={low} high={high}");
			let error_ratio = high
				.checked_sub(low)
				.and_then(|value| value.checked_mul(U256::from(1000)))
				.and_then(|value| value.checked_div(high))
				.ok_or_else(|| {
					EthTransactError::Message(
						"failed to calculate error ratio in gas estimation".into(),
					)
				})?;
			if error_ratio <= U256::from(15) {
				log::trace!(
					target: LOG_TARGET,
					"eth_estimate_gas finished due to error ratio being less than 1.5% high={}",
					high
				);
				break;
			}

			let mut midpoint = high
				.checked_sub(low)
				.and_then(|value| value.checked_div(U256::from(2)))
				.and_then(|value| value.checked_add(low))
				.ok_or_else(|| {
					EthTransactError::Message(
						"failed to calculate midpoint in gas estimation".into(),
					)
				})?;

			if let Some(other_midpoint) = low.checked_mul(U256::from(2)) {
				if other_midpoint != U256::zero() {
					midpoint = midpoint.min(other_midpoint)
				}
			};

			let dry_run_result = dry_run_at(midpoint);
			log::trace!(target: LOG_TARGET, "eth_estimate_gas dry run result with midpoint={midpoint} is dry_run_result={dry_run_result:?}");
			match dry_run_result {
				Ok(..) => {
					log::trace!(target: LOG_TARGET, "eth_estimate_gas dry run succeeded, new high={midpoint}");
					high = midpoint
				},
				Err(..) => {
					log::trace!(target: LOG_TARGET, "eth_estimate_gas dry run failed, new low={midpoint}");
					low = midpoint
				},
			}
		}

		log::trace!(target: LOG_TARGET, "eth_estimate_gas completed. high={high}");
		Ok(high)
	}

	/// Returns true when `tx` is a plain value transfer that executes no code at its destination.
	pub(crate) fn is_simple_transfer(tx: &GenericTransaction) -> bool {
		tx.to
			.map(|to| tx.has_simple_transfer_fields() && Self::address_runs_no_code(&to))
			.unwrap_or(false)
	}

	/// Returns true when a value transfer can target `address` without triggering any code
	/// execution: it is neither the runtime pallets address, a precompile, nor a contract.
	fn address_runs_no_code(address: &H160) -> bool {
		// TODO(eip-7702): also reject delegated (authorized) destinations once EIP-7702
		// delegations land, since a transfer to one executes the delegate's code.
		*address != RUNTIME_PALLETS_ADDR &&
			!exec::is_precompile::<T, ContractBlob<T>>(address) &&
			!<AccountInfo<T>>::is_contract(address)
	}

	/// Return the pre-dispatch weight booked for the signed Ethereum transaction payload.
	///
	/// This matches the weight contribution that `frame_system::CheckWeight` would add for the
	/// transaction on an otherwise empty block:
	/// - the revive call's total dispatch weight, including extension weight,
	/// - the dispatch class base extrinsic weight,
	/// - and the extrinsic-length proof-size charge.
	pub fn eth_pre_dispatch_weight(transaction_encoded: Vec<u8>) -> Result<Weight, EthTransactError>
	where
		CallOf<T>: SetWeightLimit,
	{
		let signed_tx =
			crate::evm::TransactionSigned::decode(&transaction_encoded).map_err(|err| {
				EthTransactError::Message(format!("Failed to decode transaction: {err:?}"))
			})?;
		let signer_addr = signed_tx.recover_eth_address().map_err(|err| {
			EthTransactError::Message(format!("Failed to recover signer: {err:?}"))
		})?;
		let tx =
			GenericTransaction::from_signed(signed_tx, Self::evm_base_fee(), Some(signer_addr));
		let encoded_len = T::FeeInfo::encoded_len(
			crate::Call::<T>::eth_transact { payload: transaction_encoded.clone() }.into(),
		);
		let call_info = tx
			.into_call::<T>(CreateCallMode::ExtrinsicExecution(encoded_len, transaction_encoded))
			.map_err(|err| EthTransactError::Message(format!("Invalid call: {err:?}")))?;
		let info = T::FeeInfo::dispatch_info(&call_info.call);

		Ok(frame_system::calculate_consumed_extrinsic_weight::<CallOf<T>>(
			&T::BlockWeights::get(),
			&info,
			call_info.encoded_len as usize,
		))
	}

	/// Dry-run Ethereum calls.
	///
	/// # Parameters
	///
	/// - `tx`: The Ethereum transaction to simulate.
	/// - `timestamp_override`: An optional timestamp to report to the contract instead of the
	///   current one.
	/// - `perform_balance_checks`: Whether the origin's balance is checked to cover the fees and
	///   the transferred value.
	/// - `state_overrides`: Optional state overrides to apply before executing the call.
	pub fn dry_run_eth_transact(
		mut tx: GenericTransaction,
		timestamp_override: Option<MomentOf<T>>,
		perform_balance_checks: bool,
		state_overrides: Option<StateOverrideSet>,
	) -> Result<EthTransactInfo<BalanceOf<T>>, EthTransactError>
	where
		T::Nonce: Into<U256> + TryFrom<U256>,
		CallOf<T>: SetWeightLimit,
	{
		log::debug!(target: LOG_TARGET, "dry_run_eth_transact: {tx:?}");

		let origin = T::AddressMapper::to_account_id(&tx.from.unwrap_or_default());
		Self::prepare_dry_run(&origin);

		if let Some(overrides) = state_overrides {
			state_overrides::apply_state_overrides::<T>(overrides)?;
		}

		let base_fee = Self::evm_base_fee();
		let effective_gas_price = tx.effective_gas_price(base_fee).unwrap_or(base_fee);

		if effective_gas_price < base_fee {
			Err(EthTransactError::Message(format!(
				"Effective gas price {effective_gas_price:?} lower than base fee {base_fee:?}"
			)))?;
		}

		if tx.nonce.is_none() {
			tx.nonce = Some(<System<T>>::account_nonce(&origin).into());
		}
		if tx.chain_id.is_none() {
			tx.chain_id = Some(T::ChainId::get().into());
		}

		// tx.into_call expects tx.gas_price to be the effective gas price
		tx.gas_price = Some(effective_gas_price);
		// we don't support priority fee for now as the tipping system in pallet-transaction-payment
		// works differently and the total tip needs to be known pre dispatch
		tx.max_priority_fee_per_gas = Some(0.into());
		if tx.max_fee_per_gas.is_none() {
			tx.max_fee_per_gas = Some(effective_gas_price);
		}

		let gas = tx.gas;
		if tx.gas.is_none() {
			tx.gas = Some(Self::evm_block_gas_limit());
		}
		if tx.r#type.is_none() {
			tx.r#type = Some(TYPE_EIP1559.into());
		}

		// Store values before moving the tx
		let value = tx.value.unwrap_or_default();
		let input = tx.input.clone().to_vec();
		let from = tx.from;
		let to = tx.to;

		// we need to parse the weight from the transaction so that it is run
		// using the exact weight limit passed by the eth wallet
		let mut call_info = tx
			.into_call::<T>(CreateCallMode::DryRun)
			.map_err(|err| EthTransactError::Message(format!("Invalid call: {err:?}")))?;

		// the dry-run might leave out certain fields
		// in those cases we skip the check that the caller has enough balance
		// to pay for the fees
		let base_info = T::FeeInfo::base_dispatch_info(&mut call_info.call);
		let base_weight = base_info.total_weight();
		let exec_config =
			ExecConfig::new_eth_tx(effective_gas_price, call_info.encoded_len, base_weight)
				.with_dry_run(timestamp_override);

		// emulate transaction behavior
		let fees = call_info.tx_fee.saturating_add(call_info.storage_deposit);
		if let Some(from) = &from {
			let fees = if gas.is_some() && perform_balance_checks { fees } else { Zero::zero() };
			let balance = Self::evm_balance(from);
			if balance < Pallet::<T>::convert_native_to_evm(fees).saturating_add(value) {
				return Err(EthTransactError::Message(format!(
					"insufficient funds for gas * price + value ({fees:?}): address {from:?} have {balance:?} (supplied gas {gas:?})",
				)));
			}
		}

		// the deposit is done when the transaction is transformed from an `eth_transact`
		// we emulate this behavior for the dry-run here
		T::FeeInfo::deposit_txfee(T::Currency::issue(fees));

		let extract_error = |err| {
			if err == Error::<T>::StorageDepositNotEnoughFunds.into() {
				Err(EthTransactError::Message(format!("Not enough gas supplied: {err:?}")))
			} else {
				Err(EthTransactError::Message(format!("failed to run contract: {err:?}")))
			}
		};

		let transaction_limits = TransactionLimits::EthereumGas {
			eth_gas_limit: call_info.eth_gas_limit.saturated_into(),
			weight_limit: Self::evm_max_extrinsic_weight(),
			eth_tx_info: EthTxInfo::new(call_info.encoded_len, base_weight),
		};

		// Dry run the call
		let mut dry_run = match to {
			// A contract call.
			Some(dest) => {
				if dest == RUNTIME_PALLETS_ADDR {
					let Ok(dispatch_call) = <CallOf<T>>::decode(&mut &input[..]) else {
						return Err(EthTransactError::Message(format!(
							"Failed to decode pallet-call {input:?}"
						)));
					};

					if let Err(result) =
						dispatch_call.clone().dispatch(RawOrigin::Signed(origin).into())
					{
						return Err(EthTransactError::Message(format!(
							"Failed to dispatch call: {:?}",
							result.error,
						)));
					};

					Default::default()
				} else {
					// Dry run the call.
					let result = crate::Pallet::<T>::bare_call(
						OriginFor::<T>::signed(origin),
						dest,
						value,
						transaction_limits,
						input.clone(),
						&exec_config,
					);

					let data = match result.result {
						Ok(return_value) => {
							if return_value.did_revert() {
								return Err(EthTransactError::Data(return_value.data));
							}
							return_value.data
						},
						Err(err) => {
							log::debug!(target: LOG_TARGET, "Failed to execute call: {err:?}");
							return extract_error(err);
						},
					};

					EthTransactInfo {
						weight_required: result.weight_required,
						storage_deposit: result.storage_deposit.charge_or_zero(),
						max_storage_deposit: result.max_storage_deposit.charge_or_zero(),
						data,
						eth_gas: Default::default(),
					}
				}
			},
			// A contract deployment
			None => {
				// Extract code and data from the input.
				let (code, data) = if input.starts_with(&polkavm_common::program::BLOB_MAGIC) {
					extract_code_and_data(&input).unwrap_or_else(|| (input, Default::default()))
				} else {
					(input, vec![])
				};

				// Dry run the call.
				let result = crate::Pallet::<T>::bare_instantiate(
					OriginFor::<T>::signed(origin),
					value,
					transaction_limits,
					Code::Upload(code.clone()),
					data.clone(),
					None,
					&exec_config,
				);

				let returned_data = match result.result {
					Ok(return_value) => {
						if return_value.result.did_revert() {
							return Err(EthTransactError::Data(return_value.result.data));
						}
						return_value.result.data
					},
					Err(err) => {
						log::debug!(target: LOG_TARGET, "Failed to instantiate: {err:?}");
						return extract_error(err);
					},
				};

				EthTransactInfo {
					weight_required: result.weight_required,
					storage_deposit: result.storage_deposit.charge_or_zero(),
					max_storage_deposit: result.max_storage_deposit.charge_or_zero(),
					data: returned_data,
					eth_gas: Default::default(),
				}
			},
		};

		// replace the weight passed in the transaction with the dry_run result
		call_info.call.set_weight_limit(dry_run.weight_required);

		// we notify the wallet that the tx would not fit
		let total_weight = T::FeeInfo::dispatch_info(&call_info.call).total_weight();
		let max_weight = Self::evm_max_extrinsic_weight();
		if total_weight.any_gt(max_weight) {
			log::debug!(target: LOG_TARGET, "Transaction weight estimate exceeds extrinsic maximum: \
				total_weight={total_weight:?} \
				max_weight={max_weight:?}",
			);

			Err(EthTransactError::Message(format!(
				"\
				The transaction consumes more than the allowed weight. \
				needed={total_weight} \
				allowed={max_weight} \
				overweight_by={}\
				",
				total_weight.saturating_sub(max_weight),
			)))?;
		}

		// not enough gas supplied to pay for both the tx fees and the storage deposit
		let transaction_fee = T::FeeInfo::tx_fee(call_info.encoded_len, &call_info.call);
		let available_fee = T::FeeInfo::remaining_txfee();
		if transaction_fee > available_fee {
			Err(EthTransactError::Message(format!(
				"Not enough gas supplied: Off by: {:?}",
				transaction_fee.saturating_sub(available_fee),
			)))?;
		}

		let total_cost = transaction_fee.saturating_add(dry_run.max_storage_deposit);
		let total_cost_wei = Pallet::<T>::convert_native_to_evm(total_cost);
		let (mut eth_gas, rest) = total_cost_wei.div_mod(base_fee);
		if !rest.is_zero() {
			eth_gas = eth_gas.saturating_add(1_u32.into());
		}

		log::debug!(target: LOG_TARGET, "\
			dry_run_eth_transact finished: \
			weight_limit={}, \
			total_weight={total_weight}, \
			max_weight={max_weight}, \
			weight_left={}, \
			eth_gas={eth_gas}, \
			encoded_len={}, \
			tx_fee={transaction_fee:?}, \
			storage_deposit={:?}, \
			max_storage_deposit={:?}\
			",
			dry_run.weight_required,
			max_weight.saturating_sub(total_weight),
			call_info.encoded_len,
			dry_run.storage_deposit,
			dry_run.max_storage_deposit,

		);
		dry_run.eth_gas = eth_gas;
		Ok(dry_run)
	}

	/// Get the balance with EVM decimals of the given `address`.
	///
	/// Returns the spendable balance excluding the existential deposit.
	pub fn evm_balance(address: &H160) -> U256 {
		let balance = AccountInfo::<T>::balance_of((*address).into());
		Self::convert_native_to_evm(balance)
	}

	/// Get the current Ethereum block from storage.
	pub fn eth_block() -> EthBlock {
		EthereumBlock::<T>::get()
	}

	/// Convert the Ethereum block number into the Ethereum block hash.
	///
	/// # Note
	///
	/// The Ethereum block number is identical to the Substrate block number.
	/// If the provided block number is outside of the pruning None is returned.
	pub fn eth_block_hash_from_number(number: U256) -> Option<H256> {
		let number = BlockNumberFor::<T>::try_from(number).ok()?;
		let hash = <BlockHash<T>>::get(number);
		if hash == H256::zero() { None } else { Some(hash) }
	}

	/// The details needed to reconstruct the receipt information offchain.
	pub fn eth_receipt_data() -> Vec<ReceiptGasInfo> {
		ReceiptInfoData::<T>::get()
	}

	/// Set the EVM balance of an account.
	///
	/// The account's total balance becomes the EVM value plus the existential deposit,
	/// consistent with `evm_balance` which returns the spendable balance excluding the existential
	/// deposit.
	pub fn set_evm_balance(address: &H160, evm_value: U256) -> Result<(), Error<T>> {
		let (balance, dust) = Self::new_balance_with_dust(evm_value)
			.map_err(|_| <Error<T>>::BalanceConversionFailed)?;
		let account_id = T::AddressMapper::to_account_id(&address);
		T::Currency::set_balance(&account_id, balance);
		AccountInfoOf::<T>::mutate(&address, |account| {
			if let Some(account) = account {
				account.dust = dust;
			} else {
				*account = Some(AccountInfo { dust, ..Default::default() });
			}
		});

		Ok(())
	}

	/// Construct native balance from EVM balance.
	///
	/// Adds the existential deposit and returns the native balance plus the dust.
	pub fn new_balance_with_dust(
		evm_value: U256,
	) -> Result<(BalanceOf<T>, u32), BalanceConversionError> {
		let ed = T::Currency::minimum_balance();
		let balance_with_dust = BalanceWithDust::<BalanceOf<T>>::from_value::<T>(evm_value)?;
		let (value, dust) = balance_with_dust.deconstruct();

		Ok((ed.saturating_add(value), dust))
	}

	/// Get the nonce for the given `address`.
	pub fn evm_nonce(address: &H160) -> u32
	where
		T::Nonce: Into<u32>,
	{
		let account = T::AddressMapper::to_account_id(&address);
		System::<T>::account_nonce(account).into()
	}

	/// Get the block gas limit.
	pub fn evm_block_gas_limit() -> U256 {
		// We just return `u64::MAX` because the gas cost of a transaction can get very large when
		// the transaction executes many storage deposits (in theory a contract can behave like a
		// factory, procedurally create code and make contract creation calls to store that as
		// code). It is too brittle to estimate a maximally possible amount here.
		// On the other hand, the data type `u64` seems to be the "common denominator" as the
		// typical data type tools and Ethereum implementations use to represent gas amounts.
		u64::MAX.into()
	}

	/// Returns the maximum value of gas that can be represented in weights.
	pub fn evm_max_extrinsic_weight_in_gas() -> U256 {
		let max_extrinsic_fee = T::FeeInfo::weight_to_fee(&Self::evm_max_extrinsic_weight());
		let gas_scale: BalanceOf<T> = T::GasScale::get().into();
		(max_extrinsic_fee / gas_scale).into()
	}

	/// The maximum weight an `eth_transact` is allowed to consume.
	pub fn evm_max_extrinsic_weight() -> Weight {
		let factor = <T as Config>::MaxEthExtrinsicWeight::get();
		let max_weight = <T as frame_system::Config>::BlockWeights::get()
			.get(DispatchClass::Normal)
			.max_extrinsic
			.unwrap_or_else(|| <T as frame_system::Config>::BlockWeights::get().max_block);
		Weight::from_parts(
			factor.saturating_mul_int(max_weight.ref_time()),
			factor.saturating_mul_int(max_weight.proof_size()),
		)
	}

	/// Get the base gas price.
	pub fn evm_base_fee() -> U256 {
		let gas_scale = <T as Config>::GasScale::get();
		let multiplier = T::FeeInfo::next_fee_multiplier();
		multiplier
			.saturating_mul_int::<u128>(T::NativeToEthRatio::get().into())
			.saturating_mul(gas_scale.saturated_into())
			.into()
	}

	/// Build an EVM tracer from the given tracer type.
	pub fn evm_tracer(tracer_type: TracerType) -> Tracer<T>
	where
		T::Nonce: Into<u32>,
	{
		match tracer_type {
			TracerType::CallTracer(config) => CallTracer::new(config.unwrap_or_default()).into(),
			TracerType::PrestateTracer(config) => {
				PrestateTracer::new(config.unwrap_or_default()).into()
			},
			TracerType::ExecutionTracer(config) => {
				ExecutionTracer::new(config.unwrap_or_default()).into()
			},
		}
	}

	/// A generalized version of [`Self::upload_code`].
	///
	/// It is identical to [`Self::upload_code`] and only differs in the information it returns.
	pub fn bare_upload_code(
		origin: OriginFor<T>,
		code: Vec<u8>,
		storage_deposit_limit: BalanceOf<T>,
	) -> CodeUploadResult<BalanceOf<T>> {
		let origin = T::UploadOrigin::ensure_origin(origin)?;

		let bytecode_type = if code.starts_with(&polkavm_common::program::BLOB_MAGIC) {
			BytecodeType::Pvm
		} else {
			if !T::AllowEVMBytecode::get() {
				return Err(<Error<T>>::CodeRejected.into());
			}
			BytecodeType::Evm
		};

		let mut meter = TransactionMeter::new(TransactionLimits::WeightAndDeposit {
			weight_limit: Default::default(),
			deposit_limit: storage_deposit_limit,
		})?;

		let module = Self::try_upload_code(
			origin,
			code,
			bytecode_type,
			&mut meter,
			&ExecConfig::new_substrate_tx(),
		)?;
		Ok(CodeUploadReturnValue {
			code_hash: *module.code_hash(),
			deposit: meter.deposit_consumed().charge_or_zero(),
		})
	}

	/// Query storage of a specified contract under a specified key.
	pub fn get_storage(address: H160, key: [u8; 32]) -> GetStorageResult {
		let contract_info =
			AccountInfo::<T>::load_contract(&address).ok_or(ContractAccessError::DoesntExist)?;

		let maybe_value = contract_info.read(&Key::from_fixed(key));
		Ok(maybe_value)
	}

	/// Get the immutable data of a specified contract.
	///
	/// Returns `None` if the contract does not exist or has no immutable data.
	pub fn get_immutables(address: H160) -> Option<ImmutableData> {
		let immutable_data = <ImmutableDataOf<T>>::get(address);
		immutable_data
	}

	/// Sets immutable data of a contract
	///
	/// Returns an error if the contract does not exist.
	///
	/// # Warning
	///
	/// Does not collect any storage deposit. Not safe to be called by user controlled code.
	pub fn set_immutables(address: H160, data: ImmutableData) -> Result<(), ContractAccessError> {
		AccountInfo::<T>::load_contract(&address).ok_or(ContractAccessError::DoesntExist)?;
		<ImmutableDataOf<T>>::insert(address, data);
		Ok(())
	}

	/// Query storage of a specified contract under a specified variable-sized key.
	pub fn get_storage_var_key(address: H160, key: Vec<u8>) -> GetStorageResult {
		let contract_info =
			AccountInfo::<T>::load_contract(&address).ok_or(ContractAccessError::DoesntExist)?;

		let maybe_value = contract_info.read(
			&Key::try_from_var(key)
				.map_err(|_| ContractAccessError::KeyDecodingFailed)?
				.into(),
		);
		Ok(maybe_value)
	}

	/// Convert a native balance to EVM balance.
	pub fn convert_native_to_evm(value: impl Into<BalanceWithDust<BalanceOf<T>>>) -> U256 {
		let (value, dust) = value.into().deconstruct();
		value
			.into()
			.saturating_mul(T::NativeToEthRatio::get().into())
			.saturating_add(dust.into())
	}

	/// Set storage of a specified contract under a specified key.
	///
	/// If the `value` is `None`, the storage entry is deleted.
	///
	/// Returns an error if the contract does not exist or if the write operation fails.
	///
	/// # Warning
	///
	/// Does not collect any storage deposit. Not safe to be called by user controlled code.
	pub fn set_storage(address: H160, key: [u8; 32], value: Option<Vec<u8>>) -> SetStorageResult {
		let contract_info =
			AccountInfo::<T>::load_contract(&address).ok_or(ContractAccessError::DoesntExist)?;

		contract_info
			.write(&Key::from_fixed(key), value, None, false)
			.map_err(ContractAccessError::StorageWriteFailed)
	}

	/// Set the storage of a specified contract under a specified variable-sized key.
	///
	/// If the `value` is `None`, the storage entry is deleted.
	///
	/// Returns an error if the contract does not exist, if the key decoding fails,
	/// or if the write operation fails.
	///
	/// # Warning
	///
	/// Does not collect any storage deposit. Not safe to be called by user controlled code.
	pub fn set_storage_var_key(
		address: H160,
		key: Vec<u8>,
		value: Option<Vec<u8>>,
	) -> SetStorageResult {
		let contract_info =
			AccountInfo::<T>::load_contract(&address).ok_or(ContractAccessError::DoesntExist)?;

		contract_info
			.write(
				&Key::try_from_var(key)
					.map_err(|_| ContractAccessError::KeyDecodingFailed)?
					.into(),
				value,
				None,
				false,
			)
			.map_err(ContractAccessError::StorageWriteFailed)
	}

	/// Pallet account, used to hold funds for contracts upload deposit.
	pub fn account_id() -> T::AccountId {
		use frame_support::PalletId;
		use sp_runtime::traits::AccountIdConversion;
		PalletId(*b"py/reviv").into_account_truncating()
	}

	/// The address of the validator that produced the current block.
	pub fn block_author() -> H160 {
		use frame_support::traits::FindAuthor;

		let digest = <frame_system::Pallet<T>>::digest();
		let pre_runtime_digests = digest.logs.iter().filter_map(|d| d.as_pre_runtime());

		T::FindAuthor::find_author(pre_runtime_digests)
			.map(|account_id| T::AddressMapper::to_address(&account_id))
			.unwrap_or_default()
	}

	/// Returns the code at `address`.
	///
	/// This takes pre-compiles into account.
	pub fn code(address: &H160) -> Vec<u8> {
		use precompiles::{All, Precompiles};
		if let Some(code) = <All<T>>::code(address.as_fixed_bytes()) {
			return code.into();
		}
		AccountInfo::<T>::load_contract(&address)
			.and_then(|contract| <PristineCode<T>>::get(contract.code_hash))
			.map(|code| code.into())
			.unwrap_or_default()
	}

	/// Uploads new code and returns the Vm binary contract blob and deposit amount collected.
	pub fn try_upload_code(
		origin: T::AccountId,
		code: Vec<u8>,
		code_type: BytecodeType,
		meter: &mut TransactionMeter<T>,
		exec_config: &ExecConfig<T>,
	) -> Result<ContractBlob<T>, DispatchError> {
		let mut module = match code_type {
			BytecodeType::Pvm => ContractBlob::from_pvm_code(code, origin)?,
			BytecodeType::Evm => ContractBlob::from_evm_runtime_code(code, origin)?,
		};
		module.store_code(exec_config, meter)?;
		Ok(module)
	}

	/// Run the supplied function `f` if no other instance of this pallet is on the stack.
	fn run_guarded<R, F: FnOnce() -> Result<R, ExecError>>(f: F) -> Result<R, ExecError> {
		executing_contract::using_once(&mut false, || {
			executing_contract::with(|f| {
				// Fail if already entered contract execution
				if *f {
					return Err(())
				}
				// We are entering contract execution
				*f = true;
				Ok(())
			})
				.expect("Returns `Ok` if called within `using_once`. It is syntactically obvious that this is the case; qed")
				.map_err(|_| <Error<T>>::ReenteredPallet.into())
				.map(|_| f())
				.and_then(|r| r)
		})
	}

	/// Transfer a deposit from some account to another and place it on hold under `hold_reason`.
	///
	/// `from` is usually the transaction origin and `to` a contract or
	/// the pallets own account.
	fn charge_deposit(
		hold_reason: HoldReason,
		from: &T::AccountId,
		to: &T::AccountId,
		amount: BalanceOf<T>,
		exec_config: &ExecConfig<T>,
	) -> DispatchResult {
		if amount.is_zero() {
			return Ok(());
		}

		T::Deposit::charge_and_hold(hold_reason, exec_config.funds(from), to, amount)
			.map_err(|_| Error::<T>::StorageDepositNotEnoughFunds)?;
		Ok(())
	}

	/// Refund a deposit.
	///
	/// `dst` is usually the transaction origin and `from` a contract or
	/// the pallets own account.
	fn refund_deposit(
		hold_reason: HoldReason,
		from: &T::AccountId,
		dst: deposit_payment::Funds<T::AccountId>,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		if amount.is_zero() {
			return Ok(());
		}

		let to = match &dst {
			deposit_payment::Funds::Balance(to) | deposit_payment::Funds::TxFee(to) => *to,
		};
		let result = T::Deposit::refund_on_hold(hold_reason, from, dst, amount);

		result.defensive_map_err(|err| {
			let available = T::Deposit::total_on_hold(hold_reason, from);
			if available < amount {
				// The storage deposit accounting got out of sync with the balance: This would be a
				// straight up bug in this pallet.
				log::error!(
					target: LOG_TARGET,
					"Failed to refund storage deposit {amount:?} from contract {from:?} to origin {to:?}. Not enough deposit: {available:?}. This is a bug.",
				);
				Error::<T>::StorageRefundNotEnoughFunds.into()
			} else {
				// There are some locks preventing the refund. This could be the case if the
				// contract participates in government. The consequence is that if a contract votes
				// with its storage deposit it would no longer be possible to remove storage without first
				// reducing the lock.
				log::warn!(
					target: LOG_TARGET,
					"Failed to refund storage deposit {amount:?} from contract {from:?} to origin {to:?}: {err:?}. First remove locks (staking, governance) from the contracts account.",
				);
				Error::<T>::StorageRefundLocked.into()
			}
		})
	}

	/// Returns true if the evm value carries dust.
	fn has_dust(value: U256) -> bool {
		value % U256::from(<T>::NativeToEthRatio::get()) != U256::zero()
	}

	/// Returns true if the evm value carries balance.
	fn has_balance(value: U256) -> bool {
		value >= U256::from(<T>::NativeToEthRatio::get())
	}

	/// Return the existential deposit of [`Config::Currency`].
	#[cfg(any(feature = "runtime-benchmarks", feature = "try-runtime", test))]
	fn min_balance() -> BalanceOf<T> {
		<T::Currency as Inspect<AccountIdOf<T>>>::minimum_balance()
	}

	/// Deposit a pallet revive event.
	///
	/// This method will be called by the EVM to deposit events emitted by the contract.
	/// Therefore all events must be contract emitted events.
	fn deposit_event(event: Event<T>) {
		<frame_system::Pallet<T>>::deposit_event(<T as Config>::RuntimeEvent::from(event))
	}

	// Returns Ok with the account that signed the eth transaction.
	fn ensure_eth_signed(origin: OriginFor<T>) -> Result<AccountIdOf<T>, DispatchError> {
		match <T as Config>::RuntimeOrigin::from(origin).into() {
			Ok(Origin::EthTransaction(signer)) => Ok(signer),
			_ => Err(BadOrigin.into()),
		}
	}

	/// Ensure that the origin is neither a pre-compile nor a contract.
	///
	/// This enforces EIP-3607.
	fn ensure_non_contract_if_signed(origin: &OriginFor<T>) -> DispatchResult {
		if DebugSettings::bypass_eip_3607::<T>() {
			return Ok(());
		}
		let Some(address) = origin
			.as_system_ref()
			.and_then(|o| o.as_signed())
			.map(<T::AddressMapper as AddressMapper<T>>::to_address)
		else {
			return Ok(());
		};
		if exec::is_precompile::<T, ContractBlob<T>>(&address) ||
			<AccountInfo<T>>::is_contract(&address)
		{
			log::debug!(
				target: crate::LOG_TARGET,
				"EIP-3607: reject tx as pre-compile or account exist at {address:?}",
			);
			Err(DispatchError::BadOrigin)
		} else {
			Ok(())
		}
	}
}

/// The address used to call the runtime's pallets dispatchables
///
/// Note:
/// computed with PalletId(*b"py/paddr").into_account_truncating();
pub const RUNTIME_PALLETS_ADDR: H160 =
	H160(hex_literal::hex!("6d6f646c70792f70616464720000000000000000"));

// Set up a global reference to the boolean flag used for the re-entrancy guard.
environmental!(executing_contract: bool);

sp_api::decl_runtime_apis! {
	/// The API used to dry-run contract interactions.
	#[api_version(2)]
	pub trait ReviveApi<AccountId, Balance, Nonce, BlockNumber, Moment> where
		AccountId: Codec,
		Balance: Codec,
		Nonce: Codec,
		BlockNumber: Codec,
		Moment: Codec,
	{
		/// Returns the current ETH block.
		///
		/// This is one block behind the substrate block.
		#[deprecated(note = "Use the versioned equivalent `eth_block_versioned` if available on your runtime")]
		fn eth_block() -> BlockV1;

		/// Returns the ETH block hash for the given block number.
		#[deprecated(note = "Use the versioned equivalent `eth_block_hash_versioned` if available on your runtime")]
		fn eth_block_hash(number: U256) -> Option<H256>;

		/// The details needed to reconstruct the receipt information offchain.
		///
		/// # Note
		///
		/// Each entry corresponds to the appropriate Ethereum transaction in the current block.
		#[deprecated(note = "Use the versioned equivalent `eth_receipt_data_versioned` if available on your runtime")]
		fn eth_receipt_data() -> Vec<ReceiptGasInfoV1>;

		/// Returns the block gas limit.
		#[deprecated(note = "Use the versioned equivalent `block_gas_limit_versioned` if available on your runtime")]
		fn block_gas_limit() -> U256;

		/// Returns the block gas limit as calculated from the weights.
		#[deprecated(note = "Use the versioned equivalent `max_extrinsic_weight_in_gas_versioned` if available on your runtime")]
		fn max_extrinsic_weight_in_gas() -> U256;

		/// Returns the free balance of the given `[H160]` address, using EVM decimals.
		#[deprecated(note = "Use the versioned equivalent `balance_versioned` if available on your runtime")]
		fn balance(address: H160) -> U256;

		/// Returns the gas price.
		#[deprecated(note = "Use the versioned equivalent `gas_price_versioned` if available on your runtime")]
		fn gas_price() -> U256;

		/// Returns the nonce of the given `[H160]` address.
		#[deprecated(note = "Use the versioned equivalent `nonce_versioned` if available on your runtime")]
		fn nonce(address: H160) -> Nonce;

		/// Perform a call from a specified account to a given contract.
		///
		/// See [`crate::Pallet::bare_call`].
		#[deprecated(note = "Use the versioned equivalent `call_versioned` if available on your runtime")]
		fn call(
			origin: AccountId,
			dest: H160,
			value: Balance,
			gas_limit: Option<Weight>,
			storage_deposit_limit: Option<Balance>,
			input_data: Vec<u8>,
		) -> ContractResultV1<ExecReturnValueV1, Balance>;

		/// Instantiate a new contract.
		///
		/// See `[crate::Pallet::bare_instantiate]`.
		#[deprecated(note = "Use the versioned equivalent `instantiate_versioned` if available on your runtime")]
		fn instantiate(
			origin: AccountId,
			value: Balance,
			gas_limit: Option<Weight>,
			storage_deposit_limit: Option<Balance>,
			code: CodeV1,
			data: Vec<u8>,
			salt: Option<[u8; 32]>,
		) -> ContractResultV1<InstantiateReturnValueV1, Balance>;


		/// Perform an Ethereum call.
		///
		/// See [`crate::Pallet::dry_run_eth_transact`]
		#[deprecated(note = "Use the versioned equivalent `eth_transact_versioned` if available on your runtime")]
		fn eth_transact(tx: GenericTransactionV1) -> Result<EthTransactInfoV1<Balance>, EthTransactError>;

		/// Perform an Ethereum call.
		///
		/// See [`crate::Pallet::dry_run_eth_transact`]
		#[deprecated(note = "Use the versioned equivalent `eth_transact_versioned` if available on your runtime")]
		fn eth_transact_with_config(
			tx: GenericTransactionV1,
			config: DryRunConfigV1<Moment>,
		) -> Result<EthTransactInfoV1<Balance>, EthTransactError>;

		/// Estimates the amount of gas that a transactions requires.
		///
		/// This function estimates the gas of the transaction according to the same binary search
		/// algorithm that's implemented in Geth. It stops when with an acceptable error ratio of
		/// 1.5% so that the algorithm terminates early.
		#[deprecated(note = "Use the versioned equivalent `eth_estimate_gas_versioned` if available on your runtime")]
		fn eth_estimate_gas(
			tx: GenericTransactionV1,
			config: DryRunConfigV1<Moment>
		) -> Result<U256, EthTransactError>;

		/// Return the pre-dispatch weight booked for the signed Ethereum transaction payload.
		#[deprecated(note = "Use the versioned equivalent `eth_pre_dispatch_weight_versioned` if available on your runtime")]
		fn eth_pre_dispatch_weight(tx: Vec<u8>) -> Result<Weight, EthTransactError>;

		/// Upload new code without instantiating a contract from it.
		///
		/// See [`crate::Pallet::bare_upload_code`].
		#[deprecated(note = "Use the versioned equivalent `upload_code_versioned` if available on your runtime")]
		fn upload_code(
			origin: AccountId,
			code: Vec<u8>,
			storage_deposit_limit: Option<Balance>,
		) -> Result<CodeUploadReturnValueV1<Balance>, DispatchError>;

		/// Query a given storage key in a given contract.
		///
		/// Returns `Ok(Some(Vec<u8>))` if the storage value exists under the given key in the
		/// specified account and `Ok(None)` if it doesn't. If the account specified by the address
		/// doesn't exist, or doesn't have a contract then `Err` is returned.
		#[deprecated(note = "Use the versioned equivalent `get_storage_versioned` if available on your runtime")]
		fn get_storage(
			address: H160,
			key: [u8; 32],
		) -> GetStorageResult;

		/// Query a given variable-sized storage key in a given contract.
		///
		/// Returns `Ok(Some(Vec<u8>))` if the storage value exists under the given key in the
		/// specified account and `Ok(None)` if it doesn't. If the account specified by the address
		/// doesn't exist, or doesn't have a contract then `Err` is returned.
		#[deprecated(note = "Use the versioned equivalent `get_storage_versioned` if available on your runtime")]
		fn get_storage_var_key(
			address: H160,
			key: Vec<u8>,
		) -> GetStorageResult;

		/// Traces the execution of an entire block and returns call traces.
		///
		/// This is intended to be called through `state_call` to replay the block from the
		/// parent block.
		///
		/// See eth-rpc `debug_traceBlockByNumber` for usage.
		#[deprecated(note = "Use the versioned equivalent `trace_block_versioned` if available on your runtime")]
		fn trace_block(
			block: Block,
			config: TracerTypeV1
		) -> Vec<(u32, TraceV1)>;

		/// Traces the execution of a specific transaction within a block.
		///
		/// This is intended to be called through `state_call` to replay the block from the
		/// parent hash up to the transaction.
		///
		/// See eth-rpc `debug_traceTransaction` for usage.
		#[deprecated(note = "Use the versioned equivalent `trace_tx_versioned` if available on your runtime")]
		fn trace_tx(
			block: Block,
			tx_index: u32,
			config: TracerTypeV1
		) -> Option<TraceV1>;

		/// Dry run and return the trace of the given call.
		///
		/// See eth-rpc `debug_traceCall` for usage.
		#[deprecated(note = "Use the versioned equivalent `trace_call_versioned` if available on your runtime")]
		fn trace_call(tx: GenericTransactionV1, config: TracerTypeV1) -> Result<TraceV1, EthTransactError>;

		/// Dry run and return the trace of the given call with additional configuration.
		///
		/// Like [`Self::trace_call`], but accepts a [`TracingConfigV1`] that can carry state
		/// overrides. The config must be the **last argument** for backwards compatibility.
		#[deprecated(note = "Use the versioned equivalent `trace_call_versioned` if available on your runtime")]
		fn trace_call_with_config(
			tx: GenericTransactionV1,
			tracer_type: TracerTypeV1,
			config: TracingConfigV1,
		) -> Result<TraceV1, EthTransactError>;

		/// The address of the validator that produced the current block.
		#[deprecated(note = "Use the versioned equivalent `block_author_versioned` if available on your runtime")]
		fn block_author() -> H160;

		/// Get the H160 address associated to this account id
		#[deprecated(note = "Use the versioned equivalent `address_versioned` if available on your runtime")]
		fn address(account_id: AccountId) -> H160;

		/// Get the account id associated to this H160 address.
		#[deprecated(note = "Use the versioned equivalent `account_id_versioned` if available on your runtime")]
		fn account_id(address: H160) -> AccountId;

		/// The address used to call the runtime's pallets dispatchables
		#[deprecated(note = "Use the versioned equivalent `runtime_pallets_address_versioned` if available on your runtime")]
		fn runtime_pallets_address() -> H160;

		/// The code at the specified address taking pre-compiles into account.
		#[deprecated(note = "Use the versioned equivalent `code_versioned` if available on your runtime")]
		fn code(address: H160) -> Vec<u8>;

		/// Construct the new balance and dust components of this EVM balance.
		#[deprecated(note = "Use the versioned equivalent `new_balance_with_dust_versioned` if available on your runtime")]
		fn new_balance_with_dust(balance: U256) -> Result<(Balance, u32), BalanceConversionError>;

		/* Versioned Runtime APIs */

		#[api_version(2)]
		fn version_declarations() -> ReviveRuntimeApiVersionDeclarations;

		#[api_version(2)]
		fn eth_block_versioned(input: BlockVersionedInputPayload) -> BlockVersionedOutputPayload;

		#[api_version(2)]
		fn eth_block_hash_versioned(input: BlockHashVersionedInputPayload) -> BlockHashVersionedOutputPayload;

		#[api_version(2)]
		fn eth_receipt_data_versioned(input: ReceiptDataVersionedInputPayload) -> ReceiptDataVersionedOutputPayload;

		#[api_version(2)]
		fn block_gas_limit_versioned(
			input: BlockGasLimitVersionedInputPayload
		) -> BlockGasLimitVersionedOutputPayload;

		#[api_version(2)]
		fn max_extrinsic_weight_in_gas_versioned(
			input: MaxExtrinsicWeightInGasVersionedInputPayload
		) -> MaxExtrinsicWeightInGasVersionedOutputPayload;

		#[api_version(2)]
		fn balance_versioned(input: BalanceVersionedInputPayload) -> BalanceVersionedOutputPayload;

		#[api_version(2)]
		fn gas_price_versioned(input: GasPriceVersionedInputPayload) -> GasPriceVersionedOutputPayload;

		#[api_version(2)]
		fn nonce_versioned(input: NonceVersionedInputPayload) -> NonceVersionedOutputPayload<Nonce>;

		#[api_version(2)]
		fn call_versioned(
			input: CallVersionedInputPayload<AccountId, Balance>
		) -> CallVersionedOutputPayload<Balance>;

		#[api_version(2)]
		fn instantiate_versioned(
			input: InstantiateVersionedInputPayload<AccountId, Balance>
		) -> InstantiateVersionedOutputPayload<Balance>;

		#[api_version(2)]
		fn eth_transact_versioned(
			input: TransactVersionedInputPayload<Moment>
		) -> Result<TransactVersionedOutputPayload<Balance>, EthTransactError>;

		#[api_version(2)]
		fn eth_estimate_gas_versioned(
			input: EstimateGasVersionedInputPayload<Moment>
		) -> Result<EstimateGasVersionedOutputPayload, EthTransactError>;

		#[api_version(2)]
		fn eth_pre_dispatch_weight_versioned(
			input: PreDispatchWeightVersionedInputPayload
		) -> Result<PreDispatchWeightVersionedOutputPayload, EthTransactError>;

		#[api_version(2)]
		fn upload_code_versioned(
			input: UploadCodeVersionedInputPayload<AccountId, Balance>
		) -> Result<UploadCodeVersionedOutputPayload<Balance>, DispatchError>;

		#[api_version(2)]
		fn get_storage_versioned(
			input: GetStorageVersionedInputPayload
		) -> Result<GetStorageVersionedOutputPayload, ContractAccessError>;

		#[api_version(2)]
		fn runtime_pallets_address_versioned(
			input: RuntimePalletsAddressVersionedInputPayload
		) -> RuntimePalletsAddressVersionedOutputPayload;

		#[api_version(2)]
		fn code_versioned(input: CodeVersionedInputPayload) -> CodeVersionedOutputPayload;

		#[api_version(2)]
		fn account_id_versioned(input: AccountIdVersionedInputPayload) -> AccountIdVersionedOutputPayload<AccountId>;

		#[api_version(2)]
		fn new_balance_with_dust_versioned(
			input: NewBalanceWithDustVersionedInputPayload
		) -> Result<NewBalanceWithDustVersionedOutputPayload<Balance>, BalanceConversionError>;

		#[api_version(2)]
		fn block_author_versioned(input: BlockAuthorVersionedInputPayload) -> BlockAuthorVersionedOutputPayload;

		#[api_version(2)]
		fn address_versioned(input: AddressVersionedInputPayload<AccountId>) -> AddressVersionedOutputPayload;

		#[api_version(2)]
		fn trace_block_versioned(input: TraceBlockVersionedInputPayload<Block>) -> TraceBlockVersionedOutputPayload;

		#[api_version(2)]
		fn trace_tx_versioned(input: TraceTxVersionedInputPayload<Block>) -> TraceTxVersionedOutputPayload;

		#[api_version(2)]
		fn trace_call_versioned(
			input: TraceCallVersionedInputPayload
		) -> Result<TraceCallVersionedOutputPayload, EthTransactError>;
	}
}

/// This macro wraps substrate's `impl_runtime_apis!` and implements `pallet_revive` runtime APIs
/// and other required traits.
///
/// # Note
///
/// This also implements [`SetWeightLimit`] for the runtime call.
///
/// # Parameters
/// - `$Runtime`: The runtime type to implement the APIs for.
/// - `$Revive`: The name under which revive is declared in `construct_runtime`.
/// - `$Executive`: The Executive type of the runtime.
/// - `$EthExtra`: Type for additional Ethereum runtime extension.
/// - `$($rest:tt)*`: Remaining input to be forwarded to the underlying `impl_runtime_apis!`.
#[macro_export]
macro_rules! impl_runtime_apis_plus_revive_traits {
	($Runtime: ty, $Revive: ident, $Executive: ty, $EthExtra: ty, $($rest:tt)*) => {

		type __ReviveMacroMoment = $crate::MomentOf<$Runtime>;

		impl $crate::evm::runtime::SetWeightLimit for RuntimeCall {
			fn set_weight_limit(&mut self, new_weight_limit: Weight) -> Weight {
				use $crate::pallet::Call as ReviveCall;
				match self {
					Self::$Revive(
						ReviveCall::eth_call{ weight_limit, .. } |
						ReviveCall::eth_instantiate_with_code{ weight_limit, .. }
					) => {
						let old = *weight_limit;
						*weight_limit = new_weight_limit;
						old
					},
					_ => Weight::default(),
				}
			}
		}

		impl_runtime_apis! {
			$($rest)*

			#[api_version(2)]
			impl pallet_revive::ReviveApi<Block, AccountId, Balance, Nonce, BlockNumber, __ReviveMacroMoment> for $Runtime
			{
				fn eth_block() -> $crate::pallet_revive_types::runtime_api::BlockV1 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = BlockVersionedInputPayload::from(BlockInputPayloadV1);
					let output = Self::eth_block_versioned(input);
					BlockOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.block
				}

				fn eth_block_hash(number: $crate::U256) -> Option<$crate::H256> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = BlockHashVersionedInputPayload::from(BlockHashInputPayloadV1 {
						block_number: number
					});
					let output = Self::eth_block_hash_versioned(input);
					BlockHashOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.block_hash
				}

				fn eth_receipt_data() -> Vec<$crate::pallet_revive_types::runtime_api::ReceiptGasInfoV1> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = ReceiptDataVersionedInputPayload::from(ReceiptDataInputPayloadV1);
					let output = Self::eth_receipt_data_versioned(input);
					ReceiptDataOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.receipt_data
				}

				fn balance(address: $crate::H160) -> $crate::U256 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = BalanceVersionedInputPayload::from(BalanceInputPayloadV1 { address });
					let output = Self::balance_versioned(input);
					BalanceOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.balance
				}

				fn block_author() -> $crate::H160 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = BlockAuthorVersionedInputPayload::from(BlockAuthorInputPayloadV1);
					let output = Self::block_author_versioned(input);
					BlockAuthorOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.block_author
				}

				fn block_gas_limit() -> $crate::U256 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = BlockGasLimitVersionedInputPayload::from(BlockGasLimitInputPayloadV1);
					let output = Self::block_gas_limit_versioned(input);
					BlockGasLimitOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.block_gas_limit
				}

				fn max_extrinsic_weight_in_gas() -> $crate::U256 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = MaxExtrinsicWeightInGasVersionedInputPayload::from(
						MaxExtrinsicWeightInGasInputPayloadV1
					);
					let output = Self::max_extrinsic_weight_in_gas_versioned(input);
					MaxExtrinsicWeightInGasOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.max_extrinsic_weight_in_gas
				}

				fn gas_price() -> $crate::U256 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = GasPriceVersionedInputPayload::from(GasPriceInputPayloadV1);
					let output = Self::gas_price_versioned(input);
					GasPriceOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.gas_price
				}

				fn nonce(address: $crate::H160) -> Nonce {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = NonceVersionedInputPayload::from(NonceInputPayloadV1 { address });
					let output = Self::nonce_versioned(input);
					NonceOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.nonce
				}

				fn address(account_id: AccountId) -> $crate::H160 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = AddressVersionedInputPayload::from(AddressInputPayloadV1 { account_id });
					let output = Self::address_versioned(input);
					AddressOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.address
				}

				fn eth_transact(
					tx: $crate::pallet_revive_types::runtime_api::GenericTransactionV1,
				) -> Result<
					$crate::pallet_revive_types::runtime_api::EthTransactInfoV1<Balance>,
					$crate::EthTransactError
				> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = TransactVersionedInputPayload::from(TransactInputPayloadV1 {
						tx,
						timestamp_override: None,
						perform_balance_checks: true,
						state_overrides: None
					});
					let output = Self::eth_transact_versioned(input)?;
					Ok(TransactOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.transact_info)
				}

				fn eth_transact_with_config(
					tx: $crate::pallet_revive_types::runtime_api::GenericTransactionV1,
					config: $crate::pallet_revive_types::runtime_api::DryRunConfigV1<__ReviveMacroMoment>,
				) -> Result<
					$crate::pallet_revive_types::runtime_api::EthTransactInfoV1<Balance>,
					$crate::EthTransactError
				> {
					use $crate::pallet_revive_types::runtime_api::*;

					let DryRunConfigV1 { timestamp_override, perform_balance_checks, state_overrides } =
						config;

					let input = TransactVersionedInputPayload::from(TransactInputPayloadV1 {
						tx,
						timestamp_override,
						perform_balance_checks: perform_balance_checks.unwrap_or(false),
						state_overrides
					});
					let output = Self::eth_transact_versioned(input)?;
					Ok(TransactOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.transact_info)
				}

				fn eth_estimate_gas(
					tx: $crate::pallet_revive_types::runtime_api::GenericTransactionV1,
					config: $crate::pallet_revive_types::runtime_api::DryRunConfigV1<__ReviveMacroMoment>,
				) -> Result<$crate::U256, $crate::EthTransactError>  {
					use $crate::pallet_revive_types::runtime_api::*;

					let DryRunConfigV1 { timestamp_override, perform_balance_checks: _, state_overrides } =
						config;

					let input = EstimateGasVersionedInputPayload::from(EstimateGasInputPayloadV1 {
						tx,
						timestamp_override,
						state_overrides
					});
					let output = Self::eth_estimate_gas_versioned(input)?;
					Ok(EstimateGasOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.gas_estimate)
				}

				fn eth_pre_dispatch_weight(
					tx: Vec<u8>,
				) -> Result<$crate::Weight, $crate::EthTransactError> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = PreDispatchWeightVersionedInputPayload::from(
						PreDispatchWeightInputPayloadV1 { tx }
					);
					let output = Self::eth_pre_dispatch_weight_versioned(input)?;
					Ok(PreDispatchWeightOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.weight)
				}

				fn call(
					origin: AccountId,
					dest: $crate::H160,
					value: Balance,
					weight_limit: Option<$crate::Weight>,
					storage_deposit_limit: Option<Balance>,
					input_data: Vec<u8>,
				) -> $crate::pallet_revive_types::runtime_api::ContractResultV1<
					$crate::pallet_revive_types::runtime_api::ExecReturnValueV1,
					Balance
				> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = CallVersionedInputPayload::from(CallInputPayloadV1 {
						origin,
						dest,
						value,
						gas_limit: weight_limit,
						storage_deposit_limit,
						input_data
					});
					let output = Self::call_versioned(input);
					CallOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.contract_result
				}

				fn instantiate(
					origin: AccountId,
					value: Balance,
					weight_limit: Option<$crate::Weight>,
					storage_deposit_limit: Option<Balance>,
					code: $crate::pallet_revive_types::runtime_api::CodeV1,
					data: Vec<u8>,
					salt: Option<[u8; 32]>,
				) -> $crate::pallet_revive_types::runtime_api::ContractResultV1<
					$crate::pallet_revive_types::runtime_api::InstantiateReturnValueV1,
					Balance
				> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = InstantiateVersionedInputPayload::from(InstantiateInputPayloadV1 {
						origin,
						value,
						gas_limit: weight_limit,
						storage_deposit_limit,
						code,
						data,
						salt
					});
					let output = Self::instantiate_versioned(input);
					InstantiateOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.contract_result
				}

				fn upload_code(
					origin: AccountId,
					code: Vec<u8>,
					storage_deposit_limit: Option<Balance>,
				) -> Result<$crate::pallet_revive_types::runtime_api::CodeUploadReturnValueV1<Balance>, $crate::sp_runtime::DispatchError> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = UploadCodeVersionedInputPayload::from(UploadCodeInputPayloadV1 {
						origin,
						code,
						storage_deposit_limit
					});
					let output = Self::upload_code_versioned(input)?;
					Ok(UploadCodeOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.code_upload_return_value)
				}

				fn get_storage_var_key(
					address: $crate::H160,
					key: Vec<u8>,
				) -> $crate::GetStorageResult {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = GetStorageVersionedInputPayload::from(GetStorageInputPayloadV1 {
						address,
						key: StorageKeyV1::Variable(key)
					});
					let output = Self::get_storage_versioned(input)?;
					Ok(GetStorageOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.storage)
				}

				fn get_storage(address: $crate::H160, key: [u8; 32]) -> $crate::GetStorageResult {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = GetStorageVersionedInputPayload::from(GetStorageInputPayloadV1 {
						address,
						key: StorageKeyV1::Fixed(key)
					});
					let output = Self::get_storage_versioned(input)?;
					Ok(GetStorageOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.storage)
				}

				fn trace_block(
					block: Block,
					tracer_type: $crate::pallet_revive_types::runtime_api::TracerTypeV1,
				) -> Vec<(u32, $crate::pallet_revive_types::runtime_api::TraceV1)> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = TraceBlockVersionedInputPayload::from(TraceBlockInputPayloadV1 {
						block,
						config: tracer_type
					});
					let output = Self::trace_block_versioned(input);
					TraceBlockOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.traces
				}

				fn trace_tx(
					block: Block,
					tx_index: u32,
					tracer_type: $crate::pallet_revive_types::runtime_api::TracerTypeV1,
				) -> Option<$crate::pallet_revive_types::runtime_api::TraceV1> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = TraceTxVersionedInputPayload::from(TraceTxInputPayloadV1 {
						block,
						tx_index,
						config: tracer_type
					});
					let output = Self::trace_tx_versioned(input);
					TraceTxOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.trace
				}

				fn trace_call(
					tx: $crate::pallet_revive_types::runtime_api::GenericTransactionV1,
					tracer_type: $crate::pallet_revive_types::runtime_api::TracerTypeV1,
				) -> Result<$crate::pallet_revive_types::runtime_api::TraceV1, $crate::EthTransactError> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = TraceCallVersionedInputPayload::from(TraceCallInputPayloadV1 {
						tx,
						config: tracer_type,
						state_overrides: None
					});
					let output = Self::trace_call_versioned(input)?;
					Ok(TraceCallOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.trace)
				}

				fn trace_call_with_config(
					tx: $crate::pallet_revive_types::runtime_api::GenericTransactionV1,
					tracer_type: $crate::pallet_revive_types::runtime_api::TracerTypeV1,
					config: $crate::pallet_revive_types::runtime_api::TracingConfigV1,
				) -> Result<$crate::pallet_revive_types::runtime_api::TraceV1, $crate::EthTransactError> {
					use $crate::pallet_revive_types::runtime_api::*;

					let TracingConfigV1 { state_overrides } = config;

					let input = TraceCallVersionedInputPayload::from(TraceCallInputPayloadV1 {
						tx,
						config: tracer_type,
						state_overrides
					});
					let output = Self::trace_call_versioned(input)?;
					Ok(TraceCallOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.trace)
				}

				fn runtime_pallets_address() -> $crate::H160 {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = RuntimePalletsAddressVersionedInputPayload::from(
						RuntimePalletsAddressInputPayloadV1
					);
					let output = Self::runtime_pallets_address_versioned(input);
					RuntimePalletsAddressOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.runtime_pallets_address
				}

				fn code(address: $crate::H160) -> Vec<u8> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = CodeVersionedInputPayload::from(CodeInputPayloadV1 { address });
					let output = Self::code_versioned(input);
					CodeOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.code
				}

				fn account_id(address: $crate::H160) -> AccountId {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = AccountIdVersionedInputPayload::from(AccountIdInputPayloadV1 { address });
					let output = Self::account_id_versioned(input);
					AccountIdOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed")
						.account_id
				}

				fn new_balance_with_dust(balance: $crate::U256) -> Result<(Balance, u32), $crate::BalanceConversionError> {
					use $crate::pallet_revive_types::runtime_api::*;

					let input = NewBalanceWithDustVersionedInputPayload::from(
						NewBalanceWithDustInputPayloadV1 { balance }
					);
					let output = Self::new_balance_with_dust_versioned(input)?;
					let output = NewBalanceWithDustOutputPayloadV1::try_from(output)
						.expect("v1 input must produce v1 output; qed");
					Ok((output.new_balance, output.dust))
				}

				/* Versioned Runtime APIs */

				fn version_declarations()
					-> $crate::pallet_revive_types::runtime_api::ReviveRuntimeApiVersionDeclarations
				{
					use $crate::pallet_revive_types::runtime_api::*;

					ReviveRuntimeApiVersionDeclarations::new()
						.insert("eth_block_versioned", 1)
						.insert("eth_block_hash_versioned", 1)
						.insert("eth_receipt_data_versioned", 1)
						.insert("block_gas_limit_versioned", 1)
						.insert("max_extrinsic_weight_in_gas_versioned", 1)
						.insert("balance_versioned", 1)
						.insert("gas_price_versioned", 1)
						.insert("nonce_versioned", 1)
						.insert("call_versioned", 1)
						.insert("instantiate_versioned", 1)
						.insert("eth_transact_versioned", 1)
						.insert("eth_estimate_gas_versioned", 1)
						.insert("eth_pre_dispatch_weight_versioned", 1)
						.insert("upload_code_versioned", 1)
						.insert("get_storage_versioned", 1)
						.insert("runtime_pallets_address_versioned", 1)
						.insert("code_versioned", 1)
						.insert("account_id_versioned", 1)
						.insert("new_balance_with_dust_versioned", 1)
						.insert("block_author_versioned", 1)
						.insert("address_versioned", 1)
						.insert("trace_block_versioned", 2)
						.insert("trace_tx_versioned", 2)
						.insert("trace_call_versioned", 2)
				}

				fn eth_block_versioned(
					input: $crate::pallet_revive_types::runtime_api::BlockVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::BlockVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(BlockOutputPayload) -> BlockVersionedOutputPayload>,
					) = match input {
						BlockVersionedInputPayload::V1(payload) => (
							BlockInputPayload::from(payload),
							Box::new(|output| BlockVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = BlockOutputPayload { block: $crate::Pallet::<Self>::eth_block() };
					output_wrapper(output)
				}

				fn eth_block_hash_versioned(
					input: $crate::pallet_revive_types::runtime_api::BlockHashVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::BlockHashVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(BlockHashOutputPayload) -> BlockHashVersionedOutputPayload>,
					) = match input {
						BlockHashVersionedInputPayload::V1(payload) => (
							BlockHashInputPayload::from(payload),
							Box::new(|output| BlockHashVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = BlockHashOutputPayload {
						block_hash: $crate::Pallet::<Self>::eth_block_hash_from_number(input.block_number)
					};
					output_wrapper(output)
				}

				fn eth_receipt_data_versioned(
					input: $crate::pallet_revive_types::runtime_api::ReceiptDataVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::ReceiptDataVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(ReceiptDataOutputPayload) -> ReceiptDataVersionedOutputPayload>,
					) = match input {
						ReceiptDataVersionedInputPayload::V1(payload) => (
							ReceiptDataInputPayload::from(payload),
							Box::new(|output| ReceiptDataVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = ReceiptDataOutputPayload {
						receipt_data: $crate::Pallet::<Self>::eth_receipt_data()
					};
					output_wrapper(output)
				}

				fn block_gas_limit_versioned(
					input: $crate::pallet_revive_types::runtime_api::BlockGasLimitVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::BlockGasLimitVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(BlockGasLimitOutputPayload) -> BlockGasLimitVersionedOutputPayload>,
					) = match input {
						BlockGasLimitVersionedInputPayload::V1(payload) => (
							BlockGasLimitInputPayload::from(payload),
							Box::new(|output| BlockGasLimitVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = BlockGasLimitOutputPayload {
						block_gas_limit: $crate::Pallet::<Self>::evm_block_gas_limit()
					};
					output_wrapper(output)
				}

				fn max_extrinsic_weight_in_gas_versioned(
					input: $crate::pallet_revive_types::runtime_api::MaxExtrinsicWeightInGasVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::MaxExtrinsicWeightInGasVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(MaxExtrinsicWeightInGasOutputPayload) -> MaxExtrinsicWeightInGasVersionedOutputPayload>,
					) = match input {
						MaxExtrinsicWeightInGasVersionedInputPayload::V1(payload) => (
							MaxExtrinsicWeightInGasInputPayload::from(payload),
							Box::new(|output| MaxExtrinsicWeightInGasVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = MaxExtrinsicWeightInGasOutputPayload {
						max_extrinsic_weight_in_gas: $crate::Pallet::<Self>::evm_max_extrinsic_weight_in_gas()
					};
					output_wrapper(output)
				}

				fn balance_versioned(
					input: $crate::pallet_revive_types::runtime_api::BalanceVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::BalanceVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(BalanceOutputPayload) -> BalanceVersionedOutputPayload>,
					) = match input {
						BalanceVersionedInputPayload::V1(payload) => (
							BalanceInputPayload::from(payload),
							Box::new(|output| BalanceVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = BalanceOutputPayload {
						balance: $crate::Pallet::<Self>::evm_balance(&input.address)
					};
					output_wrapper(output)
				}

				fn gas_price_versioned(
					input: $crate::pallet_revive_types::runtime_api::GasPriceVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::GasPriceVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(GasPriceOutputPayload) -> GasPriceVersionedOutputPayload>,
					) = match input {
						GasPriceVersionedInputPayload::V1(payload) => (
							GasPriceInputPayload::from(payload),
							Box::new(|output| GasPriceVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = GasPriceOutputPayload {
						gas_price: $crate::Pallet::<Self>::evm_base_fee()
					};
					output_wrapper(output)
				}

				fn nonce_versioned(
					input: $crate::pallet_revive_types::runtime_api::NonceVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::NonceVersionedOutputPayload<Nonce> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::AddressMapper;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(NonceOutputPayload<Nonce>) -> NonceVersionedOutputPayload<Nonce>>,
					) = match input {
						NonceVersionedInputPayload::V1(payload) => (
							NonceInputPayload::from(payload),
							Box::new(|output| NonceVersionedOutputPayload::V1(output.into())),
						),
					};

					let account = <Self as $crate::Config>::AddressMapper::to_account_id(&input.address);
					let output = NonceOutputPayload {
						nonce: $crate::frame_system::Pallet::<Self>::account_nonce(account)
					};
					output_wrapper(output)
				}

				fn call_versioned(
					input: $crate::pallet_revive_types::runtime_api::CallVersionedInputPayload<AccountId, Balance>
				) -> $crate::pallet_revive_types::runtime_api::CallVersionedOutputPayload<Balance> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::frame_support::traits::Get;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(CallOutputPayload<Balance>) -> CallVersionedOutputPayload<Balance>>,
					) = match input {
						CallVersionedInputPayload::V1(payload) => (
							CallInputPayload::from(payload),
							Box::new(|output| CallVersionedOutputPayload::V1(output.into())),
						),
					};

					let blockweights: $crate::BlockWeights =
						<Self as $crate::frame_system::Config>::BlockWeights::get();

					$crate::Pallet::<Self>::prepare_dry_run(&input.origin);
					let contract_result = $crate::Pallet::<Self>::bare_call(
						<Self as $crate::frame_system::Config>::RuntimeOrigin::signed(input.origin),
						input.dest,
						$crate::Pallet::<Self>::convert_native_to_evm(input.value),
						$crate::TransactionLimits::WeightAndDeposit {
							weight_limit: input.gas_limit.unwrap_or(blockweights.max_block),
							deposit_limit: input.storage_deposit_limit.unwrap_or(u128::MAX),
						},
						input.input_data,
						&$crate::ExecConfig::new_substrate_tx().with_dry_run(None),
					);

					let output = CallOutputPayload { contract_result };
					output_wrapper(output)
				}

				fn instantiate_versioned(
					input: $crate::pallet_revive_types::runtime_api::InstantiateVersionedInputPayload<AccountId, Balance>
				) -> $crate::pallet_revive_types::runtime_api::InstantiateVersionedOutputPayload<Balance> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::frame_support::traits::Get;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(InstantiateOutputPayload<Balance>) -> InstantiateVersionedOutputPayload<Balance>>,
					) = match input {
						InstantiateVersionedInputPayload::V1(payload) => (
							InstantiateInputPayload::from(payload),
							Box::new(|output| InstantiateVersionedOutputPayload::V1(output.into())),
						),
					};

					let blockweights: $crate::BlockWeights =
						<Self as $crate::frame_system::Config>::BlockWeights::get();

					$crate::Pallet::<Self>::prepare_dry_run(&input.origin);
					let contract_result = $crate::Pallet::<Self>::bare_instantiate(
						<Self as $crate::frame_system::Config>::RuntimeOrigin::signed(input.origin),
						$crate::Pallet::<Self>::convert_native_to_evm(input.value),
						$crate::TransactionLimits::WeightAndDeposit {
							weight_limit: input.gas_limit.unwrap_or(blockweights.max_block),
							deposit_limit: input.storage_deposit_limit.unwrap_or(u128::MAX),
						},
						input.code,
						input.data,
						input.salt,
						&$crate::ExecConfig::new_substrate_tx().with_dry_run(None),
					);

					let output = InstantiateOutputPayload { contract_result };
					output_wrapper(output)
				}

				fn eth_transact_versioned(
					input: $crate::pallet_revive_types::runtime_api::TransactVersionedInputPayload<__ReviveMacroMoment>
				) -> Result<
					$crate::pallet_revive_types::runtime_api::TransactVersionedOutputPayload<Balance>,
					$crate::EthTransactError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::{
						codec::Encode, evm::runtime::EthExtra, frame_support::traits::Get,
						sp_runtime::traits::TransactionExtension,
						sp_runtime::traits::Block as BlockT
					};
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(TransactOutputPayload<Balance>) -> TransactVersionedOutputPayload<Balance>>,
					) = match input {
						TransactVersionedInputPayload::V1(payload) => (
							TransactInputPayload::from(payload),
							Box::new(|output| TransactVersionedOutputPayload::V1(output.into())),
						),
					};

					let transact_info = $crate::Pallet::<Self>::dry_run_eth_transact(
						input.tx,
						input.timestamp_override,
						input.perform_balance_checks,
						input.state_overrides,
					)?;
					let output = TransactOutputPayload { transact_info };
					Ok(output_wrapper(output))
				}

				fn eth_estimate_gas_versioned(
					input: $crate::pallet_revive_types::runtime_api::EstimateGasVersionedInputPayload<__ReviveMacroMoment>
				) -> Result<
					$crate::pallet_revive_types::runtime_api::EstimateGasVersionedOutputPayload,
					$crate::EthTransactError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::{
						codec::Encode, evm::runtime::EthExtra, frame_support::traits::Get,
						sp_runtime::traits::TransactionExtension,
						sp_runtime::traits::Block as BlockT
					};
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(EstimateGasOutputPayload) -> EstimateGasVersionedOutputPayload>,
					) = match input {
						EstimateGasVersionedInputPayload::V1(payload) => (
							EstimateGasInputPayload::from(payload),
							Box::new(|output| EstimateGasVersionedOutputPayload::V1(output.into())),
						),
					};

					let gas_estimate = $crate::Pallet::<Self>::eth_estimate_gas(
						input.tx,
						input.timestamp_override,
						input.state_overrides,
					)?;
					let output = EstimateGasOutputPayload { gas_estimate };
					Ok(output_wrapper(output))
				}

				fn eth_pre_dispatch_weight_versioned(
					input: $crate::pallet_revive_types::runtime_api::PreDispatchWeightVersionedInputPayload
				) -> Result<
					$crate::pallet_revive_types::runtime_api::PreDispatchWeightVersionedOutputPayload,
					$crate::EthTransactError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(PreDispatchWeightOutputPayload) -> PreDispatchWeightVersionedOutputPayload>,
					) = match input {
						PreDispatchWeightVersionedInputPayload::V1(payload) => (
							PreDispatchWeightInputPayload::from(payload),
							Box::new(|output| PreDispatchWeightVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = PreDispatchWeightOutputPayload {
						weight: $crate::Pallet::<Self>::eth_pre_dispatch_weight(input.tx)?
					};
					Ok(output_wrapper(output))
				}

				fn upload_code_versioned(
					input: $crate::pallet_revive_types::runtime_api::UploadCodeVersionedInputPayload<AccountId, Balance>
				) -> Result<
					$crate::pallet_revive_types::runtime_api::UploadCodeVersionedOutputPayload<Balance>,
					$crate::sp_runtime::DispatchError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(UploadCodeOutputPayload<Balance>) -> UploadCodeVersionedOutputPayload<Balance>>,
					) = match input {
						UploadCodeVersionedInputPayload::V1(payload) => (
							UploadCodeInputPayload::from(payload),
							Box::new(|output| UploadCodeVersionedOutputPayload::V1(output.into())),
						),
					};

					let origin =
						<Self as $crate::frame_system::Config>::RuntimeOrigin::signed(input.origin);
					let code_upload_return_value = $crate::Pallet::<Self>::bare_upload_code(
						origin,
						input.code,
						input.storage_deposit_limit.unwrap_or(u128::MAX),
					)?;
					let output = UploadCodeOutputPayload { code_upload_return_value };
					Ok(output_wrapper(output))
				}

				fn get_storage_versioned(
					input: $crate::pallet_revive_types::runtime_api::GetStorageVersionedInputPayload
				) -> Result<
					$crate::pallet_revive_types::runtime_api::GetStorageVersionedOutputPayload,
					$crate::ContractAccessError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(GetStorageOutputPayload) -> GetStorageVersionedOutputPayload>,
					) = match input {
						GetStorageVersionedInputPayload::V1(payload) => (
							GetStorageInputPayload::from(payload),
							Box::new(|output| GetStorageVersionedOutputPayload::V1(output.into())),
						),
					};

					let storage = match input.key {
						StorageKey::Fixed(key) => $crate::Pallet::<Self>::get_storage(input.address, key)?,
						StorageKey::Variable(key) => $crate::Pallet::<Self>::get_storage_var_key(input.address, key)?,
					};
					let output = GetStorageOutputPayload { storage };
					Ok(output_wrapper(output))
				}

				fn runtime_pallets_address_versioned(
					input: $crate::pallet_revive_types::runtime_api::RuntimePalletsAddressVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::RuntimePalletsAddressVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(RuntimePalletsAddressOutputPayload) -> RuntimePalletsAddressVersionedOutputPayload>,
					) = match input {
						RuntimePalletsAddressVersionedInputPayload::V1(payload) => (
							RuntimePalletsAddressInputPayload::from(payload),
							Box::new(|output| RuntimePalletsAddressVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = RuntimePalletsAddressOutputPayload {
						runtime_pallets_address: $crate::RUNTIME_PALLETS_ADDR
					};
					output_wrapper(output)
				}

				fn code_versioned(
					input: $crate::pallet_revive_types::runtime_api::CodeVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::CodeVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(CodeOutputPayload) -> CodeVersionedOutputPayload>,
					) = match input {
						CodeVersionedInputPayload::V1(payload) => (
							CodeInputPayload::from(payload),
							Box::new(|output| CodeVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = CodeOutputPayload {
						code: $crate::Pallet::<Self>::code(&input.address)
					};
					output_wrapper(output)
				}

				fn account_id_versioned(
					input: $crate::pallet_revive_types::runtime_api::AccountIdVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::AccountIdVersionedOutputPayload<AccountId> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::AddressMapper;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(AccountIdOutputPayload<AccountId>) -> AccountIdVersionedOutputPayload<AccountId>>,
					) = match input {
						AccountIdVersionedInputPayload::V1(payload) => (
							AccountIdInputPayload::from(payload),
							Box::new(|output| AccountIdVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = AccountIdOutputPayload {
						account_id: <Self as $crate::Config>::AddressMapper::to_account_id(&input.address)
					};
					output_wrapper(output)
				}

				fn new_balance_with_dust_versioned(
					input: $crate::pallet_revive_types::runtime_api::NewBalanceWithDustVersionedInputPayload
				) -> Result<
					$crate::pallet_revive_types::runtime_api::NewBalanceWithDustVersionedOutputPayload<Balance>,
					$crate::BalanceConversionError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<
							dyn Fn(NewBalanceWithDustOutputPayload<Balance>) -> NewBalanceWithDustVersionedOutputPayload<Balance>,
						>,
					) = match input {
						NewBalanceWithDustVersionedInputPayload::V1(payload) => (
							NewBalanceWithDustInputPayload::from(payload),
							Box::new(|output| NewBalanceWithDustVersionedOutputPayload::V1(output.into())),
						),
					};

					let (new_balance, dust) = $crate::Pallet::<Self>::new_balance_with_dust(input.balance)?;
					let output = NewBalanceWithDustOutputPayload { new_balance, dust };
					Ok(output_wrapper(output))
				}

				fn block_author_versioned(
					input: $crate::pallet_revive_types::runtime_api::BlockAuthorVersionedInputPayload
				) -> $crate::pallet_revive_types::runtime_api::BlockAuthorVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use alloc::boxed::Box;

					let (_input, output_wrapper): (
						_,
						Box<dyn Fn(BlockAuthorOutputPayload) -> BlockAuthorVersionedOutputPayload>,
					) = match input {
						BlockAuthorVersionedInputPayload::V1(payload) => (
							BlockAuthorInputPayload::from(payload),
							Box::new(|output| BlockAuthorVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = BlockAuthorOutputPayload {
						block_author: $crate::Pallet::<Self>::block_author()
					};
					output_wrapper(output)
				}

				fn address_versioned(
					input: $crate::pallet_revive_types::runtime_api::AddressVersionedInputPayload<AccountId>
				) -> $crate::pallet_revive_types::runtime_api::AddressVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::AddressMapper;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(AddressOutputPayload) -> AddressVersionedOutputPayload>,
					) = match input {
						AddressVersionedInputPayload::V1(payload) => (
							AddressInputPayload::from(payload),
							Box::new(|output| AddressVersionedOutputPayload::V1(output.into())),
						),
					};

					let output = AddressOutputPayload {
						address: <Self as $crate::Config>::AddressMapper::to_address(&input.account_id)
					};
					output_wrapper(output)
				}

				fn trace_block_versioned(
					input: $crate::pallet_revive_types::runtime_api::TraceBlockVersionedInputPayload<Block>
				) -> $crate::pallet_revive_types::runtime_api::TraceBlockVersionedOutputPayload {
					use $crate::{
						sp_runtime::traits::Block,
						tracing::trace,
						evm::TraceEntry,
						runtime_api::*,
						pallet_revive_types::runtime_api::*
					};
					use alloc::boxed::Box;

					let (input, output_wrapper): (_, Box<dyn Fn(TraceBlockOutputPayload) -> TraceBlockVersionedOutputPayload>) = match input {
						TraceBlockVersionedInputPayload::V1(payload) => (
							TraceBlockInputPayload::from(payload),
							Box::new(|output| TraceBlockVersionedOutputPayload::V1(output.into()))
						),
						TraceBlockVersionedInputPayload::V2(payload) => (
							TraceBlockInputPayload::from(payload),
							Box::new(|output| TraceBlockVersionedOutputPayload::V2(output.into()))
						),
					};

					if matches!(input.config, $crate::evm::TracerType::ExecutionTracer(_)) &&
						!$crate::DebugSettings::is_execution_tracing_enabled::<Runtime>()
					{
						return output_wrapper(Default::default())
					}

					// Faithful proof-size accounting needs a PoV recorder registered for this call
					// (e.g. the node's `state_callRecorded` RPC); without one the block tail may hit
					// `ExhaustsResources`. V1 drops such traces; V2 reports them as `NotTraced`.
					let mut entries = vec![];
					let (header, extrinsics) = input.block.deconstruct();
					<$Executive>::initialize_block(&header);
					for (index, ext) in extrinsics.into_iter().enumerate() {
						let mut tracer = $crate::Pallet::<Self>::evm_tracer(input.config.clone());
						let t = tracer.as_tracing();
						let result = trace(t, || <$Executive>::apply_extrinsic(ext));

						if let Some(tx_trace) = tracer.collect_trace() {
							entries.push((index as u32, TraceEntry::Traced(tx_trace)));
						} else if let Some(entry) = TraceEntry::for_untraced(&result) {
							entries.push((index as u32, entry));
						}
					}

					let output = TraceBlockOutputPayload { entries };
					output_wrapper(output)
				}

				fn trace_tx_versioned(
					input: $crate::pallet_revive_types::runtime_api::TraceTxVersionedInputPayload<Block>
				) -> $crate::pallet_revive_types::runtime_api::TraceTxVersionedOutputPayload {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::{evm::TraceEntry, sp_runtime::traits::Block, tracing::trace};
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(TraceTxOutputPayload) -> TraceTxVersionedOutputPayload>,
					) = match input {
						TraceTxVersionedInputPayload::V1(payload) => (
							TraceTxInputPayload::from(payload),
							Box::new(|output| TraceTxVersionedOutputPayload::V1(output.into())),
						),
						TraceTxVersionedInputPayload::V2(payload) => (
							TraceTxInputPayload::from(payload),
							Box::new(|output| TraceTxVersionedOutputPayload::V2(output.into())),
						),
					};

					if matches!(&input.config, $crate::evm::TracerType::ExecutionTracer(_)) &&
						!$crate::DebugSettings::is_execution_tracing_enabled::<Runtime>()
					{
						return output_wrapper(TraceTxOutputPayload { entry: None })
					}

					// Faithful proof-size accounting needs a PoV recorder registered for this call
					// (e.g. the node's `state_callRecorded` RPC); without one the block tail may hit
					// `ExhaustsResources`. V1 drops such traces; V2 reports them as `NotTraced`.
					let mut tracer = $crate::Pallet::<Self>::evm_tracer(input.config);
					let (header, extrinsics) = input.block.deconstruct();

					<$Executive>::initialize_block(&header);
					let mut entry = None;
					for (index, ext) in extrinsics.into_iter().enumerate() {
						if index as u32 == input.tx_index {
							let t = tracer.as_tracing();
							let result = trace(t, || <$Executive>::apply_extrinsic(ext));
							entry = match tracer.collect_trace() {
								Some(tx_trace) => Some(TraceEntry::Traced(tx_trace)),
								None => TraceEntry::for_untraced(&result),
							};
							break;
						} else {
							let _ = <$Executive>::apply_extrinsic(ext);
						}
					}

					let output = TraceTxOutputPayload { entry };
					output_wrapper(output)
				}

				fn trace_call_versioned(
					input: $crate::pallet_revive_types::runtime_api::TraceCallVersionedInputPayload
				) -> Result<
					$crate::pallet_revive_types::runtime_api::TraceCallVersionedOutputPayload,
					$crate::EthTransactError
				> {
					use $crate::pallet_revive_types::runtime_api::*;
					use $crate::runtime_api::*;
					use $crate::tracing::trace;
					use alloc::boxed::Box;

					let (input, output_wrapper): (
						_,
						Box<dyn Fn(TraceCallOutputPayload) -> TraceCallVersionedOutputPayload>,
					) = match input {
						TraceCallVersionedInputPayload::V1(payload) => (
							TraceCallInputPayload::from(payload),
							Box::new(|output| TraceCallVersionedOutputPayload::V1(output.into())),
						),
						TraceCallVersionedInputPayload::V2(payload) => (
							TraceCallInputPayload::from(payload),
							Box::new(|output| TraceCallVersionedOutputPayload::V2(output.into())),
						),
					};

					if let Some(overrides) = input.state_overrides {
						$crate::state_overrides::apply_state_overrides::<Runtime>(overrides)?;
					}

					if matches!(input.config, $crate::evm::TracerType::ExecutionTracer(_)) &&
						!$crate::DebugSettings::is_execution_tracing_enabled::<Runtime>()
					{
						return Err($crate::EthTransactError::Message("Execution Tracing is disabled".into()))
					}

					let mut tracer = $crate::Pallet::<Self>::evm_tracer(input.config.clone());
					let t = tracer.as_tracing();

					t.watch_address(&input.tx.from.unwrap_or_default());
					t.watch_address(&$crate::Pallet::<Self>::block_author());
					let result = trace(t, || {
						$crate::Pallet::<Self>::dry_run_eth_transact(input.tx, None, true, None)
					});

					let trace = if let Some(trace) = tracer.collect_trace() {
						Ok(trace)
					} else if let Err(err) = result {
						Err(err)
					} else {
						Ok($crate::Pallet::<Self>::evm_tracer(input.config).empty_trace())
					}?;

					let output = TraceCallOutputPayload { trace };
					Ok(output_wrapper(output))
				}
			}
		}
	};
}
