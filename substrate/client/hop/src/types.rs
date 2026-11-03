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

//! HOP types and data structures.

use codec::{Decode, Encode};
use polkadot_primitives::{BlockNumber, Hash};
use serde::{Deserialize, Serialize};
use sp_core::{bounded_vec::BoundedVec, ConstU32};
use sp_crypto_hashing::blake2_256;
use sp_runtime::{MultiSignature, MultiSigner};

/// Block number type used by HOP.
pub type HopBlockNumber = BlockNumber;

/// Hash type used by HOP.
pub type HopHash = Hash;

/// Sender identity derived from the account that signed the submission.
pub type SenderId = [u8; 32];

/// One intended recipient of a HOP entry: the ephemeral public key the sender
/// generated for this handoff, paired with the `claimed` flag that tracks whether
/// this recipient has acked. Fusing the two into a single struct (and a single
/// `BoundedVec<Recipient, ...>`) makes it impossible — by construction and on
/// disk — for the key list and the ack state to drift out of sync.
#[derive(Debug, Clone, Encode, Decode)]
pub struct Recipient {
	/// Ephemeral public key (MultiSigner: ed25519, sr25519, or ecdsa).
	pub signer: MultiSigner,
	/// Whether this recipient has acked receipt.
	pub claimed: bool,
}

/// On-disk format version for `HopEntryMeta` records. Startup recovery rejects
/// `.meta` files whose `version` field doesn't match, so same-shape schema
/// changes (e.g. semantic reinterpretation of an existing field) can be rolled
/// out by bumping this constant; shape changes are caught by SCALE decode failure.
pub const HOP_META_VERSION: u8 = 2;

/// Metadata for a pool entry (stored in-memory index and on-disk .meta files).
#[derive(Debug, Clone, Encode, Decode)]
pub struct HopEntryMeta {
	/// On-disk format version; see `HOP_META_VERSION`.
	pub version: u8,
	/// Unix timestamp (seconds) at which this entry expires.
	pub expires_at: u64,
	/// Size in bytes
	pub size: u64,
	/// Intended recipients and their per-recipient ack state.
	///
	/// Using a `BoundedVec` means a corrupted / hostile on-disk `.meta` file with
	/// too many recipients fails to SCALE-decode and is discarded during startup
	/// recovery rather than being loaded into the in-memory index.
	pub recipients: RecipientVec,
	/// Account ID of the sender who submitted this entry.
	pub sender_id: SenderId,
	/// Whether this entry has been promoted to permanent on-chain storage.
	pub promoted: bool,
	/// `MultiSigner` of the account that signed the submission. The runtime pallet
	/// re-verifies the submit signature using this key when the unsigned promotion
	/// extrinsic lands on-chain.
	pub signer: MultiSigner,
	/// The user's `hop_submit` signature over `submit_signing_payload(blake2_256(data),
	/// submit_timestamp)`. Carried along for the runtime to re-verify; "submit implies
	/// consent to promote" is the protocol semantic.
	pub signature: MultiSignature,
	/// Submit-time wall-clock timestamp (ms since unix epoch) bound into the
	/// signing payload. The runtime rejects promotions whose timestamp is too far
	/// from on-chain time, so old `(data, signer, signature)` tuples cannot be
	/// replayed indefinitely.
	pub submit_timestamp: u64,
	/// Number of times the maintenance task has tried (and failed) to promote
	/// this entry. Used together with `next_promotion_attempt_at` for
	/// exponential back-off. Reset behavior: never reset — once an entry hits
	/// `MAX_PROMOTION_ATTEMPTS` it is left to expire normally.
	pub promotion_attempts: u8,
	/// Block height at which the next promotion attempt becomes eligible.
	/// `0` means "any tick"; non-zero means the maintenance task should skip
	/// this entry until the chain reaches this block.
	pub next_promotion_attempt_at: HopBlockNumber,
}

impl HopEntryMeta {
	/// Create a new entry metadata (without data blob)
	pub fn new(
		size: u64,
		expires_at: u64,
		recipients: RecipientVec,
		sender_id: SenderId,
		signer: MultiSigner,
		signature: MultiSignature,
		submit_timestamp: u64,
	) -> Self {
		Self {
			version: HOP_META_VERSION,
			expires_at,
			size,
			recipients,
			sender_id,
			promoted: false,
			signer,
			signature,
			submit_timestamp,
			promotion_attempts: 0,
			next_promotion_attempt_at: 0,
		}
	}
}

