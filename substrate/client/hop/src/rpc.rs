// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! HOP (Hand-Off protocol) RPC interface implementation.
//!
//! Two layers of rate limiting apply:
//! - The node's global per-connection limit configured via `--rpc-rate-limit`.
//! - HOP-specific per-account token buckets (request rate + bandwidth) enforced inside the pool;
//!   see [`crate::rate_limit`] and the `--hop-*-rate` / `--hop-*-burst` CLI flags.

use crate::{
	metrics::rpc_methods,
	pool::HopDataPool,
	runtime_api,
	types::{
		submit_signing_payload, HopError, HopHash, PoolStatus, Recipient, RecipientVec,
		SubmitResult, MAX_RECIPIENTS,
	},
};
use codec::Decode;
use jsonrpsee::{
	core::{async_trait, RpcResult},
	proc_macros::rpc,
};
use sp_api::CallApiAt;
use sp_blockchain::HeaderBackend;
use sp_core::{Bytes, H256};
use sp_crypto_hashing::blake2_256;
use sp_runtime::{
	traits::{Block as BlockT, IdentifyAccount, Verify},
	AccountId32, MultiSignature, MultiSigner,
};
use std::{marker::PhantomData, sync::Arc};

/// HOP RPC methods.
#[rpc(client, server)]
pub trait HopApi<BlockHash> {
	/// Submit data to the data pool.
	///
	/// # Arguments
	/// * `data`: The data to store, in bytes
	/// * `recipients`: List of SCALE-encoded `MultiSigner` (ed25519, sr25519, or ecdsa)
	/// * `signature`: SCALE-encoded `MultiSignature` over the submit signing payload
	///   (`blake2_256(HOP_SUBMIT_CONTEXT || blake2_256(data) || submit_timestamp.to_le_bytes())`).
	/// * `signer`: SCALE-encoded `MultiSigner` of the account signing the submission
	/// * `submit_timestamp`: Wall-clock timestamp (ms since unix epoch) bound into the signed
	///   payload. The runtime rejects promotions whose timestamp is too far from on-chain time.
	///
	/// `data.len()` must not exceed `HopRuntimeApi::max_promotion_size()`, and
	/// the signer must be authorized by the runtime (checked via
	/// `HopRuntimeApi::can_account_promote`).
	///
	/// # Returns
	/// The current pool status
	#[method(name = "hop_submit", blocking)]
	fn submit(
		&self,
		data: Bytes,
		recipients: Vec<Bytes>,
		signature: Bytes,
		signer: Bytes,
		submit_timestamp: u64,
	) -> RpcResult<SubmitResult>;

	/// Claim data from the data pool by hash (read-only download).
	///
	/// This does NOT mark the recipient as claimed. After receiving the data,
	/// call `hop_ack` with the same arguments to confirm receipt.
	///
	/// The blob may be deleted concurrently by another recipient's ack once all
	/// recipients have acknowledged; callers must be prepared for `NotFound`
	/// and should not assume availability between successive calls.
	///
	/// Requires a SCALE-encoded `MultiSignature` over the hash using the ephemeral
	/// private key corresponding to one of the recipient public keys.
	///
	/// # Arguments
	/// * `hash`: The hash of the data, in bytes (32 bytes)
	/// * `signature`: SCALE-encoded `MultiSignature` over the hash
	///
	/// # Returns
	/// The data if the signature matches a recipient that hasn't yet acked
	#[method(name = "hop_claim", blocking)]
	fn claim(&self, raw_hash: Bytes, signature: Bytes) -> RpcResult<Bytes>;

	/// Acknowledge receipt of claimed data.
	///
	/// Marks the recipient as claimed and triggers cleanup when all recipients
	/// have acknowledged. Idempotent: acking twice succeeds silently, but if the
	/// entry has already been deleted (either because all recipients have
	/// acknowledged or because it expired) the call returns `NotFound` — callers
	/// should treat `NotFound` as a benign terminal state rather than an error.
	///
	/// # Arguments
	/// * `raw_hash`: The hash of the data, in bytes (32 bytes)
	/// * `signature`: SCALE-encoded `MultiSignature` over the hash
	#[method(name = "hop_ack", blocking)]
	fn ack(&self, raw_hash: Bytes, signature: Bytes) -> RpcResult<()>;

	/// Get data pool status
	///
	/// # Returns
	/// Pool statistics including entry count and size
	#[method(name = "hop_poolStatus")]
	fn pool_status(&self) -> RpcResult<PoolStatus>;
}

