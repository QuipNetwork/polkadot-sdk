// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

#![cfg_attr(not(feature = "std"), no_std)]
// `construct_runtime!` does a lot of recursion and requires us to increase the limit to 256.
#![recursion_limit = "256"]

// Make the WASM binaries available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

mod features;
mod flavors;
mod genesis_config_presets;
pub mod test_pallet;

extern crate alloc;

use features::*;

use alloc::{vec, vec::Vec};
use frame_support::{derive_impl, traits::OnRuntimeUpgrade, PalletId};
use sp_api::{decl_runtime_apis, impl_runtime_apis};
pub use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
pub use sp_consensus_aura::sr25519::AuthorityId as AuraId;
use sp_core::{ConstBool, ConstU32, ConstU64, Get, OpaqueMetadata};

use sp_runtime::{
	generic, impl_opaque_keys,
	traits::{BlakeTwo256, Block as BlockT, IdentifyAccount, Verify},
	transaction_validity::{TransactionSource, TransactionValidity},
	ApplyExtrinsicResult, MultiAddress, MultiSignature,
};
use sp_version::RuntimeVersion;

use cumulus_primitives_core::{ParaId, RelayProofRequest, VerifySchedulingSignature};

define_flavors!(consts wasm);

// A few exports that help ease life for downstream crates.
pub use frame_support::{
	construct_runtime,
	dispatch::DispatchClass,
	genesis_builder_helper::{build_state, get_preset},
	parameter_types,
	traits::{ConstU8, Randomness},
	weights::{
		constants::{
			BlockExecutionWeight, ExtrinsicBaseWeight, RocksDbWeight, WEIGHT_REF_TIME_PER_SECOND,
		},
		ConstantMultiplier, IdentityFee, Weight,
	},
	StorageValue,
};
pub use frame_system::Call as SystemCall;
use frame_system::{
	limits::{BlockLength, BlockWeights},
	EnsureRoot,
};
pub use pallet_balances::Call as BalancesCall;
pub use pallet_glutton::Call as GluttonCall;
pub use pallet_sudo::Call as SudoCall;
pub use pallet_timestamp::{Call as TimestampCall, Now};
#[cfg(any(feature = "std", test))]
pub use sp_runtime::BuildStorage;
pub use sp_runtime::{Perbill, Permill};
pub use test_pallet::{Call as TestPalletCall, TestTransactionExtension};

pub type SessionHandlers = ();

#[cfg(not(feature = "with-authority-discovery"))]
impl_opaque_keys! {
	pub struct SessionKeys {
		pub aura: Aura,
	}
}

#[cfg(feature = "with-authority-discovery")]
impl_opaque_keys! {
	pub struct SessionKeys {
		pub aura: Aura,
		pub authority_discovery: AuthorityDiscovery,
	}
}

/// The para-id used in this runtime.
pub const PARACHAIN_ID: u32 = 100;

const RELAY_CHAIN_SLOT_DURATION_MILLIS: u32 = 6000;

// The only difference between the three declarations below is the `spec_version`.
// The behavior is:
// - by default `spec_version` should be 2
// - with `spec-version-3` feature enabled `spec_version` should be 3
// - with `spec-version-4` feature enabled `spec_version` should be 4
//
// The duplication here is unfortunate necessity.
//
// runtime_version macro is dumb. It accepts a const item declaration, passes it through and
// also emits runtime version custom section. It parses the expressions to extract the version
// details. Since macro kicks in early, it operates on AST. Thus, you cannot use constants.
// Macros are expanded top to bottom, meaning we also cannot use `cfg` here.
#[cfg(all(not(feature = "spec-version-3"), not(feature = "spec-version-4"),))]
#[sp_version::runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: alloc::borrow::Cow::Borrowed("cumulus-test-parachain"),
	impl_name: alloc::borrow::Cow::Borrowed("cumulus-test-parachain"),
	authoring_version: 1,
	// Read the note above.
	spec_version: 2,
	impl_version: 1,
	apis: RUNTIME_API_VERSIONS,
	transaction_version: 1,
	system_version: 3,
};

#[cfg(all(feature = "spec-version-3", not(feature = "spec-version-4"),))]
#[sp_version::runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: alloc::borrow::Cow::Borrowed("cumulus-test-parachain"),
	impl_name: alloc::borrow::Cow::Borrowed("cumulus-test-parachain"),
	authoring_version: 1,
	// Read the note above.
	spec_version: 3,
	impl_version: 1,
	apis: RUNTIME_API_VERSIONS,
	transaction_version: 1,
	system_version: 3,
};

