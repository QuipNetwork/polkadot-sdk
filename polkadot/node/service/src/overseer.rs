// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::{Error, IsParachainNode, Registry};
use polkadot_collator_protocol::ReputationConfig;
use polkadot_node_subsystem_types::{ChainApiBackend, RuntimeApiSubsystemClient};
use polkadot_overseer::{DummySubsystem, InitializedOverseerBuilder, SubsystemError};
use sp_core::traits::SpawnNamed;

use polkadot_availability_distribution::IncomingRequestReceivers;
use polkadot_node_core_approval_voting::Config as ApprovalVotingConfig;
use polkadot_node_core_av_store::Config as AvailabilityConfig;
use polkadot_node_core_candidate_validation::Config as CandidateValidationConfig;
use polkadot_node_core_chain_selection::Config as ChainSelectionConfig;
use polkadot_node_core_dispute_coordinator::Config as DisputeCoordinatorConfig;
use polkadot_node_network_protocol::{
	peer_set::{PeerSet, PeerSetProtocolNames},
	request_response::{
		v1 as request_v1, v2 as request_v2, v3 as request_v3, IncomingRequestReceiver,
		ReqProtocolNames,
	},
};
#[cfg(any(feature = "malus", test))]
pub use polkadot_overseer::{dummy::dummy_overseer_builder, HeadSupportsParachains};
use polkadot_overseer::{
	metrics::Metrics as OverseerMetrics, MetricsTrait, Overseer, OverseerConnector, OverseerHandle,
	SpawnGlue,
};

use parking_lot::Mutex;
use sc_authority_discovery::Service as AuthorityDiscoveryService;
use sc_client_api::AuxStore;
use sc_keystore::LocalKeystore;
use sc_network::{NetworkStateInfo, NotificationService};
use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

pub use polkadot_approval_distribution::ApprovalDistribution as ApprovalDistributionSubsystem;
pub use polkadot_availability_bitfield_distribution::BitfieldDistribution as BitfieldDistributionSubsystem;
pub use polkadot_availability_distribution::AvailabilityDistributionSubsystem;
pub use polkadot_availability_recovery::AvailabilityRecoverySubsystem;
pub use polkadot_collator_protocol::{CollatorProtocolSubsystem, ProtocolSide};
pub use polkadot_dispute_distribution::DisputeDistributionSubsystem;
pub use polkadot_gossip_support::GossipSupport as GossipSupportSubsystem;
pub use polkadot_network_bridge::{
	Metrics as NetworkBridgeMetrics, NetworkBridgeRx as NetworkBridgeRxSubsystem,
	NetworkBridgeTx as NetworkBridgeTxSubsystem,
};
pub use polkadot_node_collation_generation::CollationGenerationSubsystem;
pub use polkadot_node_core_approval_voting::ApprovalVotingSubsystem;
pub use polkadot_node_core_approval_voting_parallel::{
	ApprovalVotingParallelSubsystem, Metrics as ApprovalVotingParallelMetrics,
};
pub use polkadot_node_core_av_store::AvailabilityStoreSubsystem;
pub use polkadot_node_core_backing::CandidateBackingSubsystem;
pub use polkadot_node_core_bitfield_signing::BitfieldSigningSubsystem;
pub use polkadot_node_core_candidate_validation::CandidateValidationSubsystem;
pub use polkadot_node_core_chain_api::ChainApiSubsystem;
pub use polkadot_node_core_chain_selection::ChainSelectionSubsystem;
pub use polkadot_node_core_dispute_coordinator::DisputeCoordinatorSubsystem;
pub use polkadot_node_core_prospective_parachains::ProspectiveParachainsSubsystem;
pub use polkadot_node_core_provisioner::ProvisionerSubsystem;
pub use polkadot_node_core_pvf_checker::PvfCheckerSubsystem;
pub use polkadot_node_core_runtime_api::RuntimeApiSubsystem;
pub use polkadot_statement_distribution::StatementDistributionSubsystem;

