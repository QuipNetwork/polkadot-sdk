// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Client implementation that connects to the Substrate node over `ws`/`wss` connection
//! and is using RPC methods to get required data and submit transactions.

use crate::{
	client::{
		rpc_api::{
			SubstrateAuthorClient, SubstrateBeefyClient, SubstrateChainClient,
			SubstrateFrameSystemClient, SubstrateGrandpaClient, SubstrateStateClient,
			SubstrateSystemClient,
		},
		subscription::{StreamDescription, Subscription},
		Client,
	},
	error::{Error, Result},
	guard::Environment,
	transaction_stall_timeout, AccountIdOf, AccountKeyPairOf, BalanceOf, BlockNumberOf, Chain,
	ChainRuntimeVersion, ChainWithGrandpa, ChainWithTransactions, ConnectionParams, HashOf,
	HeaderIdOf, HeaderOf, NonceOf, SignParam, SignedBlockOf, SimpleRuntimeVersion,
	TransactionTracker, UnsignedTransaction,
};

use async_trait::async_trait;
use bp_runtime::HeaderIdProvider;
use codec::Encode;
use frame_support::weights::Weight;
use futures::TryFutureExt;
use jsonrpsee::{
	core::{client::Subscription as RpcSubscription, ClientError},
	ws_client::{WsClient, WsClientBuilder},
};
use num_traits::Zero;
use pallet_transaction_payment::RuntimeDispatchInfo;
use relay_utils::{relay_loop::RECONNECT_DELAY, STALL_TIMEOUT};
use sp_core::{
	storage::{StorageData, StorageKey},
	Bytes, Hasher, Pair,
};
use sp_runtime::{
	traits::Header,
	transaction_validity::{TransactionSource, TransactionValidity},
};
use sp_trie::StorageProof;
use sp_version::RuntimeVersion;
use std::{cmp::Ordering, future::Future, marker::PhantomData, sync::Arc};
use tokio::sync::{Mutex, RwLock};

const MAX_SUBSCRIPTION_CAPACITY: usize = 4096;

const SUB_API_TXPOOL_VALIDATE_TRANSACTION: &str = "TaggedTransactionQueue_validate_transaction";
const SUB_API_TX_PAYMENT_QUERY_INFO: &str = "TransactionPaymentApi_query_info";
const SUB_API_GRANDPA_GENERATE_KEY_OWNERSHIP_PROOF: &str =
	"GrandpaApi_generate_key_ownership_proof";

/// Client implementation that connects to the Substrate node over `ws`/`wss` connection
/// and is using RPC methods to get required data and submit transactions.
pub struct RpcClient<C: Chain> {
	// Lock order: `submit_signed_extrinsic_lock`, `data`
	/// Client connection params.
	params: Arc<ConnectionParams>,
	/// If several tasks are submitting their transactions simultaneously using
	/// `submit_signed_extrinsic` method, they may get the same transaction nonce. So one of
	/// transactions will be rejected from the pool. This lock is here to prevent situations like
	/// that.
	submit_signed_extrinsic_lock: Arc<Mutex<()>>,
	/// Genesis block hash.
	genesis_hash: HashOf<C>,
	/// Shared dynamic data.
	data: Arc<RwLock<ClientData>>,
	/// Generic arguments dump.
	_phantom: PhantomData<C>,
}

/// Client data, shared by all `RpcClient` clones.
struct ClientData {
	tokio: tokio::runtime::Handle,
	/// Substrate RPC client.
	client: Arc<WsClient>,
}

/// Already encoded value.
struct PreEncoded(Vec<u8>);

impl Encode for PreEncoded {
	fn encode(&self) -> Vec<u8> {
		self.0.clone()
	}
}

impl<C: Chain> std::fmt::Debug for RpcClient<C> {
	fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
		fmt.write_fmt(format_args!("RpcClient<{}>", C::NAME))
	}
}

impl<C: Chain> RpcClient<C> {
	/// Returns client that is able to call RPCs on Substrate node over websocket connection.
	///
	/// This function will keep connecting to given Substrate node until connection is established
	/// and is functional. If attempt fail, it will wait for `RECONNECT_DELAY` and retry again.
	pub async fn new(params: ConnectionParams) -> Self {
		let params = Arc::new(params);
		loop {
			match Self::try_connect(params.clone()).await {
				Ok(client) => return client,
				Err(error) => tracing::error!(
					target: "bridge",
					?error,
					node=%C::NAME,
					retry_as_secs=%RECONNECT_DELAY.as_secs(),
					"Failed to connect. Going to retry"
				),
			}

			tokio::time::sleep(RECONNECT_DELAY).await;
		}
	}