#[cfg(feature = "spec-version-4")]
#[sp_version::runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: alloc::borrow::Cow::Borrowed("cumulus-test-parachain"),
	impl_name: alloc::borrow::Cow::Borrowed("cumulus-test-parachain"),
	authoring_version: 1,
	// Read the note above.
	spec_version: 4,
	impl_version: 1,
	apis: RUNTIME_API_VERSIONS,
	transaction_version: 1,
	system_version: 3,
};

pub const EPOCH_DURATION_IN_BLOCKS: u32 = 10 * MINUTES;

// These time units are defined in number of blocks.
pub const MINUTES: BlockNumber = 60_000 / (slot_duration() as BlockNumber);
pub const HOURS: BlockNumber = MINUTES * 60;
pub const DAYS: BlockNumber = HOURS * 24;

// 1 in 4 blocks (on average, not counting collisions) will be primary babe blocks.
pub const PRIMARY_PROBABILITY: (u64, u64) = (1, 4);

/// We assume that ~10% of the block weight is consumed by `on_initialize` handlers.
/// This is used to limit the maximal weight of a single extrinsic.
const AVERAGE_ON_INITIALIZE_RATIO: Perbill = Perbill::from_percent(10);
/// We allow `Normal` extrinsics to fill up the block up to 75%, the rest can be used
/// by  Operational  extrinsics.
const NORMAL_DISPATCH_RATIO: Perbill = Perbill::from_percent(75);

type MaximumBlockWeight = cumulus_pallet_parachain_system::block_weight::MaxParachainBlockWeight<
	Runtime,
	ConstU32<{ block_processing_velocity() }>,
>;

parameter_types! {
	/// Target number of blocks per relay chain slot.
	pub const NumberOfBlocksPerRelaySlot: u32 = 12;
	pub const BlockHashCount: BlockNumber = 250;
	pub const Version: RuntimeVersion = VERSION;
	/// We allow for 1 second of compute with a 6 second average block time.
	pub RuntimeBlockLength: BlockLength =
		BlockLength::builder().max_length(10 * 1024 * 1024).max_header_size(5 * 1024 * 1024).build();
	pub RuntimeBlockWeights: BlockWeights = BlockWeights::builder()
		.base_block(BlockExecutionWeight::get())
		.for_class(DispatchClass::all(), |weights| {
			weights.base_extrinsic = ExtrinsicBaseWeight::get();
		})
		.for_class(DispatchClass::Normal, |weights| {
			weights.max_total = Some(NORMAL_DISPATCH_RATIO * MaximumBlockWeight::get());
		})
		.for_class(DispatchClass::Operational, |weights| {
			weights.max_total = Some(MaximumBlockWeight::get());
			// Operational transactions have some extra reserved space, so that they
			// are included even if block reached `MaximumBlockWeight`.
			weights.reserved = Some(
				MaximumBlockWeight::get() - NORMAL_DISPATCH_RATIO * MaximumBlockWeight::get()
			);
		})
		.avg_block_initialization(AVERAGE_ON_INITIALIZE_RATIO)
		.build_or_panic();
	pub const SS58Prefix: u8 = 42;
}

#[derive_impl(frame_system::config_preludes::ParaChainDefaultConfig)]
impl frame_system::Config for Runtime {
	/// The identifier used to distinguish between accounts.
	type AccountId = AccountId;
	/// The index type for storing how many extrinsics an account has signed.
	type Nonce = Nonce;
	/// The type for hashing blocks and tries.
	type Hash = Hash;
	/// The block type.
	type Block = Block;
	/// Maximum number of block number to block hash mappings to keep (oldest pruned first).
	type BlockHashCount = BlockHashCount;
	/// Runtime version.
	type Version = Version;
	type AccountData = pallet_balances::AccountData<Balance>;
	type BlockWeights = RuntimeBlockWeights;
	type BlockLength = RuntimeBlockLength;
	type SS58Prefix = SS58Prefix;
	type OnSetCode = cumulus_pallet_parachain_system::ParachainSetCode<Self>;
	type MaxConsumers = frame_support::traits::ConstU32<16>;
	type PreInherents = cumulus_pallet_parachain_system::block_weight::DynamicMaxBlockWeightHooks<
		Runtime,
		ConstU32<{ block_processing_velocity() }>,
	>;
	type SingleBlockMigrations = SingleBlockMigrations;
}