/// Arguments passed for overseer construction.
pub struct OverseerGenArgs<'a, Spawner, RuntimeClient>
where
	Spawner: 'static + SpawnNamed + Clone + Unpin,
{
	/// Runtime client generic, providing the `ProvideRuntimeApi` trait besides others.
	pub runtime_client: Arc<RuntimeClient>,
	/// Underlying network service implementation.
	pub network_service: Arc<dyn sc_network::service::traits::NetworkService>,
	/// Underlying syncing service implementation.
	pub sync_service: Arc<dyn sp_consensus::SyncOracle + Send + Sync>,
	/// Underlying authority discovery service.
	pub authority_discovery_service: AuthorityDiscoveryService,
	/// Collations request receiver for network protocol v2.
	pub collation_req_v2_receiver: IncomingRequestReceiver<request_v2::CollationFetchingRequest>,
	/// Collations request receiver for network protocol v3.
	pub collation_req_v3_receiver: IncomingRequestReceiver<request_v3::CollationFetchingRequest>,
	/// Receiver for available data requests.
	pub available_data_req_receiver:
		IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	/// Prometheus registry, commonly used for production systems, less so for test.
	pub registry: Option<&'a Registry>,
	/// Task spawner to be used throughout the overseer and the APIs it provides.
	pub spawner: Spawner,
	/// Determines the behavior of the collator.
	pub is_parachain_node: IsParachainNode,
	/// Overseer channel capacity override.
	pub overseer_message_channel_capacity_override: Option<usize>,
	/// Request-response protocol names source.
	pub req_protocol_names: ReqProtocolNames,
	/// `PeerSet` protocol names to protocols mapping.
	pub peerset_protocol_names: PeerSetProtocolNames,
	/// Notification services for validation/collation protocols.
	pub notification_services: HashMap<PeerSet, Box<dyn NotificationService>>,
}

pub struct ExtendedOverseerGenArgs {
	/// The keystore to use for i.e. validator keys.
	pub keystore: Arc<LocalKeystore>,
	/// The underlying key value store for the parachains.
	pub parachains_db: Arc<dyn polkadot_node_subsystem_util::database::Database>,
	/// Configuration for the candidate validation subsystem.
	pub candidate_validation_config: Option<CandidateValidationConfig>,
	/// Configuration for the availability store subsystem.
	pub availability_config: AvailabilityConfig,
	/// POV request receiver.
	pub pov_req_receiver: IncomingRequestReceiver<request_v1::PoVFetchingRequest>,
	/// Erasure chunk request v1 receiver.
	pub chunk_req_v1_receiver: IncomingRequestReceiver<request_v1::ChunkFetchingRequest>,
	/// Erasure chunk request v2 receiver.
	pub chunk_req_v2_receiver: IncomingRequestReceiver<request_v2::ChunkFetchingRequest>,
	/// Receiver for incoming candidate requests.
	pub candidate_req_v2_receiver: IncomingRequestReceiver<request_v2::AttestedCandidateRequest>,
	/// Configuration for the approval voting subsystem.
	pub approval_voting_config: ApprovalVotingConfig,
	/// Receiver for incoming disputes.
	pub dispute_req_receiver: IncomingRequestReceiver<request_v1::DisputeRequest>,
	/// Configuration for the dispute coordinator subsystem.
	pub dispute_coordinator_config: DisputeCoordinatorConfig,
	/// Configuration for the chain selection subsystem.
	pub chain_selection_config: ChainSelectionConfig,
	/// Optional availability recovery fetch chunks threshold. If PoV size size is lower
	/// than the value put in here we always try to recovery availability from backers.
	/// The presence of this parameter here is needed to have different values per chain.
	pub fetch_chunks_threshold: Option<usize>,
	/// Set of invulnerable AH collator `PeerId`s
	pub invulnerable_ah_collators: HashSet<polkadot_node_network_protocol::PeerId>,
	/// Override for `HOLD_OFF_DURATION` constant .
	pub collator_protocol_hold_off: Option<Duration>,
	/// Use experimental collator protocol
	pub experimental_collator_protocol: bool,
	/// Reputation DB config used by experimental collator protocol,
	pub reputation_config: ReputationConfig,
}

