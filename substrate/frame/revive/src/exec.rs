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
	AccountInfo, AccountInfoOf, BalanceOf, BalanceWithDust, Code, CodeInfo, CodeInfoOf,
	CodeRemoved, Config, ContractInfo, Error, Event, ImmutableData, ImmutableDataOf, LOG_TARGET,
	Pallet as Contracts, RuntimeCosts,
	access_list::{AccessEntry, AccessList, StorageAccessKind, StorageOp},
	address::{self, AddressMapper},
	deposit_payment::Deposit as _,
	evm::{block_storage, fees::InfoT as _, transfer_with_dust},
	limits,
	metering::{ChargedAmount, Diff, FrameMeter, ResourceMeter, State, Token, TransactionMeter},
	precompiles::{All as AllPrecompiles, Instance as PrecompileInstance, Precompiles},
	primitives::{ExecConfig, ExecReturnValue, StorageDeposit},
	runtime_decl_for_revive_api::{Decode, Encode, TypeInfo},
	storage::{AccountIdOrAddress, AccountType, WriteOutcome},
	tracing::if_tracing,
	transient_storage::TransientStorage,
};
use alloc::{
	collections::{BTreeMap, BTreeSet, btree_map},
	vec::Vec,
};
use core::{cmp, fmt::Debug, marker::PhantomData, mem, ops::ControlFlow};
use frame_support::{
	Blake2_128Concat, BoundedVec, DebugNoBound, StorageHasher,
	crypto::ecdsa::ECDSAExt,
	defensive,
	dispatch::DispatchResult,
	ensure,
	storage::{TransactionOutcome, with_transaction},
	traits::{
		Time,
		fungible::{Balanced as _, Inspect, Mutate},
		tokens::Preservation,
	},
	weights::Weight,
};
use frame_system::{
	Pallet as System, RawOrigin,
	pallet_prelude::{BlockNumberFor, OriginFor},
};
use sp_core::{
	ConstU32, Get, H160, H256, U256,
	ecdsa::Public as ECDSAPublic,
	sr25519::{Public as SR25519Public, Signature as SR25519Signature},
};
use sp_io::{crypto::secp256k1_ecdsa_recover_compressed, hashing::blake2_256};
use sp_runtime::{
	DispatchError, SaturatedConversion,
	traits::{BadOrigin, Saturating, TrailingZeroInput, Zero},
};

#[cfg(test)]
mod tests;

#[cfg(test)]
pub mod mock_ext;

pub type AccountIdOf<T> = <T as frame_system::Config>::AccountId;
pub type MomentOf<T> = <<T as Config>::Time as Time>::Moment;
pub type ExecResult = Result<ExecReturnValue, ExecError>;

/// Type for variable sized storage key. Used for transparent hashing.
type VarSizedKey = BoundedVec<u8, ConstU32<{ limits::STORAGE_KEY_BYTES }>>;

const FRAME_ALWAYS_EXISTS_ON_INSTANTIATE: &str = "The return value is only `None` if no contract exists at the specified address. This cannot happen on instantiate or delegate; qed";

/// Code hash of existing account without code (keccak256 hash of empty data).
pub const EMPTY_CODE_HASH: H256 =
	H256(sp_core::hex2array!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"));

/// Combined key type for both fixed and variable sized storage keys.
#[derive(Debug)]
pub enum Key {
	/// Variant for fixed sized keys.
	Fix([u8; 32]),
	/// Variant for variable sized keys.
	Var(VarSizedKey),
}

impl Key {
	/// Reference to the raw unhashed key.
	pub fn unhashed(&self) -> &[u8] {
		match self {
			Key::Fix(v) => v.as_ref(),
			Key::Var(v) => v.as_ref(),
		}
	}

	/// The hashed key that has be used as actual key to the storage trie.
	pub fn hash(&self) -> Vec<u8> {
		match self {
			Key::Fix(v) => blake2_256(v.as_slice()).to_vec(),
			Key::Var(v) => Blake2_128Concat::hash(v.as_slice()),
		}
	}

	pub fn from_fixed(v: [u8; 32]) -> Self {
		Self::Fix(v)
	}

	pub fn try_from_var(v: Vec<u8>) -> Result<Self, ()> {
		VarSizedKey::try_from(v).map(Self::Var).map_err(|_| ())
	}
}

/// Level of reentrancy protection.
///
/// This needs to be specifed when a contract makes a message call. This way the calling contract
/// can specify the level of re-entrancy protection while the callee (and it's recursive callees) is
/// executing.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum ReentrancyProtection {
	/// Don't activate reentrancy protection
	AllowReentry,
	/// Activate strict reentrancy protection. The direct callee and none of its own recursive
	/// callees must be the calling contract.
	Strict,
	/// Activate reentrancy protection where the direct callee can be the same contract as the
	/// caller but none of the recursive callees of the callee must be the caller.
	///
	/// This is used for calls that transfer value but restrict gas so that the callee only has a
	/// stipend gas amount. In Ethereum that is not sufficient for the callee to make another call.
	/// However, due to gas scale differences that guarantee does not automatically hold in revive
	/// and we enforce it explicitly here.
	AllowNext,
}

/// Origin of the error.
///
/// Call or instantiate both called into other contracts and pass through errors happening
/// in those to the caller. This enum is for the caller to distinguish whether the error
/// happened during the execution of the callee or in the current execution context.
#[derive(Copy, Clone, PartialEq, Eq, Debug, codec::Decode, codec::Encode)]
pub enum ErrorOrigin {
	/// Caller error origin.
	///
	/// The error happened in the current execution context rather than in the one
	/// of the contract that is called into.
	Caller,
	/// The error happened during execution of the called contract.
	Callee,
}

/// Error returned by contract execution.
#[derive(Copy, Clone, PartialEq, Eq, Debug, codec::Decode, codec::Encode)]
pub struct ExecError {
	/// The reason why the execution failed.
	pub error: DispatchError,
	/// Origin of the error.
	pub origin: ErrorOrigin,
}

impl<T: Into<DispatchError>> From<T> for ExecError {
	fn from(error: T) -> Self {
		Self { error: error.into(), origin: ErrorOrigin::Caller }
	}
}

/// The type of origins supported by the revive pallet.
#[derive(Clone, Encode, Decode, PartialEq, TypeInfo, DebugNoBound)]
pub enum Origin<T: Config> {
	Root,
	Signed(T::AccountId),
}

impl<T: Config> Origin<T> {
	/// Creates a new Signed Caller from an AccountId.
	pub fn from_account_id(account_id: T::AccountId) -> Self {
		Origin::Signed(account_id)
	}

	/// Creates a new Origin from a `RuntimeOrigin`.
	pub fn from_runtime_origin(o: OriginFor<T>) -> Result<Self, DispatchError> {
		match o.into() {
			Ok(RawOrigin::Root) => Ok(Self::Root),
			Ok(RawOrigin::Signed(t)) => Ok(Self::Signed(t)),
			_ => Err(BadOrigin.into()),
		}
	}

	/// Returns the AccountId of a Signed Origin or an error if the origin is Root.
	pub fn account_id(&self) -> Result<&T::AccountId, DispatchError> {
		match self {
			Origin::Signed(id) => Ok(id),
			Origin::Root => Err(DispatchError::RootNotAllowed),
		}
	}

	/// Make sure that this origin is mapped.
	///
	/// We require an origin to be mapped in order to be used in a `Stack`. Otherwise
	/// [`Stack::caller`] returns an address that can't be reverted to the original address.
	fn ensure_mapped(&self) -> DispatchResult {
		match self {
			Self::Root => Ok(()),
			Self::Signed(account_id) if T::AddressMapper::is_mapped(account_id) => Ok(()),
			Self::Signed(_) => Err(<Error<T>>::AccountUnmapped.into()),
		}
	}
}

/// Argument passed by a contact to describe the amount of resources allocated to a cross contact
/// call.
#[derive(DebugNoBound)]
pub enum CallResources<T: Config> {
	/// Resources are not limited
	NoLimits,
	/// Resources encoded using their actual values.
	WeightDeposit { weight: Weight, deposit_limit: BalanceOf<T> },
	/// Resources encoded as unified ethereum gas.
	Ethereum { gas: BalanceOf<T>, add_stipend: bool },
}

impl<T: Config> CallResources<T> {
	/// Creates a new `CallResources` with weight and deposit limits.
	pub fn from_weight_and_deposit(weight: Weight, deposit_limit: U256) -> Self {
		Self::WeightDeposit {
			weight,
			deposit_limit: deposit_limit.saturated_into::<BalanceOf<T>>(),
		}
	}

	/// Creates a new `CallResources` from Ethereum gas limits.
	pub fn from_ethereum_gas(gas: U256, add_stipend: bool) -> Self {
		Self::Ethereum { gas: gas.saturated_into::<BalanceOf<T>>(), add_stipend }
	}
}

impl<T: Config> Default for CallResources<T> {
	fn default() -> Self {
		Self::WeightDeposit { weight: Default::default(), deposit_limit: Default::default() }
	}
}

/// Stored inside the `Stack` for each contract that is scheduled for termination.
struct TerminateArgs<T: Config> {
	/// Where to send the free balance of the terminated contract.
	beneficiary: T::AccountId,
	/// Triggered by the EVM opcode.
	only_if_same_tx: bool,
	/// The storage deposit already refunded when the termination was scheduled.
	refunded: BalanceOf<T>,
}

/// Environment functions only available to host functions.
pub trait Ext: PrecompileWithInfoExt {
	/// Execute code in the current frame.
	///
	/// Returns the code size of the called contract.
	fn delegate_call(
		&mut self,
		call_resources: &CallResources<Self::T>,
		address: H160,
		input_data: Vec<u8>,
	) -> Result<(), ExecError>;

	/// Register the contract for destruction at the end of the call stack.
	///
	/// Transfer all funds to `beneficiary`.
	/// Contract is deleted only if it was created in the same call stack.
	///
	/// This function will fail if called from constructor.
	fn terminate_if_same_tx(&mut self, beneficiary: &H160) -> Result<CodeRemoved, DispatchError>;

	/// Returns the code hash of the contract being executed.
	#[allow(dead_code)]
	fn own_code_hash(&mut self) -> &H256;

	/// Get the length of the immutable data.
	///
	/// This query is free as it does not need to load the immutable data from storage.
	/// Useful when we need a constant time lookup of the length.
	fn immutable_data_len(&mut self) -> u32;

	/// Returns the immutable data of the current contract.
	///
	/// Returns `Err(InvalidImmutableAccess)` if called from a constructor.
	fn get_immutable_data(&mut self) -> Result<ImmutableData, DispatchError>;

	/// Set the immutable data of the current contract.
	///
	/// Returns `Err(InvalidImmutableAccess)` if not called from a constructor.
	///
	/// Note: Requires &mut self to access the contract info.
	fn set_immutable_data(&mut self, data: ImmutableData) -> Result<(), DispatchError>;
}

/// Environment functions which are available to pre-compiles with `HAS_CONTRACT_INFO = true`.
pub trait PrecompileWithInfoExt: PrecompileExt {
	/// Instantiate a contract from the given code.
	///
	/// Returns the original code size of the called contract.
	/// The newly created account will be associated with `code`. `value` specifies the amount of
	/// value transferred from the caller to the newly created account.
	fn instantiate(
		&mut self,
		limits: &CallResources<Self::T>,
		code: Code,
		value: U256,
		input_data: Vec<u8>,
		salt: Option<&[u8; 32]>,
	) -> Result<H160, ExecError>;
}

/// Environment functions which are available to all pre-compiles.
pub trait PrecompileExt: sealing::Sealed {
	type T: Config;

	/// Charges the weight meter with the given weight.
	fn charge(&mut self, weight: Weight) -> Result<ChargedAmount, DispatchError> {
		self.frame_meter_mut().charge_weight_token(RuntimeCosts::Precompile(weight))
	}

	/// Reconcile an earlier gas charge with the actual weight consumed.
	/// This updates the current weight meter to reflect the real cost of the token.
	fn adjust_gas(&mut self, charged: ChargedAmount, actual_weight: Weight) {
		self.frame_meter_mut()
			.adjust_weight(charged, RuntimeCosts::Precompile(actual_weight));
	}