impl cumulus_pallet_weight_reclaim::Config for Runtime {
	type WeightInfo = ();
}

parameter_types! {
	pub const MinimumPeriod: u64 = 0;
}

parameter_types! {
	pub const PotId: PalletId = PalletId(*b"PotStake");
	pub const SessionLength: BlockNumber = 10 * MINUTES;
	pub const Offset: u32 = 0;
}

impl cumulus_pallet_aura_ext::Config for Runtime {}

impl pallet_timestamp::Config for Runtime {
	/// A timestamp: milliseconds since the unix epoch.
	type Moment = u64;
	type OnTimestampSet = Aura;
	type MinimumPeriod = MinimumPeriod;
	type WeightInfo = ();
}

parameter_types! {
	pub const ExistentialDeposit: u128 = 500;
	pub const TransferFee: u128 = 0;
	pub const CreationFee: u128 = 0;
	pub const TransactionByteFee: u128 = 1;
	pub const MaxReserves: u32 = 50;
}

impl pallet_balances::Config for Runtime {
	/// The type for recording an account's balance.
	type Balance = Balance;
	/// The ubiquitous event type.
	type RuntimeEvent = RuntimeEvent;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
	type MaxLocks = ();
	type MaxReserves = MaxReserves;
	type ReserveIdentifier = [u8; 8];
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = ();
	type MaxFreezes = ConstU32<0>;
	type DoneSlashHandler = ();
}

impl pallet_transaction_payment::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OnChargeTransaction = pallet_transaction_payment::FungibleAdapter<Balances, ()>;
	type WeightToFee = IdentityFee<Balance>;
	type LengthToFee = ConstantMultiplier<Balance, TransactionByteFee>;
	type FeeMultiplierUpdate = ();
	type OperationalFeeMultiplier = ConstU8<5>;
	type WeightInfo = pallet_transaction_payment::weights::SubstrateWeight<Runtime>;
}

impl pallet_sudo::Config for Runtime {
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = pallet_sudo::weights::SubstrateWeight<Runtime>;
}

impl pallet_utility::Config for Runtime {
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = pallet_utility::weights::SubstrateWeight<Runtime>;
}

impl pallet_glutton::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type AdminOrigin = EnsureRoot<AccountId>;
	type WeightInfo = pallet_glutton::weights::SubstrateWeight<Runtime>;
}

/// Scheduling-info verifier used by `cumulus-test-runtime`.
///
/// Accepts any signature; `V3_SCHEDULING_ENABLED` is gated on the `v3-descriptor` cargo
/// feature so the test runtime can flip V3 scheduling on without needing a runtime upgrade
/// per build.
pub struct NoVerification;

impl VerifySchedulingSignature for NoVerification {
	const V3_SCHEDULING_ENABLED: bool = SCHEDULING_V3_ENABLED;

	fn verify(
		_signed_info: &cumulus_primitives_core::SignedSchedulingInfo,
		_relay_slot: cumulus_primitives_core::relay_chain::Slot,
	) -> bool {
		true
	}
}

type ConsensusHook = cumulus_pallet_aura_ext::FixedVelocityConsensusHook<
	Runtime,
	RELAY_CHAIN_SLOT_DURATION_MILLIS,
	{ block_processing_velocity() },
	{ unincluded_segment_capacity() },
>;
impl cumulus_pallet_parachain_system::Config for Runtime {
	type WeightInfo = ();
	type SelfParaId = parachain_info::Pallet<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type OnSystemEvent = TestPallet;
	type OutboundXcmpMessageSource = TestPallet;
	// Ignore all DMP messages by enqueueing them into `()`:
	type DmpQueue = frame_support::traits::EnqueueWithOrigin<(), sp_core::ConstU8<0>>;
	type ReservedDmpWeight = ();
	type XcmpMessageHandler = ();
	type ReservedXcmpWeight = ();
	type CheckAssociatedRelayNumber =
		cumulus_pallet_parachain_system::RelayNumberMonotonicallyIncreases;
	type ConsensusHook = ConsensusHook;
	type RelayParentOffset = ConstU32<{ relay_parent_offset() }>;
	type SchedulingSignatureVerifier = NoVerification;
}