/// Obtain a prepared validator `Overseer`, that is initialized with all default values.
pub fn validator_overseer_builder<Spawner, RuntimeClient>(
	OverseerGenArgs {
		runtime_client,
		network_service,
		sync_service,
		authority_discovery_service,
		collation_req_v2_receiver: _,
		collation_req_v3_receiver: _,
		available_data_req_receiver,
		registry,
		spawner,
		is_parachain_node,
		overseer_message_channel_capacity_override,
		req_protocol_names,
		peerset_protocol_names,
		notification_services,
	}: OverseerGenArgs<Spawner, RuntimeClient>,
	ExtendedOverseerGenArgs {
		keystore,
		parachains_db,
		candidate_validation_config,
		availability_config,
		pov_req_receiver,
		chunk_req_v1_receiver,
		chunk_req_v2_receiver,
		candidate_req_v2_receiver,
		approval_voting_config,
		dispute_req_receiver,
		dispute_coordinator_config,
		chain_selection_config,
		fetch_chunks_threshold,
		invulnerable_ah_collators,
		collator_protocol_hold_off,
		experimental_collator_protocol,
		reputation_config,
	}: ExtendedOverseerGenArgs,
) -> Result<
	InitializedOverseerBuilder<
		SpawnGlue<Spawner>,
		Arc<RuntimeClient>,
		CandidateValidationSubsystem,
		PvfCheckerSubsystem,
		CandidateBackingSubsystem,
		StatementDistributionSubsystem,
		AvailabilityDistributionSubsystem,
		AvailabilityRecoverySubsystem,
		BitfieldSigningSubsystem,
		BitfieldDistributionSubsystem,
		ProvisionerSubsystem,
		RuntimeApiSubsystem<RuntimeClient>,
		AvailabilityStoreSubsystem,
		NetworkBridgeRxSubsystem<
			Arc<dyn sc_network::service::traits::NetworkService>,
			AuthorityDiscoveryService,
		>,
		NetworkBridgeTxSubsystem<
			Arc<dyn sc_network::service::traits::NetworkService>,
			AuthorityDiscoveryService,
		>,
		ChainApiSubsystem<RuntimeClient>,
		DummySubsystem,
		CollatorProtocolSubsystem,
		DummySubsystem,
		DummySubsystem,
		ApprovalVotingParallelSubsystem,
		GossipSupportSubsystem<AuthorityDiscoveryService>,
		DisputeCoordinatorSubsystem,
		DisputeDistributionSubsystem<AuthorityDiscoveryService>,
		ChainSelectionSubsystem,
		ProspectiveParachainsSubsystem,
	>,
	Error,
>
where
	RuntimeClient: RuntimeApiSubsystemClient + ChainApiBackend + AuxStore + 'static,
	Spawner: 'static + SpawnNamed + Clone + Unpin,
{
	use polkadot_node_subsystem_util::metrics::Metrics;

	let metrics = <OverseerMetrics as MetricsTrait>::register(registry)?;
	let notification_sinks = Arc::new(Mutex::new(HashMap::new()));

	let spawner = SpawnGlue(spawner);

	let network_bridge_metrics: NetworkBridgeMetrics = Metrics::register(registry)?;
	let approval_voting_parallel_metrics: ApprovalVotingParallelMetrics =
		Metrics::register(registry)?;
	let builder = Overseer::builder()
		.network_bridge_tx(NetworkBridgeTxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			network_bridge_metrics.clone(),
			req_protocol_names.clone(),
			peerset_protocol_names.clone(),
			notification_sinks.clone(),
		))
		.network_bridge_rx(NetworkBridgeRxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			Box::new(sync_service.clone()),
			network_bridge_metrics,
			peerset_protocol_names,
			notification_services,
			notification_sinks,
		))
		.availability_distribution(AvailabilityDistributionSubsystem::new(
			keystore.clone(),
			IncomingRequestReceivers {
				pov_req_receiver,
				chunk_req_v1_receiver,
				chunk_req_v2_receiver,
			},
			req_protocol_names.clone(),
			Metrics::register(registry)?,
		))
		.availability_recovery(AvailabilityRecoverySubsystem::for_validator(
			fetch_chunks_threshold,
			available_data_req_receiver,
			&req_protocol_names,
			Metrics::register(registry)?,
		))
		.availability_store(AvailabilityStoreSubsystem::new(
			parachains_db.clone(),
			availability_config,
			Box::new(sync_service.clone()),
			Metrics::register(registry)?,
		))
		.bitfield_distribution(BitfieldDistributionSubsystem::new(Metrics::register(registry)?))
		.bitfield_signing(BitfieldSigningSubsystem::new(
			keystore.clone(),
			Metrics::register(registry)?,
		))
		.candidate_backing(CandidateBackingSubsystem::new(
			keystore.clone(),
			Metrics::register(registry)?,
		))
		.candidate_validation(CandidateValidationSubsystem::with_config(
			candidate_validation_config,
			keystore.clone(),
			Metrics::register(registry)?, // candidate-validation metrics
			Metrics::register(registry)?, // validation host metrics
		))
		.pvf_checker(PvfCheckerSubsystem::new(keystore.clone(), Metrics::register(registry)?))
		.chain_api(ChainApiSubsystem::new(runtime_client.clone(), Metrics::register(registry)?))
		.collation_generation(DummySubsystem)
		.collator_protocol({
			let side = match is_parachain_node {
				IsParachainNode::Collator(_) | IsParachainNode::FullNode => {
					return Err(Error::Overseer(SubsystemError::Context(
						"build validator overseer for parachain node".to_owned(),
					)))
				},
				IsParachainNode::No => {
					if experimental_collator_protocol {
						ProtocolSide::ValidatorExperimental {
							keystore: keystore.clone(),
							metrics: Metrics::register(registry)?,
							db: parachains_db.clone(),
							reputation_config,
							clock: polkadot_node_clock::system_clock(),
						}
					} else {
						ProtocolSide::Validator {
							keystore: keystore.clone(),
							eviction_policy: Default::default(),
							metrics: Metrics::register(registry)?,
							invulnerables: invulnerable_ah_collators,
							collator_protocol_hold_off,
							clock: polkadot_node_clock::system_clock(),
						}
					}
				},
			};
			CollatorProtocolSubsystem::new(side)
		})
		.provisioner(ProvisionerSubsystem::new(Metrics::register(registry)?))
		.runtime_api(RuntimeApiSubsystem::new(
			runtime_client.clone(),
			Metrics::register(registry)?,
			spawner.clone(),
		))
		.statement_distribution(StatementDistributionSubsystem::new(
			keystore.clone(),
			candidate_req_v2_receiver,
			Metrics::register(registry)?,
		))
		.approval_distribution(DummySubsystem)
		.approval_voting(DummySubsystem)
		.approval_voting_parallel(ApprovalVotingParallelSubsystem::with_config(
			approval_voting_config,
			parachains_db.clone(),
			keystore.clone(),
			Box::new(sync_service.clone()),
			approval_voting_parallel_metrics,
			spawner.clone(),
			overseer_message_channel_capacity_override,
		))
		.gossip_support(GossipSupportSubsystem::new(
			keystore.clone(),
			authority_discovery_service.clone(),
			Metrics::register(registry)?,
		))
		.dispute_coordinator(DisputeCoordinatorSubsystem::new(
			parachains_db.clone(),
			dispute_coordinator_config,
			keystore.clone(),
			Metrics::register(registry)?,
		))
		.dispute_distribution(DisputeDistributionSubsystem::new(
			keystore.clone(),
			dispute_req_receiver,
			authority_discovery_service.clone(),
			Metrics::register(registry)?,
		))
		.chain_selection(ChainSelectionSubsystem::new(chain_selection_config, parachains_db))
		.prospective_parachains(ProspectiveParachainsSubsystem::new(Metrics::register(registry)?))
		.activation_external_listeners(Default::default())
		.active_leaves(Default::default())
		.supports_parachains(runtime_client)
		.metrics(metrics)
		.spawner(spawner);

	let builder = if let Some(capacity) = overseer_message_channel_capacity_override {
		builder.message_channel_capacity(capacity)
	} else {
		builder
	};
	Ok(builder)
}

