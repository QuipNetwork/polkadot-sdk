// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

mod approval_voting_coalescing;
mod approved_peer_mixed_collators;
mod approved_peer_mixed_validators;
mod async_backing_6_seconds_rate;
mod beefy_and_mmr;
mod chunk_fetching_network_compatibility;
mod collation_protocol_version_negotiation;
mod collators_reputation_persistence;
mod coretime_collation_fetching_fairness;
mod coretime_partitioning;
mod coretime_shared_core;
mod dispute_freshly_finalized;
mod dispute_old_finalized;
mod duplicate_collations;
mod parachains_disputes;
mod parachains_disputes_garbage_candidate;
mod parachains_max_tranche0;
mod parachains_pvf;
mod scheduling_v3;
mod shared_core_idle_parachain;
mod spam_statement_distribution_requests;
mod sync_backing;
mod systematic_chunk_recovery;
mod v3_dynamic_enablement;
mod v3_rolling_upgrade;
mod validator_disabling;