	/// Charges the weight meter with the given token or halts execution if not enough weight is
	/// left.
	#[inline]
	fn charge_or_halt<Tok: Token<Self::T>>(
		&mut self,
		token: Tok,
	) -> ControlFlow<crate::vm::evm::Halt, ChargedAmount> {
		self.frame_meter_mut().charge_or_halt(token)
	}

	/// Call (possibly transferring some amount of funds) into the specified account.
	fn call(
		&mut self,
		call_resources: &CallResources<Self::T>,
		to: &H160,
		value: U256,
		input_data: Vec<u8>,
		reentrancy: ReentrancyProtection,
		read_only: bool,
	) -> Result<(), ExecError>;

	/// Returns the transient storage entry of the executing account for the given `key`.
	///
	/// Returns `None` if the `key` wasn't previously set by `set_transient_storage` or
	/// was deleted.
	fn get_transient_storage(&self, key: &Key) -> Option<Vec<u8>>;

	/// Returns `Some(len)` (in bytes) if a transient storage item exists at `key`.
	///
	/// Returns `None` if the `key` wasn't previously set by `set_transient_storage` or
	/// was deleted.
	fn get_transient_storage_size(&self, key: &Key) -> Option<u32>;

	/// Sets the transient storage entry for the given key to the specified value. If `value` is
	/// `None` then the storage entry is deleted.
	fn set_transient_storage(
		&mut self,
		key: &Key,
		value: Option<Vec<u8>>,
		take_old: bool,
	) -> Result<WriteOutcome, DispatchError>;

	/// Returns the caller.
	fn caller(&self) -> Origin<Self::T>;

	/// Returns the caller of the caller.
	fn caller_of_caller(&self) -> Origin<Self::T>;

	/// Return the origin of the whole call stack.
	fn origin(&self) -> &Origin<Self::T>;

	/// Returns the account id for the given `address`.
	fn to_account_id(&self, address: &H160) -> AccountIdOf<Self::T>;

	/// Returns the code hash of the contract for the given `address`.
	/// If not a contract but account exists then `keccak_256([])` is returned, otherwise `zero`.
	fn code_hash(&self, address: &H160) -> H256;

	/// Returns the code size of the contract at the given `address` or zero.
	fn code_size(&self, address: &H160) -> u64;

	/// Check if the caller of the current contract is the origin of the whole call stack.
	fn caller_is_origin(&self, use_caller_of_caller: bool) -> bool;

	/// Check if the caller is origin, and this origin is root.
	fn caller_is_root(&self, use_caller_of_caller: bool) -> bool;

	/// Check if the origin of the whole call stack is root.
	///
	/// Unlike [`Self::caller_is_root`], this does not require the caller to be the origin: any
	/// number of intermediate frames may sit between this contract and the original dispatch.
	fn origin_is_root(&self) -> bool;

	/// Returns a reference to the account id of the current contract.
	fn account_id(&self) -> &AccountIdOf<Self::T>;

	/// Returns a reference to the [`H160`] address of the current contract.
	fn address(&self) -> H160 {
		<Self::T as Config>::AddressMapper::to_address(self.account_id())
	}

	/// Returns the balance of the current contract.
	///
	/// The `value_transferred` is already added.
	fn balance(&self) -> U256;

	/// Returns the balance of the supplied account.
	///
	/// The `value_transferred` is already added.
	fn balance_of(&self, address: &H160) -> U256;

	/// Returns the value transferred along with this call.
	fn value_transferred(&self) -> U256;

	/// Returns the timestamp of the current block in seconds.
	fn now(&self) -> U256;

	/// Returns the minimum balance that is required for creating an account.
	fn minimum_balance(&self) -> U256;

	/// Deposit an event with the given topics.
	///
	/// There should not be any duplicates in `topics`.
	fn deposit_event(&mut self, topics: Vec<H256>, data: Vec<u8>);

	/// Returns the current block number.
	fn block_number(&self) -> U256;

	/// Returns the block hash at the given `block_number` or `None` if
	/// `block_number` isn't within the range of the previous 256 blocks.
	fn block_hash(&self, block_number: U256) -> Option<H256>;

	/// Returns the author of the current block.
	fn block_author(&self) -> H160;

	/// Returns the block gas limit.
	fn gas_limit(&self) -> u64;

	/// Returns the chain id.
	fn chain_id(&self) -> u64;

	/// Get an immutable reference to the nested resource meter of the frame.
	#[deprecated(note = "Renamed to `frame_meter`; this alias will be removed in future versions")]
	fn gas_meter(&self) -> &FrameMeter<Self::T>;

	/// Get a mutable reference to the nested resource meter of the frame.
	#[deprecated(
		note = "Renamed to `frame_meter_mut`; this alias will be removed in future versions"
	)]
	fn gas_meter_mut(&mut self) -> &mut FrameMeter<Self::T>;

	/// Get an immutable reference to the nested resource meter of the frame.
	fn frame_meter(&self) -> &FrameMeter<Self::T>;

	/// Get a mutable reference to the nested resource meter of the frame.
	fn frame_meter_mut(&mut self) -> &mut FrameMeter<Self::T>;

	/// Recovers ECDSA compressed public key based on signature and message hash.
	fn ecdsa_recover(&self, signature: &[u8; 65], message_hash: &[u8; 32]) -> Result<[u8; 33], ()>;

	/// Verify a sr25519 signature.
	fn sr25519_verify(&self, signature: &[u8; 64], message: &[u8], pub_key: &[u8; 32]) -> bool;

	/// Returns Ethereum address from the ECDSA compressed public key.
	fn ecdsa_to_eth_address(&self, pk: &[u8; 33]) -> Result<[u8; 20], DispatchError>;

	/// Tests sometimes need to modify and inspect the contract info directly.
	#[cfg(any(test, feature = "runtime-benchmarks"))]
	fn contract_info(&mut self) -> &mut ContractInfo<Self::T>;

	/// Get a mutable reference to the transient storage.
	/// Useful in benchmarks when it is sometimes necessary to modify and inspect the transient
	/// storage directly.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn transient_storage(&mut self) -> &mut TransientStorage<Self::T>;

	/// Check if running in read-only context.
	fn is_read_only(&self) -> bool;

	/// Check if running as a delegate call.
	fn is_delegate_call(&self) -> bool;

	/// Returns an immutable reference to the output of the last executed call frame.
	fn last_frame_output(&self) -> &ExecReturnValue;

	/// Returns a mutable reference to the output of the last executed call frame.
	fn last_frame_output_mut(&mut self) -> &mut ExecReturnValue;

	/// Copies a slice of the contract's code at `address` into the provided buffer.
	///
	/// EVM CODECOPY semantics:
	/// - If `buf.len()` = 0: Nothing happens
	/// - If `code_offset` >= code size: `len` bytes of zero are written to memory
	/// - If `code_offset + buf.len()` extends beyond code: Available code copied, remaining bytes
	///   are filled with zeros
	fn copy_code_slice(&mut self, buf: &mut [u8], address: &H160, code_offset: usize);

	/// Register the caller of the current contract for destruction.
	/// Destruction happens at the end of the call stack.
	/// This is supposed to be used by the terminate precompile.
	///
	/// Transfer all funds to `beneficiary`.
	/// Contract is deleted at the end of the call stack.
	///
	/// This function will fail if called from constructor.
	fn terminate_caller(&mut self, beneficiary: &H160) -> Result<(), DispatchError>;

	/// Returns the effective gas price of this transaction.
	fn effective_gas_price(&self) -> U256;

	/// The amount of gas left in eth gas units.
	fn gas_left(&self) -> u64;

	/// Returns the storage entry of the executing account by the given `key`.
	///
	/// Returns `None` if the `key` wasn't previously set by `set_storage` or
	/// was deleted.
	fn get_storage(&mut self, key: &Key) -> Option<Vec<u8>>;

	/// Returns `Some(len)` (in bytes) if a storage item exists at `key`.
	///
	/// Returns `None` if the `key` wasn't previously set by `set_storage` or
	/// was deleted.
	fn get_storage_size(&mut self, key: &Key) -> Option<u32>;

	/// Sets the storage entry by the given key to the specified value. If `value` is `None` then
	/// the storage entry is deleted.
	fn set_storage(
		&mut self,
		key: &Key,
		value: Option<Vec<u8>>,
		take_old: bool,
	) -> Result<WriteOutcome, DispatchError>;

	/// Checks if `key` was already accessed in this transaction and inserts it
	/// otherwise, so subsequent accesses to the same slot bill as hot. Returns
	/// the slot's [`StorageAccessKind`]. `op` is the operation being performed:
	/// a write upgrades a slot that had only paid for a read. When
	/// `transient` is true, skips the access list and returns the `Transient`
	/// variant.
	fn touch_storage_access(
		&mut self,
		transient: bool,
		key: &Key,
		op: StorageOp,
	) -> StorageAccessKind;

	/// Non-mutating sibling of `touch_storage_access`: prices the access without
	/// warming the slot.
	fn peek_storage_access(&self, transient: bool, key: &Key) -> StorageAccessKind;

	/// Charges `diff` from the meter.
	fn charge_storage(&mut self, diff: &Diff) -> DispatchResult;
}

/// Describes the different functions that can be exported by an [`Executable`].
#[derive(
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	codec::Decode,
	codec::Encode,
	codec::MaxEncodedLen,
	scale_info::TypeInfo,
)]
pub enum ExportedFunction {
	/// The constructor function which is executed on deployment of a contract.
	Constructor,
	/// The function which is executed when a contract is called.
	Call,
}

/// A trait that represents something that can be executed.
///
/// In the on-chain environment this would be represented by a vm binary module. This trait exists
/// in order to be able to mock the vm logic for testing.
pub trait Executable<T: Config>: Sized {
	/// Load the executable from storage.
	///
	/// # Note
	/// Charges size base load weight from the weight meter.
	fn from_storage<S: State>(
		code_hash: H256,
		meter: &mut ResourceMeter<T, S>,
	) -> Result<Self, DispatchError>;

	/// Load the executable from EVM bytecode
	fn from_evm_init_code(code: Vec<u8>, owner: AccountIdOf<T>) -> Result<Self, DispatchError>;

	/// Execute the specified exported function and return the result.
	///
	/// When the specified function is `Constructor` the executable is stored and its
	/// refcount incremented.
	///
	/// # Note
	///
	/// This functions expects to be executed in a storage transaction that rolls back
	/// all of its emitted storage changes.
	fn execute<E: Ext<T = T>>(
		self,
		ext: &mut E,
		function: ExportedFunction,
		input_data: Vec<u8>,
	) -> ExecResult;

	/// The code info of the executable.
	fn code_info(&self) -> &CodeInfo<T>;

	/// The raw code of the executable.
	fn code(&self) -> &[u8];

	/// The code hash of the executable.
	fn code_hash(&self) -> &H256;
}

/// The complete call stack of a contract execution.
///
/// The call stack is initiated by either a signed origin or one of the contract RPC calls.
/// This type implements `Ext` and by that exposes the business logic of contract execution to
/// the runtime module which interfaces with the contract (the vm contract blob) itself.
pub struct Stack<'a, T: Config, E> {
	/// The origin that initiated the call stack. It could either be a Signed plain account that
	/// holds an account id or Root.
	///
	/// # Note
	///
	/// Please note that it is possible that the id of a Signed origin belongs to a contract rather
	/// than a plain account when being called through one of the contract RPCs where the
	/// client can freely choose the origin. This usually makes no sense but is still possible.
	origin: Origin<T>,
	/// The resource meter that tracks all resource usage before the first frame starts.
	transaction_meter: &'a mut TransactionMeter<T>,
	/// The timestamp at the point of call stack instantiation.
	timestamp: MomentOf<T>,
	/// The block number at the time of call stack instantiation.
	block_number: BlockNumberFor<T>,
	/// The actual call stack. One entry per nested contract called/instantiated.
	/// This does **not** include the [`Self::first_frame`].
	frames: BoundedVec<Frame<T>, ConstU32<{ limits::CALL_STACK_DEPTH }>>,
	/// Statically guarantee that each call stack has at least one frame.
	first_frame: Frame<T>,
	/// Transient storage used to store data, which is kept for the duration of a transaction.
	transient_storage: TransientStorage<T>,
	/// Per-transaction cold/hot access list for storage slots (EIP-2929 style).
	access_list: AccessList,
	/// Global behavior determined by the creater of this stack.
	exec_config: &'a ExecConfig<T>,
	/// No executable is held by the struct but influences its behaviour.
	_phantom: PhantomData<E>,
}