/// Obtain a prepared collator `Overseer`, that is initialized with all default values.
pub fn collator_overseer_builder<Spawner, RuntimeClient>(
	OverseerGenArgs {
		runtime_client,
		network_service,
		sync_service,
		authority_discovery_service,
		collation_req_v2_receiver,
		collation_req_v3_receiver,
		available_data_req_receiver,
		registry,
		spawner,
		is_parachain_node,
		overseer_message_channel_capacity_override,
		req_protocol_names,
		peerset_protocol_names,
		notification_services,
	}: OverseerGenArgs<Spawner, RuntimeClient>,
) -> Result<
	InitializedOverseerBuilder<
		SpawnGlue<Spawner>,
		Arc<RuntimeClient>,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		AvailabilityRecoverySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		RuntimeApiSubsystem<RuntimeClient>,
		DummySubsystem,
		NetworkBridgeRxSubsystem<
			Arc<dyn sc_network::service::traits::NetworkService>,
			AuthorityDiscoveryService,
		>,
		NetworkBridgeTxSubsystem<
			Arc<dyn sc_network::service::traits::NetworkService>,
			AuthorityDiscoveryService,
		>,
		ChainApiSubsystem<RuntimeClient>,
		CollationGenerationSubsystem,
		CollatorProtocolSubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
	>,
	Error,
