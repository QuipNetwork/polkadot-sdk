//! # Parachain forks
//!
//! In this guide, we will examine how AURA-based parachains handle forks. AURA (Authority Round) is
//! a consensus mechanism where block authors rotate at fixed time intervals. Each author gets a
//! predetermined time slice during which they are allowed to author a block. On its own, this
//! mechanism is fork-free.
//!
//! However, since the relay chain provides security and serves as the source of truth for
//! parachains, the parachain is dependent on it. This relationship can introduce complexities that
//! lead to forking scenarios.
//!
//! ## Background
//! Each parachain block has a relay parent, which is a relay chain block that provides context to
//! our parachain block. The constraints the relay chain imposes on our parachain can cause forks
//! under certain conditions. With asynchronous-backing enabled chains, the node side is building
//! blocks on all relay chain forks. This means that no matter which fork of the relay chain
//! ultimately progressed, the parachain would have a block ready for that fork. The situation
//! changes when parachains want to produce blocks at a faster cadence. In a scenario where a
//! parachain might author on 3 cores with elastic scaling, it is not possible to author on all
//! relay chain forks. The time constraints do not allow it. Building on two forks would result in 6
//! blocks. The authoring of these blocks would consume more time than we have available before the
//! next relay chain block arrives. This limitation requires a more fork-resistant approach to
//! block-building.
//!
//! ## Impact of Forks
//! When a relay chain fork occurs and the parachain builds on a fork that will not be extended in
//! the future, the blocks built on that fork are lost and need to be rebuilt. This increases
//! latency and reduces throughput, affecting the overall performance of the parachain.
//!
//! # Building on Older Pelay Parents
//! Cumulus offers a way to mitigate the occurence of forks. Instead of picking a block at the tip
//! of the relay chain to build blocks, the node side can pick a relay chain block that is older. By
//! building on 12s old relay chain blocks, forks will already have settled and the parachain can
//! build fork-free.
//!
//! ```text
//! Without offset:
//! Relay Chain:    A --- B --- C --- D  --- E
//!                              \
//!                               --- D' --- E'
//! Parachain:            X --- Y --- ? (builds on both D and D', wasting resources)
//!
//! With offset (2 blocks):
//! Relay Chain:    A --- B --- C --- D  --- E
//!                              \
//!                               --- D' --- E'
//! Parachain:            X(A) - Y (B) - Z (on C, fork already resolved)
//! ```
//! **Note:** It is possible that relay chain forks extend over more than 1-2 blocks. However, it is
//! unlikely.
//! ## Tradeoffs
//! Fork-free parachains come with a few tradeoffs:
//! - The latency of incoming XCM messages will be delayed by `N * 6s`, where `N` is the number of
//!   relay chain blocks we want to offset by. For example, by building 2 relay chain blocks behind
//!   the tip, the XCM latency will be increased by 12 seconds.
//! - The available PoV space will be slightly reduced. Assuming a 10mb PoV, parachains need to be
//!   ready to sacrifice around 0.5% of PoV space.
//!
//! ## Enabling Guide
//! The decision whether the parachain should build on older relay parents is embedded into the
//! runtime. After the changes are implemented, the runtime will enforce that no author can build
//! with an offset smaller than the desired offset. If you wish to keep your current parachain
//! behaviour and do not want aforementioned tradeoffs, set the offset to 0.
//!
//! **Note:** The APIs mentioned here are available in `polkadot-sdk` versions after `stable-2506`.
//!
//! 1. Define the relay parent offset your parachain should respect in the runtime.
//! ```ignore
//! const RELAY_PARENT_OFFSET = 2;
//! ```
//! 2. Pass this constant to the `parachain-system` pallet.
//!
//! ```ignore
//! impl cumulus_pallet_parachain_system::Config for Runtime {
//! 	// Other config items here
//!     ...
//! 	type RelayParentOffset = ConstU32<RELAY_PARENT_OFFSET>;
//! 	type SchedulingSignatureVerifier = ();
//! }
//! ```
//! 3. Implement the `RelayParentOffsetApi` runtime API for your runtime.
//!
//! ```ignore
//! impl cumulus_primitives_core::RelayParentOffsetApi<Block> for Runtime {
//!     fn relay_parent_offset() -> u32 {
//! 		RELAY_PARENT_OFFSET
//! 	}
//! }
//! ```
//! 4. Increase the `UNINCLUDED_SEGMENT_CAPACITY` for your runtime. It needs to be increased by
//!    `RELAY_PARENT_OFFSET * BLOCK_PROCESSING_VELOCITY`.