/// Represents one entry in the call stack.
///
/// For each nested contract call or instantiate one frame is created. It holds specific
/// information for the said call and caches the in-storage `ContractInfo` data structure.
struct Frame<T: Config> {
	/// The address of the executing contract.
	account_id: T::AccountId,
	/// The cached in-storage data of the contract.
	contract_info: CachedContract<T>,
	/// The EVM balance transferred by the caller as part of the call.
	value_transferred: U256,
	/// Determines whether this is a call or instantiate frame.
	entry_point: ExportedFunction,
	/// The resource meter that tracks all resource usage of this frame.
	frame_meter: FrameMeter<T>,
	/// If `false` the contract enabled its defense against reentrance attacks.
	allows_reentry: bool,
	/// If `true` subsequent calls cannot modify storage.
	read_only: bool,
	/// The delegate call info of the currently executing frame which was spawned by
	/// `delegate_call`.
	delegate: Option<DelegateInfo<T>>,
	/// The output of the last executed call frame.
	last_frame_output: ExecReturnValue,
	/// The set of contracts that were created during this call stack.
	contracts_created: BTreeSet<T::AccountId>,
	/// The set of contracts that are registered for destruction at the end of this call stack.
	contracts_to_be_destroyed: BTreeMap<T::AccountId, TerminateArgs<T>>,
}

/// This structure is used to represent the arguments in a delegate call frame in order to
/// distinguish who delegated the call and where it was delegated to.
#[derive(Clone, DebugNoBound)]
pub struct DelegateInfo<T: Config> {
	/// The caller of the contract.
	pub caller: Origin<T>,
	/// The address of the contract the call was delegated to.
	pub callee: H160,
}

/// When calling an address it can either lead to execution of contract code or a pre-compile.
enum ExecutableOrPrecompile<T: Config, E: Executable<T>, Env> {
	/// Contract code.
	Executable(E),
	/// Code inside the runtime (so called pre-compile).
	Precompile { instance: PrecompileInstance<Env>, _phantom: PhantomData<T> },
}

impl<T: Config, E: Executable<T>, Env> ExecutableOrPrecompile<T, E, Env> {
	fn as_executable(&self) -> Option<&E> {
		if let Self::Executable(executable) = self { Some(executable) } else { None }
	}

	fn is_pvm(&self) -> bool {
		match self {
			Self::Executable(e) => e.code_info().is_pvm(),
			_ => false,
		}
	}

	fn as_precompile(&self) -> Option<&PrecompileInstance<Env>> {
		if let Self::Precompile { instance, .. } = self { Some(instance) } else { None }
	}

	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn into_executable(self) -> Option<E> {
		if let Self::Executable(executable) = self { Some(executable) } else { None }
	}
}

/// Parameter passed in when creating a new `Frame`.
///
/// It determines whether the new frame is for a call or an instantiate.
enum FrameArgs<'a, T: Config, E> {
	Call {
		/// The account id of the contract that is to be called.
		dest: T::AccountId,
		/// If `None` the contract info needs to be reloaded from storage.
		cached_info: Option<ContractInfo<T>>,
		/// This frame was created by `seal_delegate_call` and hence uses different code than
		/// what is stored at [`Self::Call::dest`]. Its caller ([`DelegatedCall::caller`]) is the
		/// account which called the caller contract
		delegated_call: Option<DelegateInfo<T>>,
	},
	Instantiate {
		/// The contract or signed origin which instantiates the new contract.
		sender: T::AccountId,
		/// The executable whose `deploy` function is run.
		executable: E,
		/// A salt used in the contract address derivation of the new contract.
		salt: Option<&'a [u8; 32]>,
		/// The input data is used in the contract address derivation of the new contract.
		input_data: &'a [u8],
	},
}

/// Describes the different states of a contract as contained in a `Frame`.
enum CachedContract<T: Config> {
	/// The cached contract is up to date with the in-storage value.
	Cached(ContractInfo<T>),
	/// A recursive call into the same contract did write to the contract info.
	///
	/// In this case the cached contract is stale and needs to be reloaded from storage.
	Invalidated,
	/// The frame is associated with pre-compile that has no contract info.
	None,
}

impl<T: Config> Frame<T> {
	/// Return the `contract_info` of the current contract.
	fn contract_info(&mut self) -> &mut ContractInfo<T> {
		self.contract_info.get(&self.account_id)
	}

	/// Schedule `account_id` for termination at the end of the call stack.
	///
	/// A contract can be scheduled more than once, for example by `System.terminate` followed by
	/// `SELFDESTRUCT`. A later termination must never downgrade an earlier one: once any of them
	/// is unconditional the contract is deleted, since `System.terminate` already refunded the
	/// deposit when it was called. The refunds add up, so the transaction meter sees each one.
	/// The `beneficiary` comes from the later termination, since it is the one the contract named
	/// last.
	fn schedule_termination(&mut self, account_id: T::AccountId, args: TerminateArgs<T>) {
		match self.contracts_to_be_destroyed.entry(account_id) {
			btree_map::Entry::Occupied(mut entry) => {
				let old = entry.get();
				let only_if_same_tx = old.only_if_same_tx && args.only_if_same_tx;
				let refunded = old.refunded.saturating_add(args.refunded);
				entry.insert(TerminateArgs { only_if_same_tx, refunded, ..args });
			},
			btree_map::Entry::Vacant(entry) => {
				entry.insert(args);
			},
		}
	}
}

/// Extract the contract info after loading it from storage.
///
/// This assumes that `load` was executed before calling this macro.
macro_rules! get_cached_or_panic_after_load {
	($c:expr) => {{
		if let CachedContract::Cached(contract) = $c {
			contract
		} else {
			panic!(
				"It is impossible to remove a contract that is on the call stack;\
				See implementations of terminate;\
				Therefore fetching a contract will never fail while using an account id
				that is currently active on the call stack;\
				qed"
			);
		}
	}};
}

/// Same as [`Stack::top_frame`].
///
/// We need this access as a macro because sometimes hiding the lifetimes behind
/// a function won't work out.
macro_rules! top_frame {
	($stack:expr) => {
		$stack.frames.last().unwrap_or(&$stack.first_frame)
	};
}

/// Same as [`Stack::top_frame_mut`].
///
/// We need this access as a macro because sometimes hiding the lifetimes behind
/// a function won't work out.
macro_rules! top_frame_mut {
	($stack:expr) => {
		$stack.frames.last_mut().unwrap_or(&mut $stack.first_frame)
	};
}

impl<T: Config> CachedContract<T> {
	/// Return `Some(ContractInfo)` if the contract is in cached state. `None` otherwise.
	fn into_contract(self) -> Option<ContractInfo<T>> {
		if let CachedContract::Cached(contract) = self { Some(contract) } else { None }
	}

	/// Return `Some(&mut ContractInfo)` if the contract is in cached state. `None` otherwise.
	fn as_contract(&mut self) -> Option<&mut ContractInfo<T>> {
		if let CachedContract::Cached(contract) = self { Some(contract) } else { None }
	}

	/// Load the `contract_info` from storage if necessary.
	fn load(&mut self, account_id: &T::AccountId) {
		if let CachedContract::Invalidated = self &&
			let Some(contract) =
				AccountInfo::<T>::load_contract(&T::AddressMapper::to_address(account_id))
		{
			*self = CachedContract::Cached(contract);
		}
	}

	/// Return the cached contract_info.
	fn get(&mut self, account_id: &T::AccountId) -> &mut ContractInfo<T> {
		self.load(account_id);
		get_cached_or_panic_after_load!(self)
	}

	/// Set the status to invalidate if is cached.
	fn invalidate(&mut self) {
		if matches!(self, CachedContract::Cached(_)) {
			*self = CachedContract::Invalidated;
		}
	}
}