	/// Try to connect to Substrate node over websocket. Returns Substrate RPC client if connection
	/// has been established or error otherwise.
	async fn try_connect(params: Arc<ConnectionParams>) -> Result<Self> {
		let (tokio, client) = Self::build_client(&params).await?;

		let genesis_hash_client = client.clone();
		let genesis_hash = tokio
			.spawn(async move {
				SubstrateChainClient::<C>::block_hash(&*genesis_hash_client, Some(Zero::zero()))
					.await
			})
			.await??;

		let chain_runtime_version = params.chain_runtime_version;
		let mut client = Self {
			params,
			submit_signed_extrinsic_lock: Arc::new(Mutex::new(())),
			genesis_hash,
			data: Arc::new(RwLock::new(ClientData { tokio, client })),
			_phantom: PhantomData,
		};
		Self::ensure_correct_runtime_version(&mut client, chain_runtime_version).await?;
		Ok(client)
	}

	// Check runtime version to understand if we need are connected to expected version, or we
	// need to wait for upgrade, we need to abort immediately.
	async fn ensure_correct_runtime_version<E: Environment<C, Error = Error>>(
		env: &mut E,
		expected: ChainRuntimeVersion,
	) -> Result<()> {
		// we are only interested if version mode is bundled or passed using CLI
		let expected = match expected {
			ChainRuntimeVersion::Auto => return Ok(()),
			ChainRuntimeVersion::Custom(expected) => expected,
		};

		// we need to wait if actual version is < than expected, we are OK of versions are the
		// same and we need to abort if actual version is > than expected
		let actual = SimpleRuntimeVersion::from_runtime_version(&env.runtime_version().await?);
		match actual.spec_version.cmp(&expected.spec_version) {
			Ordering::Less => {
				Err(Error::WaitingForRuntimeUpgrade { chain: C::NAME.into(), expected, actual })
			},
			Ordering::Equal => Ok(()),
			Ordering::Greater => {
				tracing::error!(
					target: "bridge",
					node=%C::NAME,
					?expected,
					?actual,
					"The client is configured to use runtime version, which is different from the \
					actual version. Aborting",
				);
				env.abort().await;
				Err(Error::Custom("Aborted".into()))
			},
		}
	}

	/// Build client to use in connection.
	async fn build_client(
		params: &ConnectionParams,
	) -> Result<(tokio::runtime::Handle, Arc<WsClient>)> {
		let tokio = tokio::runtime::Handle::try_current().map_err(|e| {
			Error::Custom(format!("Failed to obtain current tokio runtime handle: {e}"))
		})?;
		let uri = params.uri.clone();
		tracing::info!(target: "bridge", node=%C::NAME, %uri, "Connecting");

		let client = tokio
			.spawn(async move {
				WsClientBuilder::default()
					.max_buffer_capacity_per_subscription(MAX_SUBSCRIPTION_CAPACITY)
					.build(&uri)
					.await
			})
			.await??;

		Ok((tokio, Arc::new(client)))
	}

	/// Execute jsonrpsee future in tokio context.
	async fn jsonrpsee_execute<MF, F, T>(&self, make_jsonrpsee_future: MF) -> Result<T>
	where
		MF: FnOnce(Arc<WsClient>) -> F + Send + 'static,
		F: Future<Output = Result<T>> + Send + 'static,
		T: Send + 'static,
	{
		let data = self.data.read().await;
		let client = data.client.clone();
		data.tokio.spawn(make_jsonrpsee_future(client)).await?
	}

	/// Prepare parameters used to sign chain transactions.
	async fn build_sign_params(&self, signer: AccountKeyPairOf<C>) -> Result<SignParam<C>>
	where
		C: ChainWithTransactions,
	{
		let runtime_version = self.simple_runtime_version().await?;
		Ok(SignParam::<C> {
			spec_version: runtime_version.spec_version,
			transaction_version: runtime_version.transaction_version,
			genesis_hash: self.genesis_hash,
			signer,
		})
	}