>
where
	Spawner: 'static + SpawnNamed + Clone + Unpin,
	RuntimeClient: RuntimeApiSubsystemClient + ChainApiBackend + AuxStore + 'static,
{
	use polkadot_node_subsystem_util::metrics::Metrics;

	let notification_sinks = Arc::new(Mutex::new(HashMap::new()));

	let spawner = SpawnGlue(spawner);

	let network_bridge_metrics: NetworkBridgeMetrics = Metrics::register(registry)?;

	let builder = Overseer::builder()
		.network_bridge_tx(NetworkBridgeTxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			network_bridge_metrics.clone(),
			req_protocol_names.clone(),
			peerset_protocol_names.clone(),
			notification_sinks.clone(),
		))
		.network_bridge_rx(NetworkBridgeRxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			Box::new(sync_service.clone()),
			network_bridge_metrics,
			peerset_protocol_names,
			notification_services,
			notification_sinks,
		))
		.availability_distribution(DummySubsystem)
		.availability_recovery(AvailabilityRecoverySubsystem::for_collator(
			None,
			available_data_req_receiver,
			&req_protocol_names,
			Metrics::register(registry)?,
		))
		.availability_store(DummySubsystem)
		.bitfield_distribution(DummySubsystem)
		.bitfield_signing(DummySubsystem)
		.candidate_backing(DummySubsystem)
		.candidate_validation(DummySubsystem)
		.pvf_checker(DummySubsystem)
		.chain_api(ChainApiSubsystem::new(runtime_client.clone(), Metrics::register(registry)?))
		.collation_generation(CollationGenerationSubsystem::new(Metrics::register(registry)?))
		.collator_protocol({
			let side = match is_parachain_node {
				IsParachainNode::No => {
					return Err(Error::Overseer(SubsystemError::Context(
						"build parachain node overseer for validator".to_owned(),
					)))
				},
				IsParachainNode::Collator(collator_pair) => ProtocolSide::Collator {
					peer_id: network_service.local_peer_id(),
					collator_pair,
					request_receiver_v2: collation_req_v2_receiver,
					request_receiver_v3: collation_req_v3_receiver,
					metrics: Metrics::register(registry)?,
					clock: polkadot_node_clock::system_clock(),
				},
				IsParachainNode::FullNode => ProtocolSide::None,
			};
			CollatorProtocolSubsystem::new(side)
		})
		.provisioner(DummySubsystem)
		.runtime_api(RuntimeApiSubsystem::new(
			runtime_client.clone(),
			Metrics::register(registry)?,
			spawner.clone(),
		))
		.statement_distribution(DummySubsystem)
		.approval_distribution(DummySubsystem)
		.approval_voting(DummySubsystem)
		.approval_voting_parallel(DummySubsystem)
		.gossip_support(DummySubsystem)
		.dispute_coordinator(DummySubsystem)
		.dispute_distribution(DummySubsystem)
		.chain_selection(DummySubsystem)
		.prospective_parachains(DummySubsystem)
		.activation_external_listeners(Default::default())
		.active_leaves(Default::default())
		.supports_parachains(runtime_client)
		.metrics(Metrics::register(registry)?)
		.spawner(spawner);

	let builder = if let Some(capacity) = overseer_message_channel_capacity_override {
		builder.message_channel_capacity(capacity)
	} else {
		builder
	};
	Ok(builder)
}

/// Trait for the `fn` generating the overseer.
pub trait OverseerGen {
	/// Overwrite the full generation of the overseer, including the subsystems.
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<Spawner, RuntimeClient>,
		ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<(Overseer<SpawnGlue<Spawner>, Arc<RuntimeClient>>, OverseerHandle), Error>
	where
		RuntimeClient: RuntimeApiSubsystemClient + ChainApiBackend + AuxStore + 'static,
		Spawner: 'static + SpawnNamed + Clone + Unpin;

	// It would be nice to make `create_subsystems` part of this trait,
	// but the amount of generic arguments that would be required as
	// as consequence make this rather annoying to implement and use.
}

/// The regular set of subsystems.
pub struct ValidatorOverseerGen;

impl OverseerGen for ValidatorOverseerGen {
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<Spawner, RuntimeClient>,
		ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<(Overseer<SpawnGlue<Spawner>, Arc<RuntimeClient>>, OverseerHandle), Error>
	where
		RuntimeClient: RuntimeApiSubsystemClient + ChainApiBackend + AuxStore + 'static,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		let ext_args = ext_args.ok_or(Error::Overseer(SubsystemError::Context(
			"create validator overseer as mandatory extended arguments were not provided"
				.to_owned(),
		)))?;
		validator_overseer_builder(args, ext_args)?
			.build_with_connector(connector)
			.map_err(|e| e.into())
	}
}

/// Reduced set of subsystems, to use in collator and collator's full node.
pub struct CollatorOverseerGen;

impl OverseerGen for CollatorOverseerGen {
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<Spawner, RuntimeClient>,
		_ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<(Overseer<SpawnGlue<Spawner>, Arc<RuntimeClient>>, OverseerHandle), Error>
	where
		RuntimeClient: RuntimeApiSubsystemClient + ChainApiBackend + AuxStore + 'static,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		collator_overseer_builder(args)?
			.build_with_connector(connector)
			.map_err(|e| e.into())
	}
}