impl<'a, T, E> Stack<'a, T, E>
where
	T: Config,
	E: Executable<T>,
{
	/// Create and run a new call stack by calling into `dest`.
	///
	/// # Return Value
	///
	/// Result<(ExecReturnValue, CodeSize), (ExecError, CodeSize)>
	pub fn run_call(
		origin: Origin<T>,
		dest: H160,
		transaction_meter: &'a mut TransactionMeter<T>,
		value: U256,
		input_data: Vec<u8>,
		exec_config: &ExecConfig<T>,
	) -> ExecResult {
		let dest = T::AddressMapper::to_account_id(&dest);
		if let Some((mut stack, executable)) = Stack::<'_, T, E>::new(
			FrameArgs::Call { dest: dest.clone(), cached_info: None, delegated_call: None },
			origin.clone(),
			transaction_meter,
			value,
			exec_config,
			&input_data,
		)? {
			stack.run(executable, input_data).map(|_| stack.first_frame.last_frame_output)
		} else {
			if_tracing(|t| {
				t.enter_child_span(
					origin.account_id().map(T::AddressMapper::to_address).unwrap_or_default(),
					T::AddressMapper::to_address(&dest),
					None,
					false,
					value,
					&input_data,
					Default::default(),
				);
			});

			let result = if let Some(mock_answer) =
				exec_config.mock_handler.as_ref().and_then(|handler| {
					handler.mock_call(T::AddressMapper::to_address(&dest), &input_data, value)
				}) {
				Ok(mock_answer)
			} else {
				Self::transfer_from_origin(
					&origin,
					&origin,
					&dest,
					value,
					transaction_meter,
					exec_config,
				)
			};

			if_tracing(|t| {
				let gas_used =
					transaction_meter.total_consumed_gas().try_into().unwrap_or(u64::MAX);
				let weight_consumed = transaction_meter.weight_consumed();
				match result {
					Ok(ref output) => t.exit_child_span(&output, gas_used, weight_consumed),
					Err(e) => {
						t.exit_child_span_with_error(e.error.into(), gas_used, weight_consumed)
					},
				}
			});

			log::trace!(target: LOG_TARGET, "call finished with: {result:?}");

			result
		}
	}

	/// Create and run a new call stack by instantiating a new contract.
	///
	/// # Return Value
	///
	/// Result<(NewContractAccountId, ExecReturnValue), ExecError)>
	pub fn run_instantiate(
		origin: T::AccountId,
		executable: E,
		transaction_meter: &'a mut TransactionMeter<T>,
		value: U256,
		input_data: Vec<u8>,
		salt: Option<&[u8; 32]>,
		exec_config: &ExecConfig<T>,
	) -> Result<(H160, ExecReturnValue), ExecError> {
		let deployer = T::AddressMapper::to_address(&origin);
		let (mut stack, executable) = Stack::<'_, T, E>::new(
			FrameArgs::Instantiate {
				sender: origin.clone(),
				executable,
				salt,
				input_data: input_data.as_ref(),
			},
			Origin::from_account_id(origin),
			transaction_meter,
			value,
			exec_config,
			&input_data,
		)?
		.expect(FRAME_ALWAYS_EXISTS_ON_INSTANTIATE);
		let address = T::AddressMapper::to_address(&stack.top_frame().account_id);
		let result = stack
			.run(executable, input_data)
			.map(|_| (address, stack.first_frame.last_frame_output));
		if let Ok((contract, output)) = &result &&
			!output.did_revert()
		{
			Contracts::<T>::deposit_event(Event::Instantiated { deployer, contract: *contract });
		}
		log::trace!(target: LOG_TARGET, "instantiate finished with: {result:?}");
		result
	}

	#[cfg(any(feature = "runtime-benchmarks", test))]
	pub fn bench_new_call(
		dest: H160,
		origin: Origin<T>,
		transaction_meter: &'a mut TransactionMeter<T>,
		value: BalanceOf<T>,
		exec_config: &'a ExecConfig<T>,
		read_only: bool,
		delegate_call: bool,
	) -> (Self, E) {
		let call = Self::new(
			FrameArgs::Call {
				dest: T::AddressMapper::to_account_id(&dest),
				cached_info: None,
				delegated_call: None,
			},
			origin,
			transaction_meter,
			value.into(),
			exec_config,
			&Default::default(),
		)
		.unwrap()
		.unwrap();
		let mut stack = call.0;
		if read_only {
			stack.top_frame_mut().read_only = true;
		}
		if delegate_call {
			let frame = stack.top_frame_mut();
			frame.delegate = Some(DelegateInfo {
				caller: Origin::from_account_id(frame.account_id.clone()),
				callee: H160::zero(),
			});
		}
		(stack, call.1.into_executable().unwrap())
	}

	/// Create a new call stack.
	///
	/// Returns `None` when calling a non existent contract. This is not an error case
	/// since this will result in a value transfer.
	fn new(
		args: FrameArgs<T, E>,
		origin: Origin<T>,
		transaction_meter: &'a mut TransactionMeter<T>,
		value: U256,
		exec_config: &'a ExecConfig<T>,
		input_data: &Vec<u8>,
	) -> Result<Option<(Self, ExecutableOrPrecompile<T, E, Self>)>, ExecError> {
		origin.ensure_mapped()?;
		let Some((first_frame, executable)) = Self::new_frame(
			args,
			value,
			transaction_meter,
			&CallResources::NoLimits,
			false,
			true,
			input_data,
			exec_config,
		)?
		else {
			return Ok(None);
		};

		let mut timestamp = T::Time::now();
		let mut block_number = <frame_system::Pallet<T>>::block_number();
		// if dry run with timestamp override is provided we simulate the run in a `pending` block
		if let Some(timestamp_override) =
			exec_config.is_dry_run.as_ref().and_then(|cfg| cfg.timestamp_override)
		{
			block_number = block_number.saturating_add(1u32.into());
			// Delta is in milliseconds; increment timestamp by one second
			let delta = 1000u32.into();
			timestamp = cmp::max(timestamp.saturating_add(delta), timestamp_override);
		}

		let stack = Self {
			origin,
			transaction_meter,
			timestamp,
			block_number,
			first_frame,
			frames: Default::default(),
			transient_storage: TransientStorage::new(limits::TRANSIENT_STORAGE_BYTES),
			access_list: AccessList::new(),
			exec_config,
			_phantom: Default::default(),
		};
		Ok(Some((stack, executable)))
	}

	/// Construct a new frame.
	///
	/// This does not take `self` because when constructing the first frame `self` is
	/// not initialized, yet.
	fn new_frame<S: State>(
		frame_args: FrameArgs<T, E>,
		value_transferred: U256,
		meter: &mut ResourceMeter<T, S>,
		call_resources: &CallResources<T>,
		read_only: bool,
		origin_is_caller: bool,
		input_data: &[u8],
		exec_config: &ExecConfig<T>,
	) -> Result<Option<(Frame<T>, ExecutableOrPrecompile<T, E, Self>)>, ExecError> {
		let (account_id, contract_info, executable, delegate, entry_point) = match frame_args {
			FrameArgs::Call { dest, cached_info, delegated_call } => {
				let address = T::AddressMapper::to_address(&dest);
				let precompile = <AllPrecompiles<T>>::get(address.as_fixed_bytes());

				// which contract info to load is unaffected by the fact if this
				// is a delegate call or not
				let mut contract = match (cached_info, &precompile) {
					(Some(info), _) => CachedContract::Cached(info),
					(None, None) => {
						if let Some(info) = AccountInfo::<T>::load_contract(&address) {
							CachedContract::Cached(info)
						} else {
							return Ok(None);
						}
					},
					(None, Some(precompile)) if precompile.has_contract_info() => {
						log::trace!(target: LOG_TARGET, "found precompile for address {address:?}");
						if let Some(info) = AccountInfo::<T>::load_contract(&address) {
							CachedContract::Cached(info)
						} else {
							let info = ContractInfo::new(&address, 0u32.into(), H256::zero())?;
							CachedContract::Cached(info)
						}
					},
					(None, Some(_)) => CachedContract::None,
				};

				let delegated_call = delegated_call.or_else(|| {
					exec_config.mock_handler.as_ref().and_then(|mock_handler| {
						mock_handler.mock_delegated_caller(address, input_data)
					})
				});
				// in case of delegate the executable is not the one at `address`
				let executable = if let Some(delegated_call) = &delegated_call {
					if let Some(precompile) =
						<AllPrecompiles<T>>::get(delegated_call.callee.as_fixed_bytes())
					{
						ExecutableOrPrecompile::Precompile {
							instance: precompile,
							_phantom: Default::default(),
						}
					} else {
						let Some(info) = AccountInfo::<T>::load_contract(&delegated_call.callee)
						else {
							return Ok(None);
						};
						let executable = E::from_storage(info.code_hash, meter)?;
						ExecutableOrPrecompile::Executable(executable)
					}
				} else {
					if let Some(precompile) = precompile {
						ExecutableOrPrecompile::Precompile {
							instance: precompile,
							_phantom: Default::default(),
						}
					} else {
						let executable = E::from_storage(
							contract
								.as_contract()
								.expect("When not a precompile the contract was loaded above; qed")
								.code_hash,
							meter,
						)?;
						ExecutableOrPrecompile::Executable(executable)
					}
				};

				(dest, contract, executable, delegated_call, ExportedFunction::Call)
			},
			FrameArgs::Instantiate { sender, executable, salt, input_data } => {
				let deployer = T::AddressMapper::to_address(&sender);
				let account_nonce = <System<T>>::account_nonce(&sender);
				let address = if let Some(salt) = salt {
					address::create2(&deployer, executable.code(), input_data, salt)
				} else {
					use sp_runtime::Saturating;
					address::create1(
						&deployer,
						// the Nonce from the origin has been incremented pre-dispatch, so we
						// need to subtract 1 to get the nonce at the time of the call.
						if origin_is_caller {
							account_nonce.saturating_sub(1u32.into()).saturated_into()
						} else {
							account_nonce.saturated_into()
						},
					)
				};
				let contract = ContractInfo::new(
					&address,
					<System<T>>::account_nonce(&sender),
					*executable.code_hash(),
				)?;
				(
					T::AddressMapper::to_fallback_account_id(&address),
					CachedContract::Cached(contract),
					ExecutableOrPrecompile::Executable(executable),
					None,
					ExportedFunction::Constructor,
				)
			},
		};

		// A constructor frame creates its contract in this transaction. Marking it here covers
		// the first frame of `run_instantiate` as well as nested instantiations.
		let mut contracts_created = BTreeSet::new();
		if entry_point == ExportedFunction::Constructor {
			contracts_created.insert(account_id.clone());
		}

		let frame = Frame {
			delegate,
			value_transferred,
			contract_info,
			account_id,
			entry_point,
			frame_meter: meter.new_nested(call_resources)?,
			allows_reentry: true,
			read_only,
			last_frame_output: Default::default(),
			contracts_created,
			contracts_to_be_destroyed: Default::default(),
		};

		Ok(Some((frame, executable)))
	}

	/// Create a subsequent nested frame.
	fn push_frame(
		&mut self,
		frame_args: FrameArgs<T, E>,
		value_transferred: U256,
		call_resources: &CallResources<T>,
		read_only: bool,
		input_data: &[u8],
	) -> Result<Option<ExecutableOrPrecompile<T, E, Self>>, ExecError> {
		if self.frames.len() as u32 == limits::CALL_STACK_DEPTH {
			return Err(Error::<T>::MaxCallDepthReached.into());
		}

		// We need to make sure that changes made to the contract info are not discarded.
		// See the `in_memory_changes_not_discarded` test for more information.
		// We do not store on instantiate because we do not allow to call into a contract
		// from its own constructor.
		//
		// Additionally, we need to apply pending storage changes to the ContractInfo before
		// saving it, so that child frames can correctly calculate storage deposit refunds.
		// See: <https://github.com/paritytech/contract-issues/issues/213>
		let frame = self.top_frame();
		if let (CachedContract::Cached(contract), ExportedFunction::Call) =
			(&frame.contract_info, frame.entry_point)
		{
			let mut contract_with_pending_changes = contract.clone();
			frame
				.frame_meter
				.apply_pending_storage_changes(&mut contract_with_pending_changes);
			AccountInfo::<T>::insert_contract(
				&T::AddressMapper::to_address(&frame.account_id),
				contract_with_pending_changes,
			);
		}

		let frame = top_frame_mut!(self);
		let meter = &mut frame.frame_meter;
		if let Some((frame, executable)) = Self::new_frame(
			frame_args,
			value_transferred,
			meter,
			call_resources,
			read_only,
			false,
			input_data,
			self.exec_config,
		)? {
			// EIP-684: an in-construction address is not in `AccountInfoOf` yet, so the
			// `is_contract` guard in `ContractInfo::new` misses this re-entrant collision.
			if frame.entry_point == ExportedFunction::Constructor &&
				self.frames().any(|f| {
					f.entry_point == ExportedFunction::Constructor &&
						f.account_id == frame.account_id
				}) {
				return Err(Error::<T>::DuplicateContract.into());
			}
			self.frames.try_push(frame).map_err(|_| Error::<T>::MaxCallDepthReached)?;
			Ok(Some(executable))
		} else {
			Ok(None)
		}
	}

	/// Run the current (top) frame.
	///
	/// This can be either a call or an instantiate.
	fn run(
		&mut self,
		executable: ExecutableOrPrecompile<T, E, Self>,
		input_data: Vec<u8>,
	) -> Result<(), ExecError> {
		let frame = self.top_frame();
		let entry_point = frame.entry_point;
		let is_pvm = executable.is_pvm();

		if_tracing(|tracer| {
			// For DELEGATECALL, `from` is the contract making the delegatecall and
			// `to` is the target contract whose code is being executed.
			let (from, to) = match frame.delegate.as_ref() {
				Some(delegate) => {
					(T::AddressMapper::to_address(&frame.account_id), delegate.callee)
				},
				None => (
					self.caller()
						.account_id()
						.map(T::AddressMapper::to_address)
						.unwrap_or_default(),
					T::AddressMapper::to_address(&frame.account_id),
				),
			};
			tracer.enter_child_span(
				from,
				to,
				frame.delegate.as_ref().map(|delegate| delegate.callee),
				frame.read_only,
				frame.value_transferred,
				&input_data,
				frame
					.frame_meter
					.eth_gas_left()
					.unwrap_or_default()
					.try_into()
					.unwrap_or_default(),
			);
		});
		let mock_answer = self.exec_config.mock_handler.as_ref().and_then(|handler| {
			handler.mock_call(
				frame
					.delegate
					.as_ref()
					.map(|delegate| delegate.callee)
					.unwrap_or(T::AddressMapper::to_address(&frame.account_id)),
				&input_data,
				frame.value_transferred,
			)
		});
		// The output of the caller frame will be replaced by the output of this run.
		// It is also not accessible from nested frames.
		// Hence we drop it early to save the memory.
		let frames_len = self.frames.len();
		if let Some(caller_frame) = match frames_len {
			0 => None,
			1 => Some(&mut self.first_frame.last_frame_output),
			_ => self.frames.get_mut(frames_len - 2).map(|frame| &mut frame.last_frame_output),
		} {
			*caller_frame = Default::default();
		}

		self.with_transient_storage_mut(|transient_storage| {
			transient_storage.start_transaction();
		});
		let is_first_frame = self.frames.is_empty();
		let access_list_checkpoints_len = self.access_list.frame_depth();
		// Open an access-list frame for nested CALL/CREATE. The first frame
		// is skipped; its touches land in the bare journal and persist
		// for the whole transaction.
		if !is_first_frame {
			self.access_list.enter_frame();
		}

		let do_transaction = || -> ExecResult {
			let caller = self.caller();
			let bump_nonce = self.exec_config.bump_nonce;
			let frame = top_frame_mut!(self);
			let account_id = &frame.account_id.clone();

			if u32::try_from(input_data.len())
				.map(|len| len > limits::CALLDATA_BYTES)
				.unwrap_or(true)
			{
				Err(<Error<T>>::CallDataTooLarge)?;
			}

			// We need to make sure that the contract's account exists before calling its
			// constructor.
			if entry_point == ExportedFunction::Constructor {
				if !frame_system::Pallet::<T>::account_exists(&account_id) {
					T::Deposit::init_contract(account_id)?;
				}

				// A consumer is added at account creation and removed it on termination, otherwise
				// the runtime could remove the account. As long as a contract exists its
				// account must exist. With the consumer, a correct runtime cannot remove the
				// account.
				<System<T>>::inc_consumers(account_id)?;

				// Contracts nonce starts at 1
				<System<T>>::inc_account_nonce(account_id);

				if bump_nonce || !is_first_frame {
					// Needs to be incremented before calling into the code so that it is visible
					// in case of recursion.
					<System<T>>::inc_account_nonce(caller.account_id()?);
				}
				// The incremented refcount should be visible to the constructor.
				if is_pvm {
					<CodeInfo<T>>::increment_refcount(
						*executable
							.as_executable()
							.expect("Precompiles cannot be instantiated; qed")
							.code_hash(),
					)?;
				}
			}

			// Every non delegate call or instantiate also optionally transfers the balance.
			// If it is a delegate call, then we've already transferred tokens in the
			// last non-delegate frame.
			if frame.delegate.is_none() {
				Self::transfer_from_origin(
					&self.origin,
					&caller,
					account_id,
					frame.value_transferred,
					&mut frame.frame_meter,
					self.exec_config,
				)?;
			}

			// We need to make sure that the pre-compiles contract exist before executing it.
			// A few more conditionals:
			// 	- Only contracts with extended API (has_contract_info) are guaranteed to have an
			//    account.
			//  - Only when not delegate calling we are executing in the context of the pre-compile.
			//    Pre-compiles itself cannot delegate call.
			if let Some(precompile) = executable.as_precompile() &&
				precompile.has_contract_info() &&
				frame.delegate.is_none() &&
				!<System<T>>::account_exists(account_id)
			{
				// prefix matching pre-compiles cannot have a contract info
				// hence we only mint once per pre-compile
				T::Currency::mint_into(account_id, T::Currency::minimum_balance())?;
				// make sure the pre-compile does not destroy its account by accident
				<System<T>>::inc_consumers(account_id)?;
			}

			let mut code_deposit = executable
				.as_executable()
				.map(|exec| exec.code_info().deposit())
				.unwrap_or_default();

			let mut output = match executable {
				ExecutableOrPrecompile::Executable(executable) => {
					executable.execute(self, entry_point, input_data)
				},
				ExecutableOrPrecompile::Precompile { instance, .. } => {
					instance.call(input_data, self)
				},
			}
			.and_then(|output| {
				if u32::try_from(output.data.len())
					.map(|len| len > limits::CALLDATA_BYTES)
					.unwrap_or(true)
				{
					Err(<Error<T>>::ReturnDataTooLarge)?;
				}
				Ok(output)
			})
			.map_err(|e| ExecError { error: e.error, origin: ErrorOrigin::Callee })?;

			// Avoid useless work that would be reverted anyways.
			if output.did_revert() {
				return Ok(output);
			}

			// The deposit we charge for a contract depends on the size of the immutable data.
			// Hence we need to delay charging the base deposit after execution.
			let frame = if entry_point == ExportedFunction::Constructor {
				let frame = top_frame_mut!(self);
				// if we are dealing with EVM bytecode
				// We upload the new runtime code, and update the code
				if !is_pvm {
					// Only keep return data for tracing and for dry runs.
					// When a dry-run simulates contract deployment, keep the execution result's
					// data.
					let data = if crate::tracing::if_tracing(|_| {}).is_none() &&
						self.exec_config.is_dry_run.is_none()
					{
						core::mem::replace(&mut output.data, Default::default())
					} else {
						output.data.clone()
					};

					// Under Root there is no origin account to attribute the upload
					// deposit to: use the pallet's own account as a sentinel owner
					// with zero deposit so charge/refund are no-ops.
					let mut module = match &self.origin {
						Origin::Signed(o) => {
							crate::ContractBlob::<T>::from_evm_runtime_code(data, o.clone())?
						},
						Origin::Root => {
							crate::ContractBlob::<T>::from_evm_runtime_code_with_deposit(
								data,
								crate::Pallet::<T>::account_id(),
								Zero::zero(),
							)?
						},
					};
					module.store_code(&self.exec_config, &mut frame.frame_meter)?;
					code_deposit = module.code_info().deposit();

					let code_hash = *module.code_hash();
					frame.contract_info().code_hash = code_hash;
					<CodeInfo<T>>::increment_refcount(code_hash)?;
				}

				let deposit = frame.contract_info().update_base_deposit(code_deposit);
				frame.frame_meter.charge_contract_deposit_and_transfer(
					frame.account_id.clone(),
					StorageDeposit::Charge(deposit),
				)?;
				frame
			} else {
				self.top_frame_mut()
			};

			// The storage deposit is only charged at the end of every call stack.
			// To make sure that no sub call uses more than it is allowed to,
			// the limit is manually enforced here.
			let contract = frame.contract_info.as_contract();
			frame
				.frame_meter
				.finalize(contract)
				.map_err(|e| ExecError { error: e, origin: ErrorOrigin::Callee })?;

			Ok(output)
		};

		// All changes performed by the contract are executed under a storage transaction.
		// This allows for roll back on error. Changes to the cached contract_info are
		// committed or rolled back when popping the frame.
		//
		// `with_transactional` may return an error caused by a limit in the
		// transactional storage depth.
		let transaction_outcome =
			with_transaction(|| -> TransactionOutcome<Result<_, DispatchError>> {
				let output = if let Some(mock_answer) = mock_answer {
					Ok(mock_answer)
				} else {
					do_transaction()
				};
				match &output {
					Ok(result) if !result.did_revert() => {
						TransactionOutcome::Commit(Ok((true, output)))
					},
					_ => TransactionOutcome::Rollback(Ok((false, output))),
				}
			});

		let (success, output) = match transaction_outcome {
			// `with_transactional` executed successfully, and we have the expected output.
			Ok((success, output)) => {
				if_tracing(|tracer| {
					let frame_meter = &top_frame!(self).frame_meter;

					// we treat the initial frame meter differently to address
					// https://github.com/paritytech/polkadot-sdk/issues/8362
					let gas_consumed = if is_first_frame {
						frame_meter.total_consumed_gas()
					} else {
						frame_meter.eth_gas_consumed()
					};

					let gas_consumed: u64 = gas_consumed.try_into().unwrap_or(u64::MAX);
					let weight_consumed = frame_meter.weight_consumed();

					match &output {
						Ok(output) => {
							tracer.exit_child_span(&output, gas_consumed, weight_consumed)
						},
						Err(e) => tracer.exit_child_span_with_error(
							e.error.into(),
							gas_consumed,
							weight_consumed,
						),
					}
				});

				(success, output)
			},
			// `with_transactional` returned an error, and we propagate that error and note no state
			// has changed.
			Err(error) => {
				if_tracing(|tracer| {
					let frame_meter = &top_frame!(self).frame_meter;

					// we treat the initial frame meter differently to address
					// https://github.com/paritytech/polkadot-sdk/issues/8362
					let gas_consumed = if is_first_frame {
						frame_meter.total_consumed_gas()
					} else {
						frame_meter.eth_gas_consumed()
					};

					let gas_consumed: u64 = gas_consumed.try_into().unwrap_or(u64::MAX);
					let weight_consumed = frame_meter.weight_consumed();
					tracer.exit_child_span_with_error(error.into(), gas_consumed, weight_consumed);
				});

				(false, Err(error.into()))
			},
		};
		self.with_transient_storage_mut(|transient_storage| {
			if success {
				transient_storage.commit_transaction();
			} else {
				transient_storage.rollback_transaction();
			}
		});
		// For the first frame, only log the final metrics since it doesn't open a
		// checkpoint. Nested frames commit or roll back the checkpoint they opened.
		if is_first_frame {
			let m = self.access_list.metrics();
			log::trace!(
				target: LOG_TARGET,
				"access list metrics: size={size} cold={cold} hot={hot}",
				size = m.size, cold = m.cold, hot = m.hot,
			);
		} else if success {
			self.access_list.commit_frame();
		} else {
			self.access_list.rollback_frame();
		}
		debug_assert_eq!(
			self.access_list.frame_depth(),
			access_list_checkpoints_len,
			"this frame closed exactly the checkpoint it opened",
		);
		log::trace!(target: LOG_TARGET, "frame finished with: {output:?}");

		self.pop_frame(success);
		output.map(|output| {
			self.top_frame_mut().last_frame_output = output;
		})
	}

	/// Remove the current (top) frame from the stack.
	///
	/// This is called after running the current frame. It commits cached values to storage
	/// and invalidates all stale references to it that might exist further down the call stack.
	fn pop_frame(&mut self, persist: bool) {
		/// Bank the pending storage diff into the cached `ContractInfo`, then invalidate.
		///
		/// The `load` covers the case where an earlier same-contract reentry already
		/// invalidated this frame; without it a removal-bearing diff would be banked with
		/// no info and silently drop the refund pro-rata. A `None` after `load` means the
		/// frame is a precompile with no contract info, which has nothing to bank.
		fn bank_pending_changes_and_invalidate<T: Config>(f: &mut Frame<T>) {
			let contract = f.account_id.clone();
			f.contract_info.load(&f.account_id);
			if let Some(info) = f.contract_info.as_contract() {
				f.frame_meter.bank_pending_storage_changes(contract, info);
			}
			// `invalidate` drops the in-memory update `bank` made to `info`; that is safe
			// because storage already reflects it. Additions and `set_storage` removals leave
			// the frame `Cached` (write reloads the cache), so `push_frame` preview-persists
			// them before we get here. The only diff not yet in storage would be a removal on
			// an already-invalidated frame — reachable solely via `charge_storage`, which has
			// no contract-level caller. If that changes, persist here instead of invalidating.
			f.contract_info.invalidate();
		}

		// Pop the current frame from the stack and return it in case it needs to interact
		// with duplicates that might exist on the stack.
		// A `None` means that we are returning from the `first_frame`.
		let frame = self.frames.pop();

		// Both branches do essentially the same with the exception. The difference is that
		// the else branch does consume the hardcoded `first_frame`.
		if let Some(mut frame) = frame {
			let account_id = &frame.account_id;
			let prev = top_frame_mut!(self);

			// Only weight counter changes are persisted in case of a failure.
			if !persist {
				prev.frame_meter.absorb_weight_meter_only(frame.frame_meter);
				return;
			}

			// Record the storage meter changes of the nested call into the parent meter.
			// If the dropped frame's contract has a contract info we update the deposit
			// counter in its contract info. The load is necessary to pull it from storage in case
			// it was invalidated.
			frame.contract_info.load(account_id);
			let mut contract = frame.contract_info.into_contract();
			prev.frame_meter
				.absorb_all_meters(frame.frame_meter, account_id, contract.as_mut());

			// only on success inherit the created and to be destroyed contracts
			prev.contracts_created.extend(frame.contracts_created);
			for (account, args) in frame.contracts_to_be_destroyed {
				prev.schedule_termination(account, args);
			}

			if let Some(contract) = contract {
				// Persist the info and invalidate the first stale cache we find.
				// This triggers a reload from storage on next use. Only the first
				// cache needs to be invalidated because that one will invalidate the next cache
				// when it is popped from the stack.
				AccountInfo::<T>::insert_contract(
					&T::AddressMapper::to_address(account_id),
					contract,
				);
				if let Some(f) = self.frames_mut().find(|f| f.account_id == *account_id) {
					// Bank before invalidating so finalize doesn't apply the diff a second time.
					bank_pending_changes_and_invalidate(f);
				}
			}
		} else {
			if !persist {
				self.transaction_meter
					.absorb_weight_meter_only(mem::take(&mut self.first_frame.frame_meter));
				return;
			}

			let mut contract = self.first_frame.contract_info.as_contract();
			self.transaction_meter.absorb_all_meters(
				mem::take(&mut self.first_frame.frame_meter),
				&self.first_frame.account_id,
				contract.as_deref_mut(),
			);

			if let Some(contract) = contract {
				AccountInfo::<T>::insert_contract(
					&T::AddressMapper::to_address(&self.first_frame.account_id),
					contract.clone(),
				);
			}
			// End of the callstack: destroy scheduled contracts in line with EVM semantics.
			let contracts_created = mem::take(&mut self.first_frame.contracts_created);
			let contracts_to_destroy = mem::take(&mut self.first_frame.contracts_to_be_destroyed);
			for (contract_account, args) in contracts_to_destroy {
				if args.only_if_same_tx && !contracts_created.contains(&contract_account) {
					continue;
				}
				Self::do_terminate(
					&mut self.transaction_meter,
					self.exec_config,
					&contract_account,
					&self.origin,
					&args,
				);
			}
		}
	}

	/// Transfer some funds from `from` to `to`.
	///
	/// This is a no-op for zero `value`, avoiding events to be emitted for zero balance transfers.
	///
	/// If the destination account does not exist, it is pulled into existence by transferring the
	/// ED from `origin` to the new account. The total amount transferred to `to` will be ED +
	/// `value`. This makes the ED fully transparent for contracts.
	/// The ED transfer is executed atomically with the actual transfer, avoiding the possibility of
	/// the ED transfer succeeding but the actual transfer failing. In other words, if the `to` does
	/// not exist, the transfer does fail and nothing will be sent to `to` if either `origin` can
	/// not provide the ED or transferring `value` from `from` to `to` fails.
	/// Note: This will also fail if `origin` is root.
	fn transfer<S: State>(
		origin: &Origin<T>,
		from: &T::AccountId,
		to: &T::AccountId,
		value: U256,
		preservation: Preservation,
		meter: &mut ResourceMeter<T, S>,
		exec_config: &ExecConfig<T>,
	) -> DispatchResult {
		let value = BalanceWithDust::<BalanceOf<T>>::from_value::<T>(value)
			.map_err(|_| Error::<T>::BalanceConversionFailed)?;
		if value.is_zero() {
			return Ok(());
		}

		if <System<T>>::account_exists(to) {
			return transfer_with_dust::<T>(from, to, value, preservation);
		}

		let origin = origin.account_id()?;
		let ed = <T as Config>::Currency::minimum_balance();
		let is_eth_tx = exec_config.collect_deposit_from_hold.is_some();
		with_transaction(|| -> TransactionOutcome<DispatchResult> {
			// Meter the ED deposit only after the transfer succeeds: the meter is not rolled
			// back, so metering earlier would count an ED for an account never created.
			match Ok::<(), DispatchError>(())
				.and_then(|_| {
					if is_eth_tx {
						let credit = T::FeeInfo::withdraw_txfee(ed)
							.ok_or(Error::<T>::StorageDepositNotEnoughFunds)?;
						T::Currency::resolve(to, credit)
							.map_err(|_| Error::<T>::StorageDepositNotEnoughFunds)?;
						Ok(())
					} else {
						T::Currency::transfer(origin, to, ed, Preservation::Preserve)
							.map(|_| ())
							.map_err(|_| Error::<T>::StorageDepositNotEnoughFunds.into())
					}
				})
				.and_then(|_| transfer_with_dust::<T>(from, to, value, preservation))
				.and_then(|_| meter.charge_deposit(&StorageDeposit::Charge(ed)))
			{
				Ok(_) => TransactionOutcome::Commit(Ok(())),
				Err(err) => TransactionOutcome::Rollback(Err(err)),
			}
		})
	}

	/// Same as `transfer` but `from` is an `Origin`.
	fn transfer_from_origin<S: State>(
		origin: &Origin<T>,
		from: &Origin<T>,
		to: &T::AccountId,
		value: U256,
		meter: &mut ResourceMeter<T, S>,
		exec_config: &ExecConfig<T>,
	) -> ExecResult {
		// If the from address is root there is no account to transfer from, and therefore we can't
		// take any `value` other than 0.
		let from = match from {
			Origin::Signed(caller) => caller,
			Origin::Root if value.is_zero() => return Ok(Default::default()),
			Origin::Root => return Err(DispatchError::RootNotAllowed.into()),
		};
		Self::transfer(origin, from, to, value, Preservation::Preserve, meter, exec_config)
			.map(|_| Default::default())
			.map_err(Into::into)
	}

	/// Performs the actual deletion of a contract at the end of a call stack.
	///
	/// The contract that asked for the termination is no longer on the call stack, so an error
	/// here cannot be reported to it. Every balance step is therefore best effort: if it fails it
	/// is rolled back and the teardown carries on. The contract itself is always deleted.
	fn do_terminate(
		transaction_meter: &mut TransactionMeter<T>,
		exec_config: &ExecConfig<T>,
		contract_account: &T::AccountId,
		origin: &Origin<T>,
		args: &TerminateArgs<T>,
	) {
		let contract_address = T::AddressMapper::to_address(contract_account);
		// Every scheduled contract has its info stored by now: popping its frame wrote it.
		let account_info = AccountInfoOf::<T>::get(contract_address).unwrap_or_default();

		// `System.terminate` refunded the deposit when it was called, as far as the freeze on
		// the account allowed. This refunds what is still held. A freeze can keep part of it on
		// hold. That part stays on the account.
		let refund = Self::best_effort(&contract_address, "refund the storage deposit", || {
			Self::refund_on_terminate(exec_config, contract_account, origin)
		})
		.unwrap_or_default();

		// we added this consumer manually when instantiating
		System::<T>::dec_consumers(contract_account);

		// Send the balance that arrived after the termination was scheduled. `Preserve` leaves
		// the ED and anything a lock, freeze or hold pins on the account. The beneficiary
		// therefore gets all of the late funds only if the balance covers the lock or freeze. If
		// it does not, for example with a lock larger than the whole balance, nothing but the
		// dust is sent and the rest of the late funds stays on the account. If the transfer fails
		// they stay as well.
		//
		// If the contract named itself as the beneficiary, the sweep is a no-op and the balance
		// stays on the account, as in EIP-8246.
		let origin = Self::termination_origin(origin);
		let balance = <Contracts<T>>::convert_native_to_evm(
			account_info.balance(contract_account, Preservation::Preserve),
		);
		Self::best_effort(&contract_address, "send the remaining balance", || {
			Self::transfer(
				&origin,
				contract_account,
				&args.beneficiary,
				balance,
				Preservation::Preserve,
				transaction_meter,
				exec_config,
			)
		});

		// A sweep to another beneficiary moves the dust, so it is read only now.
		let dust = AccountInfoOf::<T>::get(contract_address)
			.map(|info| info.dust)
			.unwrap_or_default();

		// ED was minted when the account was brought into existence. Burn it only if the account
		// holds nothing besides it: no other balance and no dust. Otherwise the ED is kept and
		// the account stays in place as a plain account, so the balance left on it does not
		// change. A contract deployed to the same address later takes it over. A hold counts
		// towards the total balance, and while the account has no other provider its free
		// balance cannot drop below the ED, so a hold normally skips the burn. A lock or freeze
		// rolls the burn back, and so does a consumer another pallet placed while the account
		// has no other provider. A provider or sufficient reference another pallet placed does
		// not roll it back: the burn goes through and the account stays alive on that reference.
		if dust == 0 &&
			T::Currency::total_balance(contract_account) == T::Currency::minimum_balance()
		{
			Self::best_effort(&contract_address, "burn the existential deposit", || {
				T::Deposit::destroy_contract(contract_account)
			});
		}

		if let AccountType::Contract(info) = account_info.account_type {
			// this deletes the code if refcount drops to zero
			Self::best_effort(&contract_address, "release the code", || {
				CodeInfo::<T>::decrement_refcount(info.code_hash)
			});
			ContractInfo::<T>::queue_for_deletion(info.trie_id, contract_account.clone());
		} else {
			defensive!("Terminated contract has no contract info", contract_address);
		}
		// The dust is part of the balance left on the account, so it stays with it.
		AccountInfoOf::<T>::set(
			contract_address,
			(dust != 0).then(|| AccountInfo { account_type: AccountType::EOA, dust }),
		);
		ImmutableDataOf::<T>::remove(contract_address);

		// the meter needs to discard all deposits interacting with the terminated contract
		transaction_meter.terminate(contract_account.clone(), args.refunded.saturating_add(refund));

		log::trace!(target: LOG_TARGET, "Terminated {contract_address:?}");
	}

	/// The origin that receives the refunds of a terminated contract.
	///
	/// If root created the contract we need to use the pallet account_id because root has no
	/// account.
	fn termination_origin(origin: &Origin<T>) -> Origin<T> {
		match origin {
			Origin::Signed(o) => Origin::Signed(o.clone()),
			Origin::Root => Origin::from_account_id(crate::Pallet::<T>::account_id()),
		}
	}

	/// The storage deposit refund that `System.terminate` does when it is called.
	///
	/// Refunds the deposit held on `contract_account` to the termination origin, as far as the
	/// freezes on the account allow, and returns the amount refunded.
	fn refund_on_terminate(
		exec_config: &ExecConfig<T>,
		contract_account: &T::AccountId,
		origin: &Origin<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		let origin = Self::termination_origin(origin);
		T::Deposit::refund_all(contract_account, exec_config.funds(origin.account_id()?))
	}

	/// Runs `f` in its own storage transaction.
	///
	/// If `f` fails, its changes are rolled back and `None` is returned, so that the caller can
	/// carry on without them.
	fn best_effort<R>(
		contract_address: &H160,
		step: &str,
		f: impl FnOnce() -> Result<R, DispatchError>,
	) -> Option<R> {
		with_transaction(|| -> TransactionOutcome<Result<_, DispatchError>> {
			match f() {
				Ok(value) => TransactionOutcome::Commit(Ok(Some(value))),
				Err(e) => {
					log::debug!(
						target: LOG_TARGET,
						"Terminating {contract_address:?}: failed to {step}: {e:?}",
					);
					TransactionOutcome::Rollback(Ok(None))
				},
			}
		})
		.ok()
		.flatten()
	}

	/// Reference to the current (top) frame.
	fn top_frame(&self) -> &Frame<T> {
		top_frame!(self)
	}

	/// Mutable reference to the current (top) frame.
	fn top_frame_mut(&mut self) -> &mut Frame<T> {
		top_frame_mut!(self)
	}

	/// Iterator over all frames.
	///
	/// The iterator starts with the top frame and ends with the root frame.
	fn frames(&self) -> impl Iterator<Item = &Frame<T>> {
		core::iter::once(&self.first_frame).chain(&self.frames).rev()
	}

	/// Same as `frames` but with a mutable reference as iterator item.
	fn frames_mut(&mut self) -> impl Iterator<Item = &mut Frame<T>> {
		core::iter::once(&mut self.first_frame).chain(&mut self.frames).rev()
	}

	/// Caller of the frame at `depth`, counting from the top frame.
	///
	/// A delegate frame runs as the delegating contract, so its caller is the one recorded in
	/// its [`DelegateInfo`]. Any other frame is called by the frame below it, or by the origin
	/// when it is the first frame.
	fn caller_at(&self, depth: usize) -> Origin<T> {
		let Some(frame) = self.frames().nth(depth) else { return self.origin.clone() };
		if let Some(DelegateInfo { caller, .. }) = &frame.delegate {
			caller.clone()
		} else {
			self.frames()
				.nth(depth + 1)
				.map(|f| Origin::from_account_id(f.account_id.clone()))
				.unwrap_or(self.origin.clone())
		}
	}

	/// Returns whether the specified contract allows to be reentered right now.
	fn allows_reentry(&self, id: &T::AccountId) -> bool {
		!self.frames().any(|f| &f.account_id == id && !f.allows_reentry)
	}

	/// Returns the *free* balance of the supplied AccountId.
	fn account_balance(&self, who: &T::AccountId) -> U256 {
		let balance = AccountInfo::<T>::balance_of(AccountIdOrAddress::AccountId(who.clone()));
		crate::Pallet::<T>::convert_native_to_evm(balance)
	}

	/// Certain APIs, e.g. `{set,get}_immutable_data` behave differently depending
	/// on the configured entry point. Thus, we allow setting the export manually.
	#[cfg(feature = "runtime-benchmarks")]
	pub(crate) fn override_export(&mut self, export: ExportedFunction) {
		self.top_frame_mut().entry_point = export;
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub(crate) fn set_block_number(&mut self, block_number: BlockNumberFor<T>) {
		self.block_number = block_number;
	}

	fn block_hash(&self, block_number: U256) -> Option<H256> {
		let Ok(block_number) = BlockNumberFor::<T>::try_from(block_number) else {
			return None;
		};
		if block_number >= self.block_number {
			return None;
		}
		if block_number < self.block_number.saturating_sub(256u32.into()) {
			return None;
		}

		// Fallback to the system block hash for older blocks
		// 256 entries should suffice for all use cases, this mostly ensures
		// our benchmarks are passing.
		match crate::Pallet::<T>::eth_block_hash_from_number(block_number.into()) {
			Some(hash) => Some(hash),
			None => {
				use codec::Decode;
				let block_hash = System::<T>::block_hash(&block_number);
				Decode::decode(&mut TrailingZeroInput::new(block_hash.as_ref())).ok()
			},
		}
	}

	/// Returns true if the current context has contract info.
	/// This is the case if `no_precompile || precompile_with_info`.
	fn has_contract_info(&self) -> bool {
		let address = self.address();
		let precompile = <AllPrecompiles<T>>::get::<Stack<'_, T, E>>(address.as_fixed_bytes());
		if let Some(precompile) = precompile {
			return precompile.has_contract_info();
		}
		true
	}

	fn with_transient_storage_mut<R, F: FnOnce(&mut TransientStorage<T>) -> R>(
		&mut self,
		f: F,
	) -> R {
		if let Some(transient) = &self.exec_config.test_env_transient_storage {
			f(&mut transient.borrow_mut())
		} else {
			f(&mut self.transient_storage)
		}
	}
	fn with_transient_storage<R, F: FnOnce(&TransientStorage<T>) -> R>(&self, f: F) -> R {
		if let Some(transient) = &self.exec_config.test_env_transient_storage {
			f(&transient.borrow())
		} else {
			f(&self.transient_storage)
		}
	}
}