	/// Get the nonce of the given Substrate account.
	pub async fn next_account_index(&self, account: AccountIdOf<C>) -> Result<NonceOf<C>> {
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateFrameSystemClient::<C>::account_next_index(&*client, account).await?)
		})
		.await
	}

	/// Subscribe to finality justifications.
	async fn subscribe_finality_justifications<Fut>(
		&self,
		gadget_name: &str,
		do_subscribe: impl FnOnce(Arc<WsClient>) -> Fut + Send + 'static,
	) -> Result<Subscription<Bytes>>
	where
		Fut: Future<Output = std::result::Result<RpcSubscription<Bytes>, ClientError>> + Send,
	{
		let subscription = self
			.jsonrpsee_execute(move |client| async move { Ok(do_subscribe(client).await?) })
			.map_err(|e| Error::failed_to_subscribe_justification::<C>(e))
			.await?;

		Ok(Subscription::new_forwarded(
			StreamDescription::new(format!("{} justifications", gadget_name), C::NAME.into()),
			subscription,
		))
	}

	/// Subscribe to headers stream.
	async fn subscribe_headers<Fut>(
		&self,
		stream_name: &str,
		do_subscribe: impl FnOnce(Arc<WsClient>) -> Fut + Send + 'static,
		map_err: impl FnOnce(Error) -> Error,
	) -> Result<Subscription<HeaderOf<C>>>
	where
		Fut: Future<Output = std::result::Result<RpcSubscription<HeaderOf<C>>, ClientError>> + Send,
	{
		let subscription = self
			.jsonrpsee_execute(move |client| async move { Ok(do_subscribe(client).await?) })
			.map_err(map_err)
			.await?;

		Ok(Subscription::new_forwarded(
			StreamDescription::new(format!("{} headers", stream_name), C::NAME.into()),
			subscription,
		))
	}
}

impl<C: Chain> Clone for RpcClient<C> {
	fn clone(&self) -> Self {
		RpcClient {
			params: self.params.clone(),
			submit_signed_extrinsic_lock: self.submit_signed_extrinsic_lock.clone(),
			genesis_hash: self.genesis_hash,
			data: self.data.clone(),
			_phantom: PhantomData,
		}
	}
}

#[async_trait]
impl<C: Chain> Client<C> for RpcClient<C> {
	async fn ensure_synced(&self) -> Result<()> {
		let health = self
			.jsonrpsee_execute(|client| async move {
				Ok(SubstrateSystemClient::<C>::health(&*client).await?)
			})
			.await
			.map_err(|e| Error::failed_to_get_system_health::<C>(e))?;

		let is_synced = !health.is_syncing && (!health.should_have_peers || health.peers > 0);
		if is_synced {
			Ok(())
		} else {
			Err(Error::ClientNotSynced(health))
		}
	}

	async fn reconnect(&self) -> Result<()> {
		let mut data = self.data.write().await;
		let (tokio, client) = Self::build_client(&self.params).await?;
		data.tokio = tokio;
		data.client = client;
		Ok(())
	}

	fn genesis_hash(&self) -> HashOf<C> {
		self.genesis_hash
	}