impl parachain_info::Config for Runtime {}

impl pallet_aura::Config for Runtime {
	type AuthorityId = AuraId;
	type DisabledValidators = ();
	type MaxAuthorities = ConstU32<32>;
	type AllowMultipleBlocksPerSlot = ConstBool<{ !cfg!(feature = "sync-backing") }>;
	type SlotDuration = ConstU64<{ slot_duration() }>;
}

impl test_pallet::Config for Runtime {}

parameter_types! {
	pub const Period: u32 = 10;
}

#[cfg(feature = "with-authority-discovery")]
impl pallet_session::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ValidatorId = AccountId;
	type ValidatorIdOf = sp_runtime::traits::ConvertInto;
	type ShouldEndSession = pallet_session::PeriodicSessions<Period, Offset>;
	type NextSessionRotation = pallet_session::PeriodicSessions<Period, Offset>;
	type SessionManager = ();
	type SessionHandler = <SessionKeys as sp_runtime::traits::OpaqueKeys>::KeyTypeIdProviders;
	type Keys = SessionKeys;
	type DisablingStrategy = ();
	type WeightInfo = ();
	type Currency = Balances;
	type KeyDeposit = ();
}

#[cfg(feature = "with-authority-discovery")]
impl pallet_authority_discovery::Config for Runtime {
	type MaxAuthorities = ConstU32<32>;
}

construct_runtime! {
	pub enum Runtime
	{
		System: frame_system,
		ParachainSystem: cumulus_pallet_parachain_system,
		Timestamp: pallet_timestamp,
		ParachainInfo: parachain_info,
		Balances: pallet_balances,
		Sudo: pallet_sudo,
		Utility: pallet_utility,
		TransactionPayment: pallet_transaction_payment,
		TestPallet: test_pallet,
		Glutton: pallet_glutton,
		Aura: pallet_aura,
		// Session must come BEFORE AuraExt so its on_genesis_session populates
		// pallet_aura::Authorities before AuraExt's genesis_build snapshots it.
		#[cfg(feature = "with-authority-discovery")]
		Session: pallet_session,
		#[cfg(feature = "with-authority-discovery")]
		AuthorityDiscovery: pallet_authority_discovery,
		AuraExt: cumulus_pallet_aura_ext,
		WeightReclaim: cumulus_pallet_weight_reclaim,
	}
}

/// Index of a transaction in the chain.
pub type Nonce = u32;
/// A hash of some data used by the chain.
pub type Hash = sp_core::H256;
/// Balance of an account.
pub type Balance = u128;
/// Alias to 512-bit hash when used in the context of a transaction signature on the chain.
pub type Signature = MultiSignature;
/// An index to a block.
pub type BlockNumber = u32;
/// Some way of identifying an account on the chain. We intentionally make it equivalent
/// to the public key of our transaction signing scheme.
pub type AccountId = <<Signature as Verify>::Signer as IdentifyAccount>::AccountId;
/// Opaque block type.
pub type NodeBlock = generic::Block<Header, sp_runtime::OpaqueExtrinsic>;

/// The address format for describing accounts.
pub type Address = MultiAddress<AccountId, ()>;
/// Block header type as expected by this runtime.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
/// Block type as expected by this runtime.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;
/// A Block signed with a Justification
pub type SignedBlock = generic::SignedBlock<Block>;
/// BlockId type as expected by this runtime.
pub type BlockId = generic::BlockId<Block>;
/// The extension to the basic transaction logic.
pub type TxExtension = cumulus_pallet_parachain_system::block_weight::DynamicMaxBlockWeight<
	Runtime,
	cumulus_pallet_weight_reclaim::StorageWeightReclaim<
		Runtime,
		(
			frame_system::AuthorizeCall<Runtime>,
			frame_system::CheckNonZeroSender<Runtime>,
			frame_system::CheckSpecVersion<Runtime>,
			frame_system::CheckGenesis<Runtime>,
			frame_system::CheckEra<Runtime>,
			frame_system::CheckNonce<Runtime>,
			frame_system::CheckWeight<Runtime>,
			pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
			test_pallet::TestTransactionExtension<Runtime>,
		),
	>,
	ConstU32<{ block_processing_velocity() }>,
>;

/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic =
	generic::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>;