impl<'a, T, E> Ext for Stack<'a, T, E>
where
	T: Config,
	E: Executable<T>,
{
	fn delegate_call(
		&mut self,
		call_resources: &CallResources<T>,
		address: H160,
		input_data: Vec<u8>,
	) -> Result<(), ExecError> {
		// We reset the return data now, so it is cleared out even if no new frame was executed.
		// This is for example the case for unknown code hashes or creating the frame fails.
		*self.last_frame_output_mut() = Default::default();

		let top_frame = self.top_frame_mut();
		// Clone the contract info and apply pending storage changes so that
		// the child frame can correctly calculate storage deposit refunds.
		// See: <https://github.com/paritytech/contract-issues/issues/213>
		let mut contract_info = top_frame.contract_info().clone();
		top_frame.frame_meter.apply_pending_storage_changes(&mut contract_info);
		let account_id = top_frame.account_id.clone();
		let value = top_frame.value_transferred;
		if let Some(executable) = self.push_frame(
			FrameArgs::Call {
				dest: account_id,
				cached_info: Some(contract_info),
				delegated_call: Some(DelegateInfo {
					caller: self.caller().clone(),
					callee: address,
				}),
			},
			value,
			call_resources,
			self.is_read_only(),
			&input_data,
		)? {
			self.run(executable, input_data)
		} else {
			// Delegate-calls to non-contract accounts are considered success.
			Ok(())
		}
	}

	fn terminate_if_same_tx(&mut self, beneficiary: &H160) -> Result<CodeRemoved, DispatchError> {
		if_tracing(|tracer| {
			let addr = T::AddressMapper::to_address(self.account_id());
			tracer.terminate(
				addr,
				*beneficiary,
				self.top_frame()
					.frame_meter
					.eth_gas_left()
					.unwrap_or_default()
					.try_into()
					.unwrap_or_default(),
				crate::Pallet::<T>::evm_balance(&addr),
			);
		});
		let frame = top_frame_mut!(self);
		let contract_address = T::AddressMapper::to_address(&frame.account_id);
		let beneficiary = T::AddressMapper::to_account_id(beneficiary);

		// balance transfer is immediate
		Self::transfer(
			&self.origin,
			&frame.account_id,
			&beneficiary,
			<Contracts<T>>::evm_balance(&contract_address),
			Preservation::Preserve,
			&mut frame.frame_meter,
			self.exec_config,
		)?;

		// schedule for delayed deletion
		let account_id = frame.account_id.clone();
		self.top_frame_mut().schedule_termination(
			account_id,
			TerminateArgs { beneficiary, only_if_same_tx: true, refunded: Zero::zero() },
		);
		Ok(CodeRemoved::Yes)
	}

	fn own_code_hash(&mut self) -> &H256 {
		&self.top_frame_mut().contract_info().code_hash
	}

	fn immutable_data_len(&mut self) -> u32 {
		self.top_frame_mut().contract_info().immutable_data_len()
	}

	fn get_immutable_data(&mut self) -> Result<ImmutableData, DispatchError> {
		if self.top_frame().entry_point == ExportedFunction::Constructor {
			return Err(Error::<T>::InvalidImmutableAccess.into());
		}

		// Immutable is read from contract code being executed
		let address = self
			.top_frame()
			.delegate
			.as_ref()
			.map(|d| d.callee)
			.unwrap_or(T::AddressMapper::to_address(self.account_id()));
		Ok(<ImmutableDataOf<T>>::get(address).ok_or_else(|| Error::<T>::InvalidImmutableAccess)?)
	}

	fn set_immutable_data(&mut self, data: ImmutableData) -> Result<(), DispatchError> {
		let frame = self.top_frame_mut();
		if frame.entry_point == ExportedFunction::Call || data.is_empty() {
			return Err(Error::<T>::InvalidImmutableAccess.into());
		}
		frame.contract_info().set_immutable_data_len(data.len() as u32);
		<ImmutableDataOf<T>>::insert(T::AddressMapper::to_address(&frame.account_id), &data);
		Ok(())
	}
}