/// HOP RPC server implementation.
pub struct HopRpcServer<C, Block> {
	pool: Arc<HopDataPool>,
	client: Arc<C>,
	_phantom: PhantomData<Block>,
}

impl<C, Block> HopRpcServer<C, Block> {
	/// Create a new HOP RPC server.
	pub fn new(pool: Arc<HopDataPool>, client: Arc<C>) -> Self {
		Self { pool, client, _phantom: Default::default() }
	}

	/// Decode an RPC `hash` argument: 32 raw bytes (not hex).
	fn decode_hash(bytes: Bytes) -> Result<HopHash, HopError> {
		let hash_bytes: [u8; 32] = bytes
			.0
			.as_slice()
			.try_into()
			.map_err(|_| HopError::InvalidHashLength(bytes.0.len()))?;
		Ok(HopHash::from(hash_bytes))
	}
}

#[async_trait]
impl<C, Block> HopApiServer<<Block as BlockT>::Hash> for HopRpcServer<C, Block>
where
	Block: BlockT,
	C: HeaderBackend<Block> + CallApiAt<Block> + Send + Sync + 'static,
{
	fn submit(
		&self,
		data: Bytes,
		recipients: Vec<Bytes>,
		signature: Bytes,
		signer: Bytes,
		submit_timestamp: u64,
	) -> RpcResult<SubmitResult> {
		let result = self.do_submit(data, recipients, signature, signer, submit_timestamp);
		if let Err(e) = &result {
			self.pool.metrics().record_rpc_error(rpc_methods::SUBMIT, e);
		}
		Ok(result?)
	}

	fn claim(&self, raw_hash: Bytes, signature: Bytes) -> RpcResult<Bytes> {
		let result = Self::decode_hash(raw_hash)
			.and_then(|hash| self.pool.claim(&hash, &signature.0).map(Bytes));
		if let Err(e) = &result {
			self.pool.metrics().record_rpc_error(rpc_methods::CLAIM, e);
		}
		Ok(result?)
	}

	fn ack(&self, raw_hash: Bytes, signature: Bytes) -> RpcResult<()> {
		let result =
			Self::decode_hash(raw_hash).and_then(|hash| self.pool.ack(&hash, &signature.0));
		if let Err(e) = &result {
			self.pool.metrics().record_rpc_error(rpc_methods::ACK, e);
		}
		Ok(result?)
	}

	fn pool_status(&self) -> RpcResult<PoolStatus> {
		Ok(self.pool.status())
	}
}