/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<
	Runtime,
	Block,
	frame_system::ChainContext<Runtime>,
	Runtime,
	AllPalletsWithSystem,
>;

/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<RuntimeCall, TxExtension>;

/// Migration to verify that runtime upgrade hooks are working correctly.
///
/// This checks that the test_pallet runtime upgrade key was set in genesis.
pub struct VerifyRuntimeUpgrade;

impl OnRuntimeUpgrade for VerifyRuntimeUpgrade {
	fn on_runtime_upgrade() -> Weight {
		assert_eq!(
			sp_io::storage::get(test_pallet::TEST_RUNTIME_UPGRADE_KEY),
			Some(vec![1, 2, 3, 4].into())
		);
		Weight::from_parts(1, 0)
	}
}

/// Single-block migrations for the test runtime.
///
/// These migrations execute immediately and entirely at the beginning of the block following
/// a runtime upgrade. They must be lightweight enough to complete within a single block.
#[cfg(feature = "with-authority-discovery")]
pub type SingleBlockMigrations = (VerifyRuntimeUpgrade, migrations::EnableAuthorityDiscovery);
#[cfg(not(feature = "with-authority-discovery"))]
pub type SingleBlockMigrations = (VerifyRuntimeUpgrade,);

/// One-shot migration that seeds `pallet_session` from `pallet_aura::Authorities` when a
/// default (no-AD) chain upgrades to the `with-authority-discovery` variant.
///
/// Idempotent: only runs when `pallet_session::Validators` is empty, which is the case
/// on a chain that never had `pallet_session` in its runtime.
#[cfg(feature = "with-authority-discovery")]
pub mod migrations {
	use super::*;
	use sp_core::crypto::key_types;

	pub struct EnableAuthorityDiscovery;

	impl OnRuntimeUpgrade for EnableAuthorityDiscovery {
		fn on_runtime_upgrade() -> Weight {
			let db: frame_support::weights::RuntimeDbWeight =
				<Runtime as frame_system::Config>::DbWeight::get();

			// Idempotent guard: skip if Validators is already populated.
			if !pallet_session::Validators::<Runtime>::get().is_empty() {
				return db.reads(1);
			}

			let aura_authorities = pallet_aura::Authorities::<Runtime>::get();
			let n = aura_authorities.len() as u64;

			let mut validators: Vec<AccountId> = Vec::with_capacity(aura_authorities.len());
			let mut queued_keys: Vec<(AccountId, SessionKeys)> =
				Vec::with_capacity(aura_authorities.len());

			for aura_pub in aura_authorities.iter() {
				// `AuraId` is app-crypto over `sr25519::Public`; `.into()` gives the inner.
				let inner: sp_core::sr25519::Public = aura_pub.clone().into();
				let raw: [u8; 32] = inner.0;
				let account: AccountId = sp_core::sr25519::Public::from_raw(raw).into();
				let aura_key = AuraId::from(sp_core::sr25519::Public::from_raw(raw));
				let audi_key = AuthorityDiscoveryId::from(sp_core::sr25519::Public::from_raw(raw));
				let session_keys = SessionKeys { aura: aura_key, authority_discovery: audi_key };

				// Populate NextKeys and KeyOwner (mirrors pallet_session genesis logic).
				pallet_session::NextKeys::<Runtime>::insert(&account, &session_keys);
				// KeyOwner maps (KeyTypeId, key_bytes: Vec<u8>) → ValidatorId.
				// We use <[u8]>::to_vec() to get an owned Vec<u8> that EncodeLike<Vec<u8>>.
				let aura_bytes: alloc::vec::Vec<u8> =
					<AuraId as sp_runtime::RuntimeAppPublic>::to_raw_vec(&session_keys.aura);
				let audi_bytes: alloc::vec::Vec<u8> =
					<AuthorityDiscoveryId as sp_runtime::RuntimeAppPublic>::to_raw_vec(
						&session_keys.authority_discovery,
					);
				pallet_session::KeyOwner::<Runtime>::insert(
					(key_types::AURA, aura_bytes),
					&account,
				);
				pallet_session::KeyOwner::<Runtime>::insert(
					(key_types::AUTHORITY_DISCOVERY, audi_bytes),
					&account,
				);

				// Mirror `pallet_session::do_set_keys`: increment the account's consumer
				// count so a future `purge_keys` decrements it correctly. Zombienet-injected
				// aura keys without endowment are skipped — they have no consumer to track.
				if frame_system::Pallet::<Runtime>::providers(&account) > 0 {
					let inc_ok = frame_system::Pallet::<Runtime>::inc_consumers(&account).is_ok();
					debug_assert!(inc_ok, "inc_consumers failed despite providers > 0");
				}

				validators.push(account.clone());
				queued_keys.push((account, session_keys));
			}

			// Write Validators and QueuedKeys so the session pallet has a coherent state.
			pallet_session::Validators::<Runtime>::put(&validators);
			pallet_session::QueuedKeys::<Runtime>::put(&queued_keys);

			// YOLO so these keys are not empty until next session.
			let ad_authorities: Vec<AuthorityDiscoveryId> = aura_authorities
				.iter()
				.map(|aura_pub| {
					let inner: sp_core::sr25519::Public = aura_pub.clone().into();
					AuthorityDiscoveryId::from(sp_core::sr25519::Public::from_raw(inner.0))
				})
				.collect();
			let bounded = frame_support::WeakBoundedVec::<_, _>::force_from(
				ad_authorities,
				Some("EnableAuthorityDiscovery migration: authority count exceeds MaxAuthorities"),
			);
			pallet_authority_discovery::Keys::<Runtime>::put(bounded);

			Self::assert_post_upgrade_invariants();

			let reads = n.saturating_add(2);
			let writes = n.saturating_mul(4).saturating_add(3);
			db.reads(reads).saturating_add(db.writes(writes))
		}
	}