impl<'a, T, E> PrecompileWithInfoExt for Stack<'a, T, E>
where
	T: Config,
	E: Executable<T>,
{
	fn instantiate(
		&mut self,
		call_resources: &CallResources<T>,
		mut code: Code,
		value: U256,
		input_data: Vec<u8>,
		salt: Option<&[u8; 32]>,
	) -> Result<H160, ExecError> {
		// We reset the return data now, so it is cleared out even if no new frame was executed.
		// This is for example the case when creating the frame fails.
		*self.last_frame_output_mut() = Default::default();

		let sender = self.top_frame().account_id.clone();
		let executable = {
			let executable = match &mut code {
				Code::Upload(initcode) => {
					if !T::AllowEVMBytecode::get() {
						return Err(<Error<T>>::CodeRejected.into());
					}
					ensure!(input_data.is_empty(), <Error<T>>::EvmConstructorNonEmptyData);
					let initcode = crate::tracing::if_tracing(|_| initcode.clone())
						.unwrap_or_else(|| mem::take(initcode));
					E::from_evm_init_code(initcode, sender.clone())?
				},
				Code::Existing(hash) => {
					let executable = E::from_storage(*hash, self.frame_meter_mut())?;
					ensure!(executable.code_info().is_pvm(), <Error<T>>::EvmConstructedFromHash);
					executable
				},
			};
			self.push_frame(
				FrameArgs::Instantiate {
					sender,
					executable,
					salt,
					input_data: input_data.as_ref(),
				},
				value,
				call_resources,
				self.is_read_only(),
				&input_data,
			)?
		};
		let executable = executable.expect(FRAME_ALWAYS_EXISTS_ON_INSTANTIATE);

		let address = T::AddressMapper::to_address(&self.top_frame().account_id);
		if_tracing(|t| t.instantiate_code(&code, salt));
		self.run(executable, input_data).map(|_| address)
	}
}