impl<C, Block> HopRpcServer<C, Block>
where
	Block: BlockT,
	C: HeaderBackend<Block> + CallApiAt<Block> + Send + Sync + 'static,
{
	fn do_submit(
		&self,
		data: Bytes,
		recipients: Vec<Bytes>,
		signature: Bytes,
		signer: Bytes,
		submit_timestamp: u64,
	) -> Result<SubmitResult, HopError> {
		let recipient_keys: RecipientVec = recipients
			.into_iter()
			.map(|r| {
				MultiSigner::decode(&mut &r.0[..])
					.map(|signer| Recipient { signer, claimed: false })
					.map_err(|_| HopError::InvalidRecipientKey)
			})
			.collect::<Result<Vec<_>, _>>()?
			.try_into()
			.map_err(|v: Vec<Recipient>| HopError::TooManyRecipients {
				provided: v.len(),
				limit: MAX_RECIPIENTS as usize,
			})?;

		let signer =
			MultiSigner::decode(&mut &signer.0[..]).map_err(|_| HopError::InvalidSigner)?;
		let multi_sig = MultiSignature::decode(&mut &signature.0[..])
			.map_err(|_| HopError::InvalidSignature)?;

		let chain_info = self.client.info();
		let best_hash = chain_info.best_hash;

		let data_len = data.0.len();

		// Reject oversized payloads before the per-account authorization lookup so
		// a flood of too-big submits cannot force runtime state reads. The cap is
		// the runtime-declared `max_promotion_size`; the runtime is authoritative.
		let runtime_max = runtime_api::max_promotion_size::<Block, _>(&*self.client, best_hash)
			.map_err(HopError::from)?;
		if data_len > runtime_max as usize {
			return Err(HopError::DataTooLarge(data_len, runtime_max));
		}

		// Check authorization before verifying the signature: a flood of unauthorized
		// requests must not force a signature verification per submit.
		// `can_account_promote` returns false for any reason the runtime rejects:
		// unauthorized account or exhausted per-account quota.
		let account_id: AccountId32 = signer.clone().into_account();
		let authorized = runtime_api::can_account_promote::<Block, _>(
			&*self.client,
			best_hash,
			account_id.clone(),
			data_len as u32,
		)
		.map_err(HopError::from)?;
		if !authorized {
			return Err(HopError::NotAuthorized);
		}

		// Domain-separated payload so a submit signature cannot be replayed as claim/ack,
		// and bound to `submit_timestamp` so an old signature can't be replayed long
		// after the fact (the runtime enforces a tolerance window on the timestamp).
		let hash = H256(blake2_256(&data.0));
		let submit_payload = submit_signing_payload(&hash, submit_timestamp);
		if !multi_sig.verify(&submit_payload[..], &account_id) {
			return Err(HopError::InvalidSignature);
		}

		let sender_id: [u8; 32] = account_id.into();
		self.pool
			.insert(data.0, recipient_keys, sender_id, signer, multi_sig, submit_timestamp)?;
		Ok(SubmitResult { pool_status: self.pool.status() })
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::pool::HopDataPool;
	use codec::Encode;
	use sp_api::{ApiError, CallApiAtParams};
	use sp_blockchain::{self, Info};
	use sp_core::{crypto::Pair, ed25519};
	use sp_runtime::{
		traits::{HashingFor, NumberFor},
		MultiSigner,
	};
	use sp_state_machine::InMemoryBackend;
	use sp_test_primitives::Block;
	use std::sync::atomic::{AtomicBool, Ordering};
	use tempfile::TempDir;

	struct MockClient {
		authorized: AtomicBool,
	}

	impl MockClient {
		fn new(authorized: bool) -> Self {
			Self { authorized: AtomicBool::new(authorized) }
		}
	}

	impl HeaderBackend<Block> for MockClient {
		fn header(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<<Block as BlockT>::Header>> {
			Ok(None)
		}

		fn info(&self) -> Info<Block> {
			Info {
				best_hash: Default::default(),
				best_number: 0u64,
				genesis_hash: Default::default(),
				finalized_hash: Default::default(),
				finalized_number: 0u64,
				finalized_state: None,
				number_leaves: 0,
				block_gap: None,
			}
		}

		fn status(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
			Ok(sp_blockchain::BlockStatus::Unknown)
		}

		fn number(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> sp_blockchain::Result<Option<NumberFor<Block>>> {
			Ok(None)
		}

		fn hash(
			&self,
			_number: NumberFor<Block>,
		) -> sp_blockchain::Result<Option<<Block as BlockT>::Hash>> {
			Ok(None)
		}
	}

	impl CallApiAt<Block> for MockClient {
		type StateBackend = InMemoryBackend<HashingFor<Block>>;

		fn call_api_at(&self, params: CallApiAtParams<Block>) -> Result<Vec<u8>, ApiError> {
			match params.function {
				"HopRuntimeApi_max_promotion_size" => Ok((2u32 * 1024 * 1024).encode()),
				"HopRuntimeApi_can_account_promote" => {
					Ok(self.authorized.load(Ordering::Relaxed).encode())
				},
				"HopRuntimeApi_is_promoted_on_chain" => Ok(false.encode()),
				other => Err(ApiError::Application(
					format!("MockClient: unimplemented runtime API call {}", other).into(),
				)),
			}
		}

		fn runtime_version_at(
			&self,
			_at_hash: <Block as BlockT>::Hash,
			_call_context: sp_api::CallContext,
		) -> Result<sp_version::RuntimeVersion, ApiError> {
			unimplemented!("MockClient::runtime_version_at not used by tests")
		}

		fn state_at(&self, _at: <Block as BlockT>::Hash) -> Result<Self::StateBackend, ApiError> {
			unimplemented!("MockClient::state_at not used by tests")
		}

		fn initialize_extensions(
			&self,
			_at: <Block as BlockT>::Hash,
			_extensions: &mut sp_externalities::Extensions,
		) -> Result<(), ApiError> {
			Ok(())
		}
	}

	fn setup(authorized: bool) -> (HopRpcServer<MockClient, Block>, Arc<HopDataPool>, TempDir) {
		let dir = TempDir::new().unwrap();
		let pool = Arc::new(
			HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				crate::rate_limit::RateLimitConfig::disabled(),
				crate::metrics::HopMetrics::disabled(),
			)
			.unwrap(),
		);
		let client = Arc::new(MockClient::new(authorized));
		let rpc = HopRpcServer::new(pool.clone(), client);
		(rpc, pool, dir)
	}

	/// Same as [`setup`] but with metrics registered.
	fn setup_metered() -> (HopRpcServer<MockClient, Block>, Arc<HopDataPool>, TempDir) {
		let registry = prometheus_endpoint::Registry::new();
		let dir = TempDir::new().unwrap();
		let pool = Arc::new(
			HopDataPool::new(
				1024 * 1024,
				1024 * 1024,
				100,
				dir.path().to_path_buf(),
				crate::rate_limit::RateLimitConfig::disabled(),
				crate::metrics::HopMetrics::new(Some(&registry)).unwrap(),
			)
			.unwrap(),
		);
		let client = Arc::new(MockClient::new(true));
		let rpc = HopRpcServer::new(pool.clone(), client);
		(rpc, pool, dir)
	}

	fn make_keypair() -> (ed25519::Pair, MultiSigner) {
		let pair = ed25519::Pair::from_seed(&[1u8; 32]);
		let signer = MultiSigner::Ed25519(pair.public());
		(pair, signer)
	}

	/// Fixed submit timestamp used in tests where the actual value is irrelevant.
	const TEST_SUBMIT_TS: u64 = 1_700_000_000_000;

	/// Produce a domain-separated submit signature for `data` bound to a timestamp.
	fn submit_sig(pair: &ed25519::Pair, data: &[u8], submit_timestamp: u64) -> Bytes {
		let hash = H256(blake2_256(data));
		let payload = submit_signing_payload(&hash, submit_timestamp);
		let multi_sig = MultiSignature::Ed25519(pair.sign(&payload));
		Bytes(multi_sig.encode())
	}

	fn claim_sig(pair: &ed25519::Pair, hash: &H256) -> Bytes {
		use crate::types::{signing_payload, HOP_CLAIM_CONTEXT};
		let payload = signing_payload(HOP_CLAIM_CONTEXT, hash);
		Bytes(MultiSignature::Ed25519(pair.sign(&payload)).encode())
	}

	fn ack_sig(pair: &ed25519::Pair, hash: &H256) -> Bytes {
		use crate::types::{signing_payload, HOP_ACK_CONTEXT};
		let payload = signing_payload(HOP_ACK_CONTEXT, hash);
		Bytes(MultiSignature::Ed25519(pair.sign(&payload)).encode())
	}

	#[test]
	fn rpc_metrics_record_errors() {
		let (rpc, pool, _dir) = setup_metered();
		let (pair, _) = make_keypair();

		// Reaches the pool and comes back `not_found`.
		let unknown = H256([9u8; 32]);
		assert!(rpc.claim(Bytes(unknown.0.to_vec()), claim_sig(&pair, &unknown)).is_err());
		assert_eq!(pool.metrics().rpc_error_count(rpc_methods::CLAIM, "not_found"), 1);

		// Never reaches the pool, still counted.
		assert!(rpc.ack(Bytes(vec![0u8; 4]), Bytes(vec![0u8; 64])).is_err());
		assert_eq!(pool.metrics().rpc_error_count(rpc_methods::ACK, "invalid_hash_length"), 1);
	}

	#[test]
	fn submit_invalid_scale_signer_returns_error() {
		let (rpc, _, _dir) = setup(true);
		// One valid recipient so the RecipientVec step passes; then the SCALE-invalid
		// signer bytes trigger `InvalidSigner`.
		let (_, valid_signer) = make_keypair();
		let result = rpc.submit(
			Bytes(vec![1, 2, 3]),
			vec![Bytes(valid_signer.encode())],
			Bytes(vec![0u8; 3]),
			Bytes(vec![0u8; 3]),
			TEST_SUBMIT_TS,
		);
		assert!(result.is_err());
		let err = result.unwrap_err();
		assert!(err.message().contains("SCALE-decode MultiSigner"), "got: {}", err.message());
	}

	#[test]
	fn submit_invalid_scale_signature_returns_error() {
		let (rpc, _, _dir) = setup(true);
		let (_, signer) = make_keypair();
		let result = rpc.submit(
			Bytes(vec![1, 2, 3]),
			vec![Bytes(signer.encode())],
			Bytes(vec![0u8; 3]),
			Bytes(signer.encode()),
			TEST_SUBMIT_TS,
		);
		assert!(result.is_err());
		let err = result.unwrap_err();
		assert!(err.message().contains("Invalid signature"), "got: {}", err.message());
	}

	#[test]
	fn submit_bad_signature_returns_error() {
		let (rpc, _, _dir) = setup(true);
		let (_, signer) = make_keypair();
		// Sign with a different key.
		let wrong_pair = ed25519::Pair::from_seed(&[99u8; 32]);
		let data = vec![1, 2, 3];
		let sig = submit_sig(&wrong_pair, &data, TEST_SUBMIT_TS);

		let result = rpc.submit(
			Bytes(data),
			vec![Bytes(signer.encode())],
			sig,
			Bytes(signer.encode()),
			TEST_SUBMIT_TS,
		);
		assert!(result.is_err());
		let err = result.unwrap_err();
		assert!(err.message().contains("Invalid signature"), "got: {}", err.message());
	}

	#[test]
	fn submit_unauthorized_account_returns_error() {
		let (rpc, _, _dir) = setup(false);
		let (pair, signer) = make_keypair();
		let data = vec![1, 2, 3];
		let sig = submit_sig(&pair, &data, TEST_SUBMIT_TS);

		let result = rpc.submit(
			Bytes(data),
			vec![Bytes(signer.encode())],
			sig,
			Bytes(signer.encode()),
			TEST_SUBMIT_TS,
		);
		assert!(result.is_err());
		let err = result.unwrap_err();
		assert!(err.message().contains("authorization"), "got: {}", err.message());
	}

	#[test]
	fn submit_success() {
		let (rpc, pool, _dir) = setup(true);
		let (pair, signer) = make_keypair();
		let data = vec![1, 2, 3, 4, 5];
		let sig = submit_sig(&pair, &data, TEST_SUBMIT_TS);

		let result = rpc.submit(
			Bytes(data),
			vec![Bytes(signer.encode())],
			sig,
			Bytes(signer.encode()),
			TEST_SUBMIT_TS,
		);
		assert!(result.is_ok(), "submit failed: {:?}", result.err());
		let submit_result = result.unwrap();
		assert_eq!(submit_result.pool_status.entry_count, 1);
		// Accounted bytes include per-recipient metadata overhead, not just the blob.
		assert_eq!(submit_result.pool_status.total_bytes, crate::types::entry_accounted_size(5, 1),);
		assert_eq!(pool.status().entry_count, 1);
	}

	#[test]
	fn submit_rejects_oversized_recipient_list() {
		let (rpc, _, _dir) = setup(true);
		let (pair, signer) = make_keypair();
		let data = vec![1, 2, 3];
		let sig = submit_sig(&pair, &data, TEST_SUBMIT_TS);

		let oversized: Vec<Bytes> = std::iter::repeat_with(|| Bytes(signer.encode()))
			.take(MAX_RECIPIENTS as usize + 1)
			.collect();

		let result =
			rpc.submit(Bytes(data), oversized, sig, Bytes(signer.encode()), TEST_SUBMIT_TS);
		assert!(result.is_err());
		let err = result.unwrap_err();
		assert!(err.message().contains("Too many recipients"), "got: {}", err.message());
	}

	#[test]
	fn claim_invalid_hash_length() {
		let (rpc, _, _dir) = setup(true);
		let result = rpc.claim(Bytes(vec![0u8; 31]), Bytes(vec![0u8; 64]));
		assert!(result.is_err());
		let err = result.unwrap_err();
		assert!(err.message().contains("expected 32 bytes"), "got: {}", err.message());
	}

	#[test]
	fn claim_and_ack_through_rpc() {
		let (rpc, _, _dir) = setup(true);
		let (pair, signer) = make_keypair();
		let data = vec![10, 20, 30];
		let sig = submit_sig(&pair, &data, TEST_SUBMIT_TS);

		rpc.submit(
			Bytes(data.clone()),
			vec![Bytes(signer.encode())],
			sig,
			Bytes(signer.encode()),
			TEST_SUBMIT_TS,
		)
		.unwrap();

		let hash = H256(blake2_256(&data));
		let claimed = rpc.claim(Bytes(hash.0.to_vec()), claim_sig(&pair, &hash)).unwrap();
		assert_eq!(claimed.0, data);

		rpc.ack(Bytes(hash.0.to_vec()), ack_sig(&pair, &hash)).unwrap();

		let status = rpc.pool_status().unwrap();
		assert_eq!(status.entry_count, 0);
	}

	#[test]
	fn pool_status_returns_correct_values() {
		let (rpc, _, _dir) = setup(true);
		let status = rpc.pool_status().unwrap();
		assert_eq!(status.entry_count, 0);
		assert_eq!(status.total_bytes, 0);
		assert_eq!(status.max_bytes, 1024 * 1024);
	}
}