	impl EnableAuthorityDiscovery {
		fn assert_post_upgrade_invariants() {
			let aura_count = pallet_aura::Authorities::<Runtime>::get().len();
			let validators = pallet_session::Validators::<Runtime>::get();
			let queued = pallet_session::QueuedKeys::<Runtime>::get();
			let ad_keys = pallet_authority_discovery::Keys::<Runtime>::get();

			assert!(!validators.is_empty(), "Validators empty after migration");
			assert_eq!(validators.len(), aura_count, "Validators ≠ aura Authorities");
			assert_eq!(queued.len(), aura_count, "QueuedKeys ≠ aura Authorities");
			assert_eq!(ad_keys.len(), aura_count, "AuthorityDiscovery::Keys ≠ aura Authorities");
			assert_eq!(
				pallet_session::NextKeys::<Runtime>::iter().count(),
				aura_count,
				"NextKeys entry count ≠ aura Authorities",
			);
			assert_eq!(
				pallet_session::KeyOwner::<Runtime>::iter().count(),
				2 * aura_count,
				"KeyOwner count ≠ 2× aura Authorities (aura + audi)",
			);
			// Each provisioned validator account had its consumer count bumped by
			// `inc_consumers`, mirroring `pallet_session::do_set_keys` semantics.
			// Un-provisioned aura authorities (e.g. extra zombienet-generated collator keys
			// that aren't in the endowed-accounts list) are skipped: `inc_consumers`
			// returned `Err` for them at migration time, and they have no consumer to bump.
			for account in &validators {
				if frame_system::Pallet::<Runtime>::providers(account) > 0 {
					assert!(
						frame_system::Pallet::<Runtime>::consumers(account) >= 1,
						"provisioned validator {account:?} has 0 consumers; \
						 inc_consumers didn't fire",
					);
				}
			}
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;
		use frame_support::traits::OnRuntimeUpgrade;
		use sp_keyring::Sr25519Keyring;

		fn ext_with_aura(keys: &[Sr25519Keyring]) -> sp_io::TestExternalities {
			let mut ext = sp_io::TestExternalities::new_empty();
			ext.execute_with(|| {
				let aura_keys: alloc::vec::Vec<AuraId> = keys
					.iter()
					.map(|k| AuraId::from(sp_core::sr25519::Public::from_raw(k.public().0)))
					.collect();
				let bounded = frame_support::BoundedVec::<_, _>::try_from(aura_keys).expect("fits");
				pallet_aura::Authorities::<Runtime>::put(bounded);
				// Provision providers so the migration's `inc_consumers` call can succeed —
				// production parachains rely on every authority account being funded.
				for k in keys {
					let account: AccountId = k.to_account_id();
					frame_system::Pallet::<Runtime>::inc_providers(&account);
				}
			});
			ext
		}

		fn expected_weight(n: u64) -> Weight {
			let db: frame_support::weights::RuntimeDbWeight =
				<Runtime as frame_system::Config>::DbWeight::get();
			let reads = n.saturating_add(2);
			let writes = n.saturating_mul(4).saturating_add(3);
			db.reads(reads).saturating_add(db.writes(writes))
		}

		#[test]
		fn populates_session_state() {
			// Invariants are asserted in `on_runtime_upgrade`.
			let keys = [Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie];
			ext_with_aura(&keys).execute_with(|| {
				let w = EnableAuthorityDiscovery::on_runtime_upgrade();
				assert_eq!(w, expected_weight(keys.len() as u64));
			});
		}

		#[test]
		fn is_idempotent() {
			let keys = [Sr25519Keyring::Alice, Sr25519Keyring::Bob];
			ext_with_aura(&keys).execute_with(|| {
				EnableAuthorityDiscovery::on_runtime_upgrade();
				let w2 = EnableAuthorityDiscovery::on_runtime_upgrade();

				let db: frame_support::weights::RuntimeDbWeight =
					<Runtime as frame_system::Config>::DbWeight::get();
				assert_eq!(w2, db.reads(1), "second call should be a 1-read no-op");
			});
		}

		#[test]
		fn noop_when_validators_already_set() {
			let keys = [Sr25519Keyring::Alice];
			ext_with_aura(&keys).execute_with(|| {
				pallet_session::Validators::<Runtime>::put(alloc::vec![
					Sr25519Keyring::Alice.to_account_id(),
				]);
				let w = EnableAuthorityDiscovery::on_runtime_upgrade();
				let db: frame_support::weights::RuntimeDbWeight =
					<Runtime as frame_system::Config>::DbWeight::get();
				assert_eq!(w, db.reads(1));
				assert!(pallet_authority_discovery::Keys::<Runtime>::get().is_empty());
			});
		}
	}
}

decl_runtime_apis! {
	pub trait GetLastTimestamp {
		/// Returns the last timestamp of a runtime.
		fn get_last_timestamp() -> u64;
	}
}

impl_runtime_apis! {
	impl sp_api::Core<Block> for Runtime {
		fn version() -> RuntimeVersion {
			VERSION
		}

		fn execute_block(block: <Block as BlockT>::LazyBlock) {
			Executive::execute_block(block)
		}

		fn initialize_block(header: &<Block as BlockT>::Header) -> sp_runtime::ExtrinsicInclusionMode {
			Executive::initialize_block(header)
		}
	}


	impl cumulus_primitives_aura::AuraUnincludedSegmentApi<Block> for Runtime {
		fn can_build_upon(
			included_hash: <Block as BlockT>::Hash,
			slot: cumulus_primitives_aura::Slot,
		) -> bool {
			ConsensusHook::can_build_upon(included_hash, slot)
		}
	}

	impl cumulus_primitives_core::RelayParentOffsetApi<Block> for Runtime {
		fn relay_parent_offset() -> u32 {
			relay_parent_offset()
		}

		fn max_claim_queue_offset() -> u8 {
			cumulus_pallet_parachain_system::Pallet::<Runtime>::max_claim_queue_offset()
		}
	}

	impl cumulus_primitives_core::SchedulingV3EnabledApi<Block> for Runtime {
		fn scheduling_v3_enabled() -> bool {
			<Runtime as cumulus_pallet_parachain_system::Config>::SchedulingSignatureVerifier::V3_SCHEDULING_ENABLED
		}
	}

	impl sp_consensus_aura::AuraApi<Block, AuraId> for Runtime {
		fn slot_duration() -> sp_consensus_aura::SlotDuration {
			sp_consensus_aura::SlotDuration::from_millis(slot_duration())
		}

		fn authorities() -> Vec<AuraId> {
			pallet_aura::Authorities::<Runtime>::get().into_inner()
		}
	}

	impl sp_api::Metadata<Block> for Runtime {
		fn metadata() -> OpaqueMetadata {
			OpaqueMetadata::new(Runtime::metadata().into())
		}

		fn metadata_at_version(version: u32) -> Option<OpaqueMetadata> {
			Runtime::metadata_at_version(version)
		}

		fn metadata_versions() -> Vec<u32> {
			Runtime::metadata_versions()
		}
	}

	impl frame_system_rpc_runtime_api::AccountNonceApi<Block, AccountId, Nonce> for Runtime {
		fn account_nonce(account: AccountId) -> Nonce {
			System::account_nonce(account)
		}
	}

	impl sp_block_builder::BlockBuilder<Block> for Runtime {
		fn apply_extrinsic(
			extrinsic: <Block as BlockT>::Extrinsic,
		) -> ApplyExtrinsicResult {
			Executive::apply_extrinsic(extrinsic)
		}

		fn finalize_block() -> <Block as BlockT>::Header {
			Executive::finalize_block()
		}

		fn inherent_extrinsics(data: sp_inherents::InherentData) -> Vec<<Block as BlockT>::Extrinsic> {
			data.create_extrinsics()
		}

		fn check_inherents(block: <Block as BlockT>::LazyBlock, data: sp_inherents::InherentData) -> sp_inherents::CheckInherentsResult {
			data.check_extrinsics(&block)
		}

	}

	impl sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block> for Runtime {
		fn validate_transaction(
			source: TransactionSource,
			tx: <Block as BlockT>::Extrinsic,
			block_hash: <Block as BlockT>::Hash,
		) -> TransactionValidity {
			Executive::validate_transaction(source, tx, block_hash)
		}
	}

	impl sp_offchain::OffchainWorkerApi<Block> for Runtime {
		fn offchain_worker(header: &<Block as BlockT>::Header) {
			Executive::offchain_worker(header)
		}
	}

	impl sp_session::SessionKeys<Block> for Runtime {
		fn decode_session_keys(
			encoded: Vec<u8>,
		) -> Option<Vec<(Vec<u8>, sp_core::crypto::KeyTypeId)>> {
			SessionKeys::decode_into_raw_public_keys(&encoded)
		}

		fn generate_session_keys(owner: Vec<u8>, seed: Option<Vec<u8>>) -> sp_session::OpaqueGeneratedSessionKeys {
			SessionKeys::generate(&owner, seed).into()
		}
	}

	impl crate::GetLastTimestamp<Block> for Runtime {
		fn get_last_timestamp() -> u64 {
			Now::<Runtime>::get()
		}
	}

	impl cumulus_primitives_core::CollectCollationInfo<Block> for Runtime {
		fn collect_collation_info(header: &<Block as BlockT>::Header) -> cumulus_primitives_core::CollationInfo {
			ParachainSystem::collect_collation_info(header)
		}
	}

	impl sp_genesis_builder::GenesisBuilder<Block> for Runtime {
		fn build_state(config: Vec<u8>) -> sp_genesis_builder::Result {
			build_state::<RuntimeGenesisConfig>(config)
		}

		fn get_preset(id: &Option<sp_genesis_builder::PresetId>) -> Option<Vec<u8>> {
			get_preset::<RuntimeGenesisConfig>(id, genesis_config_presets::get_preset)
		}

		fn preset_names() -> Vec<sp_genesis_builder::PresetId> {
			genesis_config_presets::preset_names()
		}
	}

	impl cumulus_primitives_core::GetParachainInfo<Block> for Runtime {
		fn parachain_id() -> ParaId {
			ParachainInfo::parachain_id()
		}
	}

	impl cumulus_primitives_core::TargetBlockRate<Block> for Runtime {
		fn target_block_rate() -> u32 {
			block_processing_velocity()
		}
	}

	impl cumulus_primitives_core::KeyToIncludeInRelayProof<Block> for Runtime {
		fn keys_to_prove() -> cumulus_primitives_core::RelayProofRequest {
			use cumulus_primitives_core::RelayStorageKey;
			RelayProofRequest {
				keys: vec![
					// Request a key to verify its inclusion in the proof.
					RelayStorageKey::Top(test_pallet::relay_alice_account_key()),
				],
			}
		}
	}

	impl sp_authority_discovery::AuthorityDiscoveryApi<Block> for Runtime {
		// Return the current authority set in authoring (session/validator-index) order,
		fn authorities() -> Vec<AuthorityDiscoveryId> {
			#[cfg(feature = "with-authority-discovery")]
			{ AuthorityDiscovery::current_authorities().to_vec() }
			#[cfg(not(feature = "with-authority-discovery"))]
			{ Vec::new() }
		}
	}
}

cumulus_pallet_parachain_system::register_validate_block! {
	Runtime = Runtime,
	BlockExecutor = cumulus_pallet_aura_ext::BlockExecutor::<Runtime, Executive>,
}