/// Maximum number of promotion attempts per entry before the maintenance
/// task gives up and lets the entry expire naturally. With the back-off
/// schedule below this caps wasted work at 1+2+4+8+16 = 31 check
/// intervals (~2.6 h at the default 5 min cadence) per stuck entry. The
/// first 5 attempts fit inside the default 2 h promotion buffer; the 6th
/// is an upper bound that may land past expiry on a stuck entry.
pub const MAX_PROMOTION_ATTEMPTS: u8 = 6;

/// Compute the back-off in blocks to wait before the next promotion attempt
/// after `attempts` consecutive failures. The first failure triggers a 1×
/// wait, doubling each subsequent failure: `1×, 2×, 4×, 8×, 16×, 32×` the
/// check interval, with the shift saturated to keep multiplication safe.
pub fn promotion_backoff_blocks(attempts: u8, check_interval_blocks: u32) -> u32 {
	let shift = attempts.saturating_sub(1).min(5) as u32;
	check_interval_blocks.saturating_mul(1u32 << shift)
}

/// Pool statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
	/// Number of entries in the pool
	pub entry_count: usize,
	/// Total bytes used
	pub total_bytes: u64,
	/// Maximum bytes allowed
	pub max_bytes: u64,
}

/// Result of a successful `hop_submit` call
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitResult {
	/// Current pool status after the submission
	pub pool_status: PoolStatus,
}

/// HOP errors
#[derive(Debug, thiserror::Error)]
pub enum HopError {
	#[error("Data too large: {0} bytes (max: {1})")]
	DataTooLarge(usize, u32),

	#[error("Pool full: {0}/{1} bytes used")]
	PoolFull(u64, u64),

	#[error("Data already exists in pool")]
	DuplicateEntry,

	#[error("Data not found")]
	NotFound,

	#[error("Invalid data: size cannot be zero")]
	EmptyData,

	#[error("Invalid signature")]
	InvalidSignature,

	#[error("Not an intended recipient")]
	NotRecipient,

	#[error("At least one recipient public key is required")]
	NoRecipients,

	#[error("Invalid recipient: failed to SCALE-decode MultiSigner")]
	InvalidRecipientKey,

	#[error("User quota exceeded: using {used} of {limit} bytes")]
	UserQuotaExceeded { used: u64, limit: u64 },

	#[error("Account does not have a valid authorization")]
	NotAuthorized,

	#[error("Invalid signer: failed to SCALE-decode MultiSigner")]
	InvalidSigner,

