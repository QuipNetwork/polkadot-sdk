//! # Cumulus
//!
//! Substrate provides a framework ([FRAME]) through which a blockchain node and runtime can easily
//! be created. Cumulus aims to extend the same approach to creation of Polkadot parachains.
//!
//! > Cumulus clouds are shaped sort of like dots; together they form a system that is intricate,
//! > beautiful and functional.
//!
//! ## Example: Runtime
//!
//! A Cumulus-based runtime is fairly similar to other [FRAME]-based runtimes. Most notably, the
//! following changes are applied to a normal FRAME-based runtime to make it a Cumulus-based
//! runtime:
//!
//! #### Cumulus Pallets
//!
//! A parachain runtime should use a number of pallets that are provided by Cumulus and Substrate.
//! Notably:
//!
//! - [`frame-system`](frame::prelude::frame_system), like all FRAME-based runtimes.
//! - [`cumulus_pallet_parachain_system`]
//! - [`parachain_info`]
#![doc = docify::embed!("./src/polkadot_sdk/cumulus.rs", system_pallets)]
//! Given that all Cumulus-based runtimes use a simple Aura-based consensus mechanism, the following
//! pallets also need to be added:
//!
//! - [`pallet_timestamp`]
//! - [`pallet_aura`]
//! - [`cumulus_pallet_aura_ext`]
#![doc = docify::embed!("./src/polkadot_sdk/cumulus.rs", consensus_pallets)]
//! Finally, a separate macro, similar to
//! [`impl_runtime_api`](frame::runtime::prelude::impl_runtime_apis), which creates the default set
//! of runtime APIs, will generate the parachain runtime's validation runtime API, also known as
//! parachain validation function (PVF). Without this API, the relay chain is unable to validate
//! blocks produced by our parachain.
#![doc = docify::embed!("./src/polkadot_sdk/cumulus.rs", validate_block)]
//! ---
//!
//! [FRAME]: crate::polkadot_sdk::frame_runtime

#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]

#[cfg(test)]
mod tests {
	mod runtime {
		pub use frame::{
			deps::sp_consensus_aura::sr25519::AuthorityId as AuraId, prelude::*,
			runtime::prelude::*, testing_prelude::*,
		};

		#[docify::export(CR)]
		construct_runtime!(
			pub enum Runtime {
				// system-level pallets.
				System: frame_system,
				Timestamp: pallet_timestamp,
				ParachainSystem: cumulus_pallet_parachain_system,
				ParachainInfo: parachain_info,

				// parachain consensus support -- mandatory.
				Aura: pallet_aura,
				AuraExt: cumulus_pallet_aura_ext,
			}
		);

		#[docify::export]
		mod system_pallets {
			use super::*;

			#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
			impl frame_system::Config for Runtime {
				type Block = MockBlock<Self>;
				type OnSetCode = cumulus_pallet_parachain_system::ParachainSetCode<Self>;
			}

			impl cumulus_pallet_parachain_system::Config for Runtime {
				type RuntimeEvent = RuntimeEvent;
				type OnSystemEvent = ();
				type SelfParaId = parachain_info::Pallet<Runtime>;
				type OutboundXcmpMessageSource = ();
				type XcmpMessageHandler = ();
				type ReservedDmpWeight = ();
				type ReservedXcmpWeight = ();
				type CheckAssociatedRelayNumber =
					cumulus_pallet_parachain_system::RelayNumberMonotonicallyIncreases;
				type ConsensusHook = cumulus_pallet_aura_ext::FixedVelocityConsensusHook<
					Runtime,
					6000, // relay chain block time
					1,
					1,
				>;
				type WeightInfo = ();
				type DmpQueue = frame::traits::EnqueueWithOrigin<(), sp_core::ConstU8<0>>;
				type RelayParentOffset = ConstU32<0>;
				type SchedulingSignatureVerifier = ();
			}

			impl parachain_info::Config for Runtime {}
		}

		#[docify::export]
		mod consensus_pallets {
			use super::*;

			impl pallet_aura::Config for Runtime {
				type AuthorityId = AuraId;
				type DisabledValidators = ();
				type MaxAuthorities = ConstU32<100_000>;
				type AllowMultipleBlocksPerSlot = ConstBool<false>;
				type SlotDuration = pallet_aura::MinimumPeriodTimesTwo<Self>;
			}

			#[docify::export(timestamp)]
			#[derive_impl(pallet_timestamp::config_preludes::TestDefaultConfig)]
			impl pallet_timestamp::Config for Runtime {}

			impl cumulus_pallet_aura_ext::Config for Runtime {}
		}

		#[docify::export(validate_block)]
		cumulus_pallet_parachain_system::register_validate_block! {
			Runtime = Runtime,
			BlockExecutor = cumulus_pallet_aura_ext::BlockExecutor::<Runtime, Executive>,
		}
	}
}