	async fn header_hash_by_number(&self, number: BlockNumberOf<C>) -> Result<HashOf<C>> {
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateChainClient::<C>::block_hash(&*client, Some(number)).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_header_hash_by_number::<C>(number, e))
	}

	async fn header_by_hash(&self, hash: HashOf<C>) -> Result<HeaderOf<C>> {
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateChainClient::<C>::header(&*client, Some(hash)).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_header_by_hash::<C>(hash, e))
	}

	async fn block_by_hash(&self, hash: HashOf<C>) -> Result<SignedBlockOf<C>> {
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateChainClient::<C>::block(&*client, Some(hash)).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_block_by_hash::<C>(hash, e))
	}

	async fn best_finalized_header_hash(&self) -> Result<HashOf<C>> {
		self.jsonrpsee_execute(|client| async move {
			Ok(SubstrateChainClient::<C>::finalized_head(&*client).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_best_finalized_header_hash::<C>(e))
	}

	async fn best_header(&self) -> Result<HeaderOf<C>> {
		self.jsonrpsee_execute(|client| async move {
			Ok(SubstrateChainClient::<C>::header(&*client, None).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_best_header::<C>(e))
	}

	async fn subscribe_best_headers(&self) -> Result<Subscription<HeaderOf<C>>> {
		self.subscribe_headers(
			"best headers",
			move |client| async move { SubstrateChainClient::<C>::subscribe_new_heads(&*client).await },
			|e| Error::failed_to_subscribe_best_headers::<C>(e),
		)
		.await
	}

	async fn subscribe_finalized_headers(&self) -> Result<Subscription<HeaderOf<C>>> {
		self.subscribe_headers(
			"best finalized headers",
			move |client| async move {
				SubstrateChainClient::<C>::subscribe_finalized_heads(&*client).await
			},
			|e| Error::failed_to_subscribe_finalized_headers::<C>(e),
		)
		.await
	}

	async fn subscribe_grandpa_finality_justifications(&self) -> Result<Subscription<Bytes>>
	where
		C: ChainWithGrandpa,
	{
		self.subscribe_finality_justifications("GRANDPA", move |client| async move {
			SubstrateGrandpaClient::<C>::subscribe_justifications(&*client).await
		})
		.await
	}

	async fn generate_grandpa_key_ownership_proof(
		&self,
		at: HashOf<C>,
		set_id: sp_consensus_grandpa::SetId,
		authority_id: sp_consensus_grandpa::AuthorityId,
	) -> Result<Option<sp_consensus_grandpa::OpaqueKeyOwnershipProof>> {
		self.state_call(
			at,
			SUB_API_GRANDPA_GENERATE_KEY_OWNERSHIP_PROOF.into(),
			(set_id, authority_id),
		)
		.await
	}

	async fn subscribe_beefy_finality_justifications(&self) -> Result<Subscription<Bytes>> {
		self.subscribe_finality_justifications("BEEFY", move |client| async move {
			SubstrateBeefyClient::<C>::subscribe_justifications(&*client).await
		})
		.await
	}

	async fn token_decimals(&self) -> Result<Option<u64>> {
		self.jsonrpsee_execute(move |client| async move {
			let system_properties = SubstrateSystemClient::<C>::properties(&*client).await?;
			Ok(system_properties.get("tokenDecimals").and_then(|v| v.as_u64()))
		})
		.await
	}

	async fn runtime_version(&self) -> Result<RuntimeVersion> {
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateStateClient::<C>::runtime_version(&*client).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_runtime_version::<C>(e))
	}

	async fn simple_runtime_version(&self) -> Result<SimpleRuntimeVersion> {
		Ok(match self.params.chain_runtime_version {
			ChainRuntimeVersion::Auto => {
				let runtime_version = self.runtime_version().await?;
				SimpleRuntimeVersion::from_runtime_version(&runtime_version)
			},
			ChainRuntimeVersion::Custom(ref version) => *version,
		})
	}

	fn can_start_version_guard(&self) -> bool {
		!matches!(self.params.chain_runtime_version, ChainRuntimeVersion::Auto)
	}

	async fn raw_storage_value(
		&self,
		at: HashOf<C>,
		storage_key: StorageKey,
	) -> Result<Option<StorageData>> {
		let cloned_storage_key = storage_key.clone();
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateStateClient::<C>::storage(&*client, cloned_storage_key, Some(at)).await?)
		})
		.await
		.map_err(|e| Error::failed_to_read_storage_value::<C>(at, storage_key, e))
	}

	async fn pending_extrinsics(&self) -> Result<Vec<Bytes>> {
		self.jsonrpsee_execute(move |client| async move {
			Ok(SubstrateAuthorClient::<C>::pending_extrinsics(&*client).await?)
		})
		.await
		.map_err(|e| Error::failed_to_get_pending_extrinsics::<C>(e))
	}

	async fn submit_unsigned_extrinsic(&self, transaction: Bytes) -> Result<HashOf<C>> {
		// one last check that the transaction is valid. Most of checks happen in the relay loop and
		// it is the "final" check before submission.
		let best_header_hash = self.best_header_hash().await?;
		self.validate_transaction(best_header_hash, PreEncoded(transaction.0.clone()))
			.await
			.map_err(|e| Error::failed_to_submit_transaction::<C>(e))?
			.map_err(|e| Error::failed_to_submit_transaction::<C>(Error::TransactionInvalid(e)))?;

		self.jsonrpsee_execute(move |client| async move {
			let tx_hash = SubstrateAuthorClient::<C>::submit_extrinsic(&*client, transaction)
				.await
				.map_err(|e| {
					tracing::error!(target: "bridge", error=?e, node=%C::NAME, "Failed to send transaction");
					e
				})?;
			tracing::trace!(target: "bridge", node=%C::NAME, ?tx_hash, "Sent transaction");
			Ok(tx_hash)
		})
		.await
		.map_err(|e| Error::failed_to_submit_transaction::<C>(e))
	}

	async fn submit_signed_extrinsic(
		&self,
		signer: &AccountKeyPairOf<C>,
		prepare_extrinsic: impl FnOnce(HeaderIdOf<C>, NonceOf<C>) -> Result<UnsignedTransaction<C>>
			+ Send
			+ 'static,
	) -> Result<HashOf<C>>
	where
		C: ChainWithTransactions,
		AccountIdOf<C>: From<<AccountKeyPairOf<C> as Pair>::Public>,
	{
		let _guard = self.submit_signed_extrinsic_lock.lock().await;
		let transaction_nonce = self.next_account_index(signer.public().into()).await?;
		let best_header = self.best_header().await?;
		let signing_data = self.build_sign_params(signer.clone()).await?;

		// By using parent of best block here, we are protecting again best-block reorganizations.
		// E.g. transaction may have been submitted when the best block was `A[num=100]`. Then it
		// has been changed to `B[num=100]`. Hash of `A` has been included into transaction
		// signature payload. So when signature will be checked, the check will fail and transaction
		// will be dropped from the pool.
		let best_header_id = best_header.parent_id().unwrap_or_else(|| best_header.id());

		let extrinsic = prepare_extrinsic(best_header_id, transaction_nonce)?;
		let signed_extrinsic = C::sign_transaction(signing_data, extrinsic)?.encode();
		self.submit_unsigned_extrinsic(Bytes(signed_extrinsic)).await
	}

	async fn submit_and_watch_signed_extrinsic(
		&self,
		signer: &AccountKeyPairOf<C>,
		prepare_extrinsic: impl FnOnce(HeaderIdOf<C>, NonceOf<C>) -> Result<UnsignedTransaction<C>>
			+ Send
			+ 'static,
	) -> Result<TransactionTracker<C, Self>>
	where
		C: ChainWithTransactions,
		AccountIdOf<C>: From<<AccountKeyPairOf<C> as Pair>::Public>,
	{
		let self_clone = self.clone();
		let signing_data = self.build_sign_params(signer.clone()).await?;
		let _guard = self.submit_signed_extrinsic_lock.lock().await;
		let transaction_nonce = self.next_account_index(signer.public().into()).await?;
		let best_header = self.best_header().await?;
		let best_header_id = best_header.id();

		let extrinsic = prepare_extrinsic(best_header_id, transaction_nonce)?;
		let stall_timeout = transaction_stall_timeout(
			extrinsic.era.mortality_period(),
			C::AVERAGE_BLOCK_INTERVAL,
			STALL_TIMEOUT,
		);
		let signed_extrinsic = C::sign_transaction(signing_data, extrinsic)?.encode();

		// one last check that the transaction is valid. Most of checks happen in the relay loop and
		// it is the "final" check before submission.
		self.validate_transaction(best_header_id.hash(), PreEncoded(signed_extrinsic.clone()))
			.await
			.map_err(|e| Error::failed_to_submit_transaction::<C>(e))?
			.map_err(|e| Error::failed_to_submit_transaction::<C>(Error::TransactionInvalid(e)))?;

		self.jsonrpsee_execute(move |client| async move {
			let tx_hash = C::Hasher::hash(&signed_extrinsic);
			let subscription: jsonrpsee::core::client::Subscription<_> =
				SubstrateAuthorClient::<C>::submit_and_watch_extrinsic(
					&*client,
					Bytes(signed_extrinsic),
				)
				.await
				.map_err(|e| {
					tracing::error!(target: "bridge", error=?e, node=%C::NAME, "Failed to send transaction");
					e
				})?;
			tracing::trace!(target: "bridge", node=%C::NAME, ?tx_hash, "Sent transaction");
			Ok(TransactionTracker::new(
				self_clone,
				stall_timeout,
				tx_hash,
				Subscription::new_forwarded(
					StreamDescription::new("transaction events".into(), C::NAME.into()),
					subscription,
				),
			))
		})
		.await
		.map_err(|e| Error::failed_to_submit_transaction::<C>(e))
	}

	async fn validate_transaction<SignedTransaction: Encode + Send + 'static>(
		&self,
		at: HashOf<C>,
		transaction: SignedTransaction,
	) -> Result<TransactionValidity> {
		self.state_call(
			at,
			SUB_API_TXPOOL_VALIDATE_TRANSACTION.into(),
			(TransactionSource::External, transaction, at),
		)
		.await
	}

	async fn estimate_extrinsic_weight<SignedTransaction: Encode + Send + 'static>(
		&self,
		at: HashOf<C>,
		transaction: SignedTransaction,
	) -> Result<Weight> {
		let transaction_len = transaction.encoded_size() as u32;
		let dispatch_info: RuntimeDispatchInfo<BalanceOf<C>> = self
			.state_call(at, SUB_API_TX_PAYMENT_QUERY_INFO.into(), (transaction, transaction_len))
			.await?;

		Ok(dispatch_info.weight)
	}

	async fn raw_state_call<Args: Encode + Send>(
		&self,
		at: HashOf<C>,
		method: String,
		arguments: Args,
	) -> Result<Bytes> {
		let arguments = Bytes(arguments.encode());
		let arguments_clone = arguments.clone();
		let method_clone = method.clone();
		self.jsonrpsee_execute(move |client| async move {
			SubstrateStateClient::<C>::call(&*client, method, arguments, Some(at))
				.await
				.map_err(Into::into)
		})
		.await
		.map_err(|e| Error::failed_state_call::<C>(at, method_clone, arguments_clone, e))
	}

	async fn prove_storage(
		&self,
		at: HashOf<C>,
		keys: Vec<StorageKey>,
	) -> Result<(StorageProof, HashOf<C>)> {
		let state_root = *self.header_by_hash(at).await?.state_root();

		let keys_clone = keys.clone();
		let read_proof = self
			.jsonrpsee_execute(move |client| async move {
				SubstrateStateClient::<C>::prove_storage(&*client, keys_clone, Some(at))
					.await
					.map(|proof| StorageProof::new(proof.proof.into_iter().map(|b| b.0)))
					.map_err(Into::into)
			})
			.await
			.map_err(|e| Error::failed_to_prove_storage::<C>(at, keys.clone(), e))?;

		Ok((read_proof, state_root))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{guard::tests::TestEnvironment, test_chain::TestChain};
	use futures::{channel::mpsc::unbounded, FutureExt, SinkExt, StreamExt};

	async fn run_ensure_correct_runtime_version(
		expected: ChainRuntimeVersion,
		actual: RuntimeVersion,
	) -> Result<()> {
		let (
			(mut runtime_version_tx, runtime_version_rx),
			(slept_tx, _slept_rx),
			(aborted_tx, mut aborted_rx),
		) = (unbounded(), unbounded(), unbounded());
		runtime_version_tx.send(actual).await.unwrap();
		let mut env = TestEnvironment { runtime_version_rx, slept_tx, aborted_tx };

		let ensure_correct_runtime_version =
			RpcClient::<TestChain>::ensure_correct_runtime_version(&mut env, expected).boxed();
		let aborted = aborted_rx.next().map(|_| Err(Error::Custom("".into()))).boxed();
		futures::pin_mut!(ensure_correct_runtime_version, aborted);
		futures::future::select(ensure_correct_runtime_version, aborted)
			.await
			.into_inner()
			.0
	}

	#[tokio::test]
	async fn ensure_correct_runtime_version_works() {
		// when we are configured to use auto version
		assert!(matches!(
			run_ensure_correct_runtime_version(
				ChainRuntimeVersion::Auto,
				RuntimeVersion {
					spec_version: 100,
					transaction_version: 100,
					..Default::default()
				},
			)
			.await,
			Ok(()),
		));
		// when actual == expected
		assert!(matches!(
			run_ensure_correct_runtime_version(
				ChainRuntimeVersion::Custom(SimpleRuntimeVersion {
					spec_version: 100,
					transaction_version: 100
				}),
				RuntimeVersion {
					spec_version: 100,
					transaction_version: 100,
					..Default::default()
				},
			)
			.await,
			Ok(()),
		));
		// when actual spec version < expected spec version
		assert!(matches!(
			run_ensure_correct_runtime_version(
				ChainRuntimeVersion::Custom(SimpleRuntimeVersion {
					spec_version: 100,
					transaction_version: 100
				}),
				RuntimeVersion { spec_version: 99, transaction_version: 100, ..Default::default() },
			)
			.await,
			Err(Error::WaitingForRuntimeUpgrade {
				expected: SimpleRuntimeVersion { spec_version: 100, transaction_version: 100 },
				actual: SimpleRuntimeVersion { spec_version: 99, transaction_version: 100 },
				..
			}),
		));
		// when actual spec version > expected spec version
		assert!(matches!(
			run_ensure_correct_runtime_version(
				ChainRuntimeVersion::Custom(SimpleRuntimeVersion {
					spec_version: 100,
					transaction_version: 100
				}),
				RuntimeVersion {
					spec_version: 101,
					transaction_version: 100,
					..Default::default()
				},
			)
			.await,
			Err(Error::Custom(_)),
		));
	}
}