impl<'a, T, E> PrecompileExt for Stack<'a, T, E>
where
	T: Config,
	E: Executable<T>,
{
	type T = T;

	fn call(
		&mut self,
		call_resources: &CallResources<T>,
		dest_addr: &H160,
		value: U256,
		input_data: Vec<u8>,
		allows_reentry: ReentrancyProtection,
		read_only: bool,
	) -> Result<(), ExecError> {
		// Before pushing the new frame: Protect the caller contract against reentrancy attacks.
		// It is important to do this before calling `allows_reentry` so that a direct recursion
		// is caught by it.

		if allows_reentry == ReentrancyProtection::Strict {
			self.top_frame_mut().allows_reentry = false;
		}

		// We reset the return data now, so it is cleared out even if no new frame was executed.
		// This is for example the case for balance transfers or when creating the frame fails.
		*self.last_frame_output_mut() = Default::default();

		let try_call = || {
			// Enable read-only access if requested; cannot disable it if already set.
			let is_read_only = read_only || self.is_read_only();

			// We can skip the stateful lookup for pre-compiles.
			let dest = if <AllPrecompiles<T>>::get::<Self>(dest_addr.as_fixed_bytes()).is_some() {
				T::AddressMapper::to_fallback_account_id(dest_addr)
			} else {
				T::AddressMapper::to_account_id(dest_addr)
			};

			if !self.allows_reentry(&dest) {
				return Err(<Error<T>>::ReentranceDenied.into());
			}

			if allows_reentry == ReentrancyProtection::AllowNext {
				self.top_frame_mut().allows_reentry = false;
			}

			// We ignore instantiate frames in our search for a cached contract.
			// Otherwise it would be possible to recursively call a contract from its own
			// constructor: We disallow calling not fully constructed contracts.
			//
			// When cloning the cached contract, we apply pending storage changes so that
			// the child frame can correctly calculate storage deposit refunds.
			// See: <https://github.com/paritytech/contract-issues/issues/213>
			let cached_info = self
				.frames()
				.find(|f| f.entry_point == ExportedFunction::Call && f.account_id == dest)
				.and_then(|f| match &f.contract_info {
					CachedContract::Cached(contract) => {
						let mut contract_with_pending = contract.clone();
						f.frame_meter.apply_pending_storage_changes(&mut contract_with_pending);
						Some(contract_with_pending)
					},
					_ => None,
				});

			if let Some(executable) = self.push_frame(
				FrameArgs::Call { dest: dest.clone(), cached_info, delegated_call: None },
				value,
				call_resources,
				is_read_only,
				&input_data,
			)? {
				self.run(executable, input_data)
			} else {
				if_tracing(|t| {
					t.enter_child_span(
						T::AddressMapper::to_address(self.account_id()),
						T::AddressMapper::to_address(&dest),
						None,
						is_read_only,
						value,
						&input_data,
						Default::default(),
					);
				});

				let snapshot = if_tracing(|_| top_frame!(self).frame_meter.snapshot());

				let result = if let Some(mock_answer) =
					self.exec_config.mock_handler.as_ref().and_then(|handler| {
						handler.mock_call(T::AddressMapper::to_address(&dest), &input_data, value)
					}) {
					*self.last_frame_output_mut() = mock_answer.clone();
					Ok(mock_answer)
				} else if is_read_only && value.is_zero() {
					Ok(Default::default())
				} else if is_read_only {
					Err(Error::<T>::StateChangeDenied.into())
				} else {
					let account_id = self.account_id().clone();
					let frame = top_frame_mut!(self);
					Self::transfer_from_origin(
						&self.origin,
						&Origin::from_account_id(account_id),
						&dest,
						value,
						&mut frame.frame_meter,
						self.exec_config,
					)
				};

				if_tracing(|t| {
					let snapshot = snapshot.as_ref().expect(
						"snapshot is taken inside if_tracing above; tracing state cannot \
						 change mid-call, so it is Some whenever this closure runs; qed",
					);
					let (gas_used, weight_delta) =
						top_frame!(self).frame_meter.delta_since(snapshot);
					match result {
						Ok(ref output) => t.exit_child_span(&output, gas_used, weight_delta),
						Err(e) => {
							t.exit_child_span_with_error(e.error.into(), gas_used, weight_delta)
						},
					}
				});

				result.map(|_| ())
			}
		};

		// We need to make sure to reset `allows_reentry` even on failure.
		let result = try_call();

		// Protection is on a per call basis.
		self.top_frame_mut().allows_reentry = true;

		result
	}

	fn get_transient_storage(&self, key: &Key) -> Option<Vec<u8>> {
		self.with_transient_storage(|transient_storage| {
			transient_storage.read(self.account_id(), key)
		})
	}

	fn get_transient_storage_size(&self, key: &Key) -> Option<u32> {
		self.with_transient_storage(|transient_storage| {
			transient_storage.read(self.account_id(), key).map(|value| value.len() as _)
		})
	}

	fn set_transient_storage(
		&mut self,
		key: &Key,
		value: Option<Vec<u8>>,
		take_old: bool,
	) -> Result<WriteOutcome, DispatchError> {
		let account_id = self.account_id().clone();
		self.with_transient_storage_mut(|transient_storage| {
			transient_storage.write(&account_id, key, value, take_old)
		})
	}

	fn account_id(&self) -> &T::AccountId {
		&self.top_frame().account_id
	}

	fn caller(&self) -> Origin<T> {
		if let Some(Ok(mock_caller)) = self
			.exec_config
			.mock_handler
			.as_ref()
			.and_then(|mock_handler| mock_handler.mock_caller(self.frames.len()))
			.map(|mock_caller| Origin::<T>::from_runtime_origin(mock_caller))
		{
			return mock_caller;
		}

		self.caller_at(0)
	}

	fn caller_of_caller(&self) -> Origin<T> {
		self.caller_at(1)
	}

	fn origin(&self) -> &Origin<T> {
		if let Some(mock_origin) = self
			.exec_config
			.mock_handler
			.as_ref()
			.and_then(|mock_handler| mock_handler.mock_origin())
		{
			return mock_origin;
		}

		&self.origin
	}

	fn to_account_id(&self, address: &H160) -> T::AccountId {
		T::AddressMapper::to_account_id(address)
	}

	fn code_hash(&self, address: &H160) -> H256 {
		if let Some(code) = <AllPrecompiles<T>>::code(address.as_fixed_bytes()).or_else(|| {
			self.exec_config
				.mock_handler
				.as_ref()
				.and_then(|handler| handler.mocked_code(*address))
		}) {
			return sp_io::hashing::keccak_256(code).into();
		}

		<AccountInfo<T>>::load_contract(&address)
			.map(|contract| contract.code_hash)
			.unwrap_or_else(|| {
				if System::<T>::account_exists(&T::AddressMapper::to_account_id(address)) {
					return EMPTY_CODE_HASH;
				}
				H256::zero()
			})
	}

	fn code_size(&self, address: &H160) -> u64 {
		if let Some(code) = <AllPrecompiles<T>>::code(address.as_fixed_bytes()).or_else(|| {
			self.exec_config
				.mock_handler
				.as_ref()
				.and_then(|handler| handler.mocked_code(*address))
		}) {
			return code.len() as u64;
		}

		<AccountInfo<T>>::load_contract(&address)
			.and_then(|contract| CodeInfoOf::<T>::get(contract.code_hash))
			.map(|info| info.code_len())
			.unwrap_or_default()
	}

	fn caller_is_origin(&self, use_caller_of_caller: bool) -> bool {
		let caller = if use_caller_of_caller { self.caller_of_caller() } else { self.caller() };
		self.origin == caller
	}

	fn caller_is_root(&self, use_caller_of_caller: bool) -> bool {
		// if the caller isn't origin, then it can't be root.
		self.caller_is_origin(use_caller_of_caller) && self.origin == Origin::Root
	}

	fn origin_is_root(&self) -> bool {
		self.origin == Origin::Root
	}

	fn balance(&self) -> U256 {
		self.account_balance(&self.top_frame().account_id)
	}

	fn balance_of(&self, address: &H160) -> U256 {
		let balance =
			self.account_balance(&<Self::T as Config>::AddressMapper::to_account_id(address));
		if_tracing(|tracer| {
			tracer.balance_read(address, balance);
		});
		balance
	}

	fn value_transferred(&self) -> U256 {
		self.top_frame().value_transferred.into()
	}

	fn now(&self) -> U256 {
		(self.timestamp / 1000u32.into()).into()
	}

	fn minimum_balance(&self) -> U256 {
		let min = T::Currency::minimum_balance();
		crate::Pallet::<T>::convert_native_to_evm(min)
	}

	fn deposit_event(&mut self, topics: Vec<H256>, data: Vec<u8>) {
		let contract = T::AddressMapper::to_address(self.account_id());
		if_tracing(|tracer| {
			let log_index = frame_system::Pallet::<Self::T>::event_count();
			tracer.log_event(contract, &topics, &data, log_index);
		});

		// Capture the log only if it is generated by an Ethereum transaction.
		block_storage::capture_ethereum_log(&contract, &data, &topics);

		Contracts::<Self::T>::deposit_event(Event::ContractEmitted { contract, data, topics });
	}

	fn block_number(&self) -> U256 {
		self.block_number.into()
	}

	fn block_hash(&self, block_number: U256) -> Option<H256> {
		self.block_hash(block_number)
	}

	fn block_author(&self) -> H160 {
		Contracts::<Self::T>::block_author()
	}

	fn gas_limit(&self) -> u64 {
		<Contracts<T>>::evm_block_gas_limit().saturated_into()
	}

	fn chain_id(&self) -> u64 {
		<T as Config>::ChainId::get()
	}

	fn gas_meter(&self) -> &FrameMeter<Self::T> {
		&self.top_frame().frame_meter
	}

	#[inline]
	fn gas_meter_mut(&mut self) -> &mut FrameMeter<Self::T> {
		&mut self.top_frame_mut().frame_meter
	}

	fn frame_meter(&self) -> &FrameMeter<Self::T> {
		&self.top_frame().frame_meter
	}

	#[inline]
	fn frame_meter_mut(&mut self) -> &mut FrameMeter<Self::T> {
		&mut self.top_frame_mut().frame_meter
	}

	fn ecdsa_recover(&self, signature: &[u8; 65], message_hash: &[u8; 32]) -> Result<[u8; 33], ()> {
		secp256k1_ecdsa_recover_compressed(signature, message_hash).map_err(|_| ())
	}

	fn sr25519_verify(&self, signature: &[u8; 64], message: &[u8], pub_key: &[u8; 32]) -> bool {
		sp_io::crypto::sr25519_verify(
			&SR25519Signature::from(*signature),
			message,
			&SR25519Public::from(*pub_key),
		)
	}

	fn ecdsa_to_eth_address(&self, pk: &[u8; 33]) -> Result<[u8; 20], DispatchError> {
		Ok(ECDSAPublic::from(*pk)
			.to_eth_address()
			.or_else(|()| Err(Error::<T>::EcdsaRecoveryFailed))?)
	}

	#[cfg(any(test, feature = "runtime-benchmarks"))]
	fn contract_info(&mut self) -> &mut ContractInfo<Self::T> {
		self.top_frame_mut().contract_info()
	}

	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn transient_storage(&mut self) -> &mut TransientStorage<Self::T> {
		&mut self.transient_storage
	}

	fn is_read_only(&self) -> bool {
		self.top_frame().read_only
	}

	fn is_delegate_call(&self) -> bool {
		self.top_frame().delegate.is_some()
	}

	fn last_frame_output(&self) -> &ExecReturnValue {
		&self.top_frame().last_frame_output
	}

	fn last_frame_output_mut(&mut self) -> &mut ExecReturnValue {
		&mut self.top_frame_mut().last_frame_output
	}

	fn copy_code_slice(&mut self, buf: &mut [u8], address: &H160, code_offset: usize) {
		let len = buf.len();
		if len == 0 {
			return;
		}

		let code_hash = self.code_hash(address);
		let code = crate::PristineCode::<T>::get(&code_hash).unwrap_or_default();

		let len = len.min(code.len().saturating_sub(code_offset));
		if len > 0 {
			buf[..len].copy_from_slice(&code[code_offset..code_offset + len]);
		}

		buf[len..].fill(0);
	}

	fn terminate_caller(&mut self, beneficiary: &H160) -> Result<(), DispatchError> {
		ensure!(self.top_frame().delegate.is_none(), Error::<T>::PrecompileDelegateDenied);
		let parent = self.frames_mut().nth(1).ok_or_else(|| Error::<T>::ContractNotFound)?;
		ensure!(parent.entry_point == ExportedFunction::Call, Error::<T>::TerminatedInConstructor);
		ensure!(parent.delegate.is_none(), Error::<T>::PrecompileDelegateDenied);

		let contract_address = T::AddressMapper::to_address(&parent.account_id);
		let beneficiary = T::AddressMapper::to_account_id(beneficiary);

		let parent_account_id = parent.account_id.clone();

		// Refund the storage deposit before the payout. The hold counts towards any freeze on
		// the account, so after the payout it would be needed to cover the freeze. Released now,
		// it is paid back as far as the freeze allows, and only the remainder stays on hold.
		let refunded =
			Self::refund_on_terminate(&self.exec_config, &parent_account_id, &self.origin)?;

		// balance transfer is immediate
		Self::transfer(
			&self.origin,
			&parent_account_id,
			&beneficiary,
			<Contracts<T>>::evm_balance(&contract_address),
			Preservation::Preserve,
			&mut top_frame_mut!(self).frame_meter,
			&self.exec_config,
		)?;

		// schedule for delayed deletion
		let args = TerminateArgs { beneficiary, only_if_same_tx: false, refunded };
		self.top_frame_mut().schedule_termination(parent_account_id, args);

		Ok(())
	}

	fn effective_gas_price(&self) -> U256 {
		self.exec_config
			.effective_gas_price
			.unwrap_or_else(|| <Contracts<T>>::evm_base_fee())
	}

	fn gas_left(&self) -> u64 {
		let frame = self.top_frame();

		frame.frame_meter.eth_gas_left().unwrap_or_default().saturated_into::<u64>()
	}

	fn get_storage(&mut self, key: &Key) -> Option<Vec<u8>> {
		assert!(self.has_contract_info());
		self.top_frame_mut().contract_info().read(key)
	}

	fn get_storage_size(&mut self, key: &Key) -> Option<u32> {
		assert!(self.has_contract_info());
		self.top_frame_mut().contract_info().size(key.into())
	}

	fn set_storage(
		&mut self,
		key: &Key,
		value: Option<Vec<u8>>,
		take_old: bool,
	) -> Result<WriteOutcome, DispatchError> {
		assert!(self.has_contract_info());
		let frame = self.top_frame_mut();
		frame.contract_info.get(&frame.account_id).write(
			key.into(),
			value,
			Some(&mut frame.frame_meter),
			take_old,
		)
	}

	fn touch_storage_access(
		&mut self,
		transient: bool,
		key: &Key,
		op: StorageOp,
	) -> StorageAccessKind {
		if transient {
			return StorageAccessKind::Transient;
		}
		let address = self.address();
		StorageAccessKind::Persistent(
			self.access_list.touch(AccessEntry { address, slot: key.into() }, op),
		)
	}

	fn peek_storage_access(&self, transient: bool, key: &Key) -> StorageAccessKind {
		if transient {
			return StorageAccessKind::Transient;
		}
		let address = self.address();
		StorageAccessKind::Persistent(
			self.access_list.peek(&AccessEntry { address, slot: key.into() }),
		)
	}

	fn charge_storage(&mut self, diff: &Diff) -> DispatchResult {
		assert!(self.has_contract_info());
		self.top_frame_mut().frame_meter.record_contract_storage_changes(diff)
	}
}