	#[error("I/O error: {0}")]
	IoError(#[from] std::io::Error),

	#[error("Recipient already acknowledged, data may have been deleted")]
	AlreadyClaimed,

	#[error("Invalid hash length: expected 32 bytes, got {0}")]
	InvalidHashLength(usize),

	#[error("Runtime API error: {0}")]
	RuntimeApiError(#[from] sp_api::ApiError),

	#[error("Too many recipients: {provided} (max {limit})")]
	TooManyRecipients { provided: usize, limit: usize },

	#[error("Duplicate recipient in list")]
	DuplicateRecipient,

	#[error("Rate limited: retry after {retry_after_secs}s")]
	RateLimited { retry_after_secs: u64 },

	#[error("No database path available and --hop-data-dir not specified")]
	MissingDataDir,
}

impl From<HopError> for jsonrpsee::types::ErrorObjectOwned {
	fn from(err: HopError) -> Self {
		let code = match err {
			HopError::DataTooLarge(_, _) => 1001,
			HopError::PoolFull(_, _) => 1002,
			HopError::DuplicateEntry => 1003,
			HopError::NotFound => 1004,
			HopError::EmptyData => 1005,
			HopError::InvalidSignature => 1007,
			HopError::NotRecipient => 1008,
			HopError::NoRecipients => 1009,
			HopError::InvalidRecipientKey => 1010,
			HopError::UserQuotaExceeded { .. } => 1011,
			HopError::NotAuthorized => 1012,
			HopError::IoError(_) => 1013,
			HopError::InvalidSigner => 1014,
			HopError::AlreadyClaimed => 1015,
			HopError::InvalidHashLength(_) => 1016,
			HopError::RuntimeApiError(_) => 1017,
			HopError::TooManyRecipients { .. } => 1018,
			HopError::DuplicateRecipient => 1019,
			HopError::RateLimited { .. } => 1020,
			HopError::MissingDataDir => 1021,
		};

		jsonrpsee::types::ErrorObject::owned(code, err.to_string(), None::<()>)
	}
}

/// Default retention period in seconds (24 hours).
pub const DEFAULT_RETENTION_SECS: u64 = 86_400;

/// Default maximum pool size in bytes (10 GiB)
pub const DEFAULT_MAX_POOL_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Default maximum pool size in MiB (10 GiB = 10240 MiB)
pub const DEFAULT_MAX_POOL_SIZE_MIB: u64 = DEFAULT_MAX_POOL_SIZE / (1024 * 1024);

/// Default maintenance interval in seconds (5 minutes)
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 300;

/// Block-time assumption used when translating the wall-clock maintenance
/// interval into block deltas for the promotion back-off scheduler.
pub const HOP_BLOCK_TIME_SECS: u64 = 6;

/// Maximum number of recipients allowed per submission.
///
/// Caps the fan-out so that per-entry metadata (both RAM and disk) is bounded
/// and `find_recipient`'s signature-verification scan is bounded.
pub const MAX_RECIPIENTS: u32 = 256;

/// A `Vec<Recipient>` that SCALE-decode rejects if it exceeds `MAX_RECIPIENTS`,
/// enforcing the fan-out cap at the type level instead of via scattered runtime checks.
pub type RecipientVec = BoundedVec<Recipient, ConstU32<MAX_RECIPIENTS>>;

/// Default per-user quota in MiB (256 MiB). Hard cap, not scaled by active users.
pub const DEFAULT_MAX_USER_SIZE_MIB: u64 = 256;

/// Default buffer before expiry at which to start promoting entries on-chain (2 h).
pub const DEFAULT_PROMOTION_BUFFER_SECS: u64 = 7200;

/// Default sustained submit rate per account (requests per minute).
pub const DEFAULT_SUBMIT_RATE_PER_MIN: u32 = 60;

/// Default submit burst per account (requests).
pub const DEFAULT_SUBMIT_BURST: u32 = 120;

/// Default sustained bandwidth per account in MiB per minute.
pub const DEFAULT_BANDWIDTH_PER_MIN_MIB: u64 = 128;

/// Default bandwidth burst per account in MiB.
pub const DEFAULT_BANDWIDTH_BURST_MIB: u64 = 256;

/// Domain-separator prefix for `hop_submit` signatures.
pub const HOP_SUBMIT_CONTEXT: &[u8] = b"hop-submit-v1:";

/// Domain-separator prefix for `hop_claim` signatures.
pub const HOP_CLAIM_CONTEXT: &[u8] = b"hop-claim-v1:";

/// Domain-separator prefix for `hop_ack` signatures.
pub const HOP_ACK_CONTEXT: &[u8] = b"hop-ack-v1:";

/// Compute the 32-byte payload that HOP recipients / submitters sign for a given
/// operation. This is `blake2_256(context || hash)` and ensures signatures from
/// one operation cannot be replayed in another.
pub fn signing_payload(context: &[u8], hash: &HopHash) -> [u8; 32] {
	let mut buf = Vec::with_capacity(context.len() + 32);
	buf.extend_from_slice(context);
	buf.extend_from_slice(hash.as_bytes());
	blake2_256(&buf)
}

/// Compute the 32-byte payload signed at `hop_submit` time.
///
/// The runtime pallet re-derives this exact byte sequence to verify the
/// signature on-chain, so the construction must remain byte-identical to the
/// pallet's `signing_payload(data, submit_timestamp)`:
/// `blake2_256(HOP_SUBMIT_CONTEXT || blake2_256(data) || submit_timestamp.to_le_bytes())`.
pub fn submit_signing_payload(hash: &HopHash, submit_timestamp: u64) -> [u8; 32] {
	let mut buf = [0u8; HOP_SUBMIT_CONTEXT.len() + 32 + 8];
	buf[..HOP_SUBMIT_CONTEXT.len()].copy_from_slice(HOP_SUBMIT_CONTEXT);
	buf[HOP_SUBMIT_CONTEXT.len()..HOP_SUBMIT_CONTEXT.len() + 32].copy_from_slice(hash.as_bytes());
	buf[HOP_SUBMIT_CONTEXT.len() + 32..].copy_from_slice(&submit_timestamp.to_le_bytes());
	blake2_256(&buf)
}

/// Per-recipient overhead charged against pool capacity and per-user quota, in bytes.
/// Covers the in-memory `Recipient` (a `MultiSigner` plus a `bool`). Kept as a
/// small constant that over-approximates `size_of::<Recipient>()`.
pub const METADATA_COST_PER_RECIPIENT: u64 = 40;

/// Total bytes an entry charges against pool capacity: the blob plus bounded
/// per-recipient metadata overhead.
pub fn entry_accounted_size(data_size: u64, num_recipients: usize) -> u64 {
	data_size.saturating_add((num_recipients as u64).saturating_mul(METADATA_COST_PER_RECIPIENT))
}
