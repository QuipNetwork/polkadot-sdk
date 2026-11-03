//! # Upgrade Parachain for Asynchronous Backing Compatibility
//!
//! This guide is relevant for cumulus based parachain projects started in 2023 or before, whose
//! backing process is synchronous where parablocks can only be built on the latest Relay Chain
//! block. Async Backing allows collators to build parablocks on older Relay Chain blocks and create
//! pipelines of multiple pending parablocks. This parallel block generation increases efficiency
//! and throughput. For more information on Async backing and its terminology, refer to the document
//! on [the Polkadot SDK docs.](https://paritytech.github.io/polkadot-sdk/master/polkadot_sdk_docs/guides/async_backing_guide/index.html)
//!
//! > If starting a new parachain project, please use an async backing compatible template such as
//! > the
//! > [parachain template](https://github.com/paritytech/polkadot-sdk/tree/master/templates/parachain).
//! The rollout process for Async Backing has three phases. Phases 1 and 2 below put new
//! infrastructure in place. Then we can simply turn on async backing in phase 3.
//!
//! ## Prerequisite
//!
//! The relay chain needs to have async backing enabled so double-check that the relay-chain
//! configuration contains the following parameter (especially when testing locally e.g. with
//! zombienet):
//!
//! ```json
//! "scheduler_params": {
//!     "lookahead": 3
//! }
//! ```
//!
//! <div class="warning"><code>lookahead</code> must be set to at least 3, otherwise parachain
//! block times will degrade to worse than with sync backing!</div>
//!
//! Note: `async_backing_params` (`max_candidate_depth` and `allowed_ancestry_len`) are no longer
//! required to be explicitly configured. Async backing works with them set to their default values
//! of 0.
//!
//! ## Phase 1 - Update Parachain Runtime
//!
//! This phase involves configuring your parachain’s runtime `/runtime/src/lib.rs` to make use of
//! async backing system.
//!
//! 1. Establish and ensure constants for `capacity` and `velocity` in the runtime.
//! 2. Establish and ensure the constant relay chain slot duration measured in milliseconds equal to
//!    `6000` in the runtime.
//! ```rust
//! // Maximum number of blocks simultaneously accepted by the Runtime, not yet included into the
//! // relay chain.
//! pub const UNINCLUDED_SEGMENT_CAPACITY: u32 = 3;
//! // How many parachain blocks are processed by the relay chain per parent. Limits the number of
//! // blocks authored per slot.
//! pub const BLOCK_PROCESSING_VELOCITY: u32 = 1;
//! // Relay chain slot duration, in milliseconds.
//! pub const RELAY_CHAIN_SLOT_DURATION_MILLIS: u32 = 6000;
//! ```
//!
//! 3. Establish constants `MILLISECS_PER_BLOCK` and `SLOT_DURATION` if not already present in the
//!    runtime.
//! ```ignore
//! // `SLOT_DURATION` is picked up by `pallet_timestamp` which is in turn picked
//! // up by `pallet_aura` to implement `fn slot_duration()`.
//! //
//! // Change this to adjust the block time.
//! pub const MILLISECS_PER_BLOCK: u64 = 12000;
//! pub const SLOT_DURATION: u64 = MILLISECS_PER_BLOCK;
//! ```
//!
//! 4. Configure `cumulus_pallet_parachain_system` in the runtime.
//!
//! - Define a `FixedVelocityConsensusHook` using our capacity, velocity, and relay slot duration
//! constants. Use this to set the parachain system `ConsensusHook` property.
#![doc = docify::embed!("../../templates/parachain/runtime/src/lib.rs", ConsensusHook)]
//! ```ignore
//! impl cumulus_pallet_parachain_system::Config for Runtime {
//!     ..
//!     type ConsensusHook = ConsensusHook;
//!     ..
//! }
//! ```
//! - Set the parachain system property `CheckAssociatedRelayNumber` to
//! `RelayNumberMonotonicallyIncreases`
//! ```ignore
//! impl cumulus_pallet_parachain_system::Config for Runtime {
//! 	..
//! 	type CheckAssociatedRelayNumber = RelayNumberMonotonicallyIncreases;
//! 	..
//! }
//! ```
//!
//! 5. Configure `pallet_aura` in the runtime.
//!
//! - Set `AllowMultipleBlocksPerSlot` to `true` to allow multiple blocks per slot.
//!
//! - Define `pallet_aura::SlotDuration` using our constant `SLOT_DURATION`
//! ```ignore
//! impl pallet_aura::Config for Runtime {
//! 	..
//! 	type AllowMultipleBlocksPerSlot = ConstBool<true>;
//! 	type SlotDuration = ConstU64<SLOT_DURATION>;
//! 	..
//! }
//! ```
//!
//! 6. Update `sp_consensus_aura::AuraApi::slot_duration` in `sp_api::impl_runtime_apis` to match
//!    the constant `SLOT_DURATION`
#![doc = docify::embed!("../../templates/parachain/runtime/src/apis.rs", impl_slot_duration)]
//! 7. Implement the `AuraUnincludedSegmentApi`, which allows the collator client to query its
//!    runtime to determine whether it should author a block.
//!
//!    - Add the dependency `cumulus-primitives-aura` to the `runtime/Cargo.toml` file for your
//!      runtime
//! ```ignore
//! ..
//! cumulus-primitives-aura = { path = "../../../../primitives/aura", default-features = false }
//! ..
//! ```
//!
//! - In the same file, add `"cumulus-primitives-aura/std",` to the `std` feature.
//!
//! - Inside the `impl_runtime_apis!` block for your runtime, implement the
//!   `cumulus_primitives_aura::AuraUnincludedSegmentApi` as shown below.
#![doc = docify::embed!("../../templates/parachain/runtime/src/apis.rs", impl_can_build_upon)]
//! **Note:** With the default capacity of 3 and velocity of 1, a single parachain block is
//! authored per relay chain block, giving a 6 second parachain block time.
//!
//! 8. If your `runtime/src/lib.rs` provides a `CheckInherents` type to `register_validate_block`,
//!    remove it. `FixedVelocityConsensusHook` makes it unnecessary. The following example shows how
//!    `register_validate_block` should look after removing `CheckInherents`.
#![doc = docify::embed!("../../templates/parachain/runtime/src/lib.rs", register_validate_block)]
//! ## Phase 2 - Update Parachain Nodes
//!
//! This phase consists of plugging in the new lookahead collator node.
//!
//! 1. Import `cumulus_primitives_core::ValidationCode` to `node/src/service.rs`.
#![doc = docify::embed!("../../templates/parachain/node/src/service.rs", cumulus_primitives)]
//! 2. In `node/src/service.rs`, modify `sc_service::spawn_tasks` to use a clone of `Backend` rather
//!    than the original
//! ```ignore
//! sc_service::spawn_tasks(sc_service::SpawnTasksParams {
//!     ..
//!     backend: backend.clone(),
//!     ..
//! })?;
//! ```
//!
//! 3. Add `backend` as a parameter to `start_consensus()` in `node/src/service.rs`
//! ```text
//! fn start_consensus(
//!     ..
//!     backend: Arc<ParachainBackend>,
//!     ..
//! ```
//! ```ignore
//! if validator {
//!   start_consensus(
//!     ..
//!     backend.clone(),
//!     ..
//!    )?;
//! }
//! ```
//!
//! 4. In `node/src/service.rs` import the lookahead collator rather than the basic collator
#![doc = docify::embed!("../../templates/parachain/node/src/service.rs", lookahead_collator)]
//! 5. In `start_consensus()` replace the `BasicAuraParams` struct with `AuraParams`
//!    - Change the struct type from `BasicAuraParams` to `AuraParams`
//!    - In the `para_client` field, pass in a cloned para client rather than the original
//!    - Add a `para_backend` parameter after `para_client`, passing in our para backend
//!    - Provide a `code_hash_provider` closure like that shown below
//!    - Increase `authoring_duration` from 500 milliseconds to 2000
//! ```ignore
//! let params = AuraParams {
//!     ..
//!     para_client: client.clone(),
//!     para_backend: backend.clone(),
//!     ..
//!     code_hash_provider: move |block_hash| {
//!         client.code_at(block_hash).ok().map(|c| ValidationCode::from(c).hash())
//!     },
//!     ..
//!     authoring_duration: Duration::from_millis(2000),
//!     ..
//! };
//! ```
//!
//! **Note:** Set `authoring_duration` to whatever you want, taking your own hardware into account.
//! But if the backer who should be slower than you due to reading from disk, times out at two
//! seconds your candidates will be rejected.
//!
//! 6. In `start_consensus()` replace `basic_aura::run` with `aura::run`
//! ```ignore
//! let fut =
//! aura::run::<Block, sp_consensus_aura::sr25519::AuthorityPair, _, _, _, _, _, _, _, _, _>(
//!    params,
//! );
//! task_manager.spawn_essential_handle().spawn("aura", None, fut);
//! ```
//!
//! ## Phase 3 - Activate Async Backing
//!
//! This phase consists of changes to your parachain’s runtime that activate async backing feature.
//!
//! 1. Verify `pallet_aura` has `AllowMultipleBlocksPerSlot` set to `true` in `runtime/src/lib.rs`
//!    (this should already be done from Phase 1).
#![doc = docify::embed!("../../templates/parachain/runtime/src/configs/mod.rs", aura_config)]
//! 2. Verify `UNINCLUDED_SEGMENT_CAPACITY` is set to at least `3` in `runtime/src/lib.rs`.
#![doc = docify::embed!("../../templates/parachain/runtime/src/lib.rs", async_backing_params)]
//! 3. Decrease `MILLISECS_PER_BLOCK` to 6000.
//!
//! - Note: For a parachain which measures time in terms of its own block number rather than by
//!   relay block number it may be preferable to increase velocity. Changing block time may cause
//!   complications, requiring additional changes. See the section “Timing by Block Number”.
#![doc = docify::embed!("../../templates/parachain/runtime/src/lib.rs", block_times)]
//! 4. Update `MAXIMUM_BLOCK_WEIGHT` to reflect the increased time available for block production.
#![doc = docify::embed!("../../templates/parachain/runtime/src/lib.rs", max_block_weight)]
//! 5. Set `MinimumPeriod` to `0` in `pallet_timestamp`. This is required to allow multiple blocks
//!    within the same slot.
//! ```ignore
//! impl pallet_timestamp::Config for Runtime {
//!     ..
//!     type MinimumPeriod = ConstU64<0>;
//!     ..
//! }
//! ```
//!
//! ## Timing by Block Number
//!
//! With asynchronous backing, parachains produce blocks every 6 seconds rather than 12 seconds.
//! This means that any on-chain logic that derives time from parachain block numbers will see
//! time move twice as fast. This could result in expected and actual time not matching up,
//! potentially causing issues with vesting schedules, unlock periods, or other time-dependent
//! logic.
//!
//! The recommended strategy is to rely on relay chain block numbers for timing instead of
//! parachain block numbers. Relay block number is kept track of by each parachain in
//! `pallet-parachain-system` with the storage value `LastRelayChainBlockNumber`. This value can
//! be obtained and used wherever timing based on block number is needed.
//!
//! Alternatively, `pallet_timestamp` provides wall-clock time which is independent of block
//! number and is not affected by changes in block time.

#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