/// Returns true if the address has a precompile contract, else false.
pub fn is_precompile<T: Config, E: Executable<T>>(address: &H160) -> bool {
	<AllPrecompiles<T>>::get::<Stack<'_, T, E>>(address.as_fixed_bytes()).is_some()
}

/// Runs the balance work of a termination: the storage deposit refund that `System.terminate`
/// does when it is called, followed by the teardown at the end of the call stack. `SELFDESTRUCT`
/// (`only_if_same_tx`) does not refund when it is called, so the refund is skipped for it.
#[cfg(feature = "runtime-benchmarks")]
pub fn bench_do_terminate<T: Config>(
	transaction_meter: &mut TransactionMeter<T>,
	exec_config: &ExecConfig<T>,
	contract_account: &T::AccountId,
	origin: &Origin<T>,
	beneficiary: T::AccountId,
	only_if_same_tx: bool,
) -> Result<(), DispatchError> {
	type BenchStack<'a, T> = Stack<'a, T, crate::ContractBlob<T>>;
	let refunded = if only_if_same_tx {
		Zero::zero()
	} else {
		BenchStack::<T>::refund_on_terminate(exec_config, contract_account, origin)?
	};
	BenchStack::<T>::do_terminate(
		transaction_meter,
		exec_config,
		contract_account,
		origin,
		&TerminateArgs { beneficiary, only_if_same_tx, refunded },
	);
	Ok(())
}

mod sealing {
	use super::*;

	pub trait Sealed {}
	impl<'a, T: Config, E> Sealed for Stack<'a, T, E> {}

	#[cfg(test)]
	impl<T: Config> sealing::Sealed for mock_ext::MockExt<T> {}
}
