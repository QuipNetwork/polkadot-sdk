// This file is part of Substrate.

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

//! Topic affinity filters for the `statement/2` protocol.
//!
//! A peer can advertise the set of topics it is interested in as a space-efficient bloom filter
//! ([`AffinityFilter`]). Once a peer has advertised one, only statements whose topics match the
//! filter are propagated to it; statements with no topics always match, as they are broadcasts.
//!
//! Filters received from the network are validated against `MAX_BLOOM_BITS` and `MAX_NUM_HASHES`
//! to bound the memory and CPU a malicious peer can force. Hashing is platform-independent so that
//! filter bits are identical on `wasm32` and 64-bit targets.

use crate::config::MAX_STATEMENT_NOTIFICATION_SIZE;
use codec::{Decode, Encode};
use fastbloom::{BloomFilter, DefaultHasher as BloomDefaultHasher};
use sp_statement_store::Statement;
use std::hash::{BuildHasher, Hasher};

/// Maximum number of bits allowed in a bloom filter received from the network.
/// Derived from [`MAX_STATEMENT_NOTIFICATION_SIZE`] so the affinity filter can never exceed the
/// protocol's notification budget. 1 MiB = 8_388_608 bits.
const MAX_BLOOM_BITS: usize = MAX_STATEMENT_NOTIFICATION_SIZE as usize * 8;

/// Maximum number of hash functions allowed.
/// Optimal hash count is `(bits / items) * ln(2)`. With the minimum allocation of 64 bits
/// and 1 expected item this yields ≈ 44, so the limit must be at least that high. 64 covers all
/// practical configurations while preventing CPU abuse from peers.
const MAX_NUM_HASHES: u32 = 64;

/// A [`BuildHasher`] factory that produces [`PortableHasher`] instances with
/// platform-independent hashing.  This ensures bloom-filter bits are identical
/// on `wasm32` and 64-bit targets when hashing types whose `Hash` impl calls
/// `write_usize` (e.g. slices, which hash their length).
#[derive(Clone, Debug)]
struct PortableBuildHasher(BloomDefaultHasher);

impl PortableBuildHasher {
	fn seeded(seed: u128) -> Self {
		Self(BloomDefaultHasher::seeded(&seed.to_le_bytes()))
	}
}

impl BuildHasher for PortableBuildHasher {
	type Hasher = PortableHasher;

	fn build_hasher(&self) -> Self::Hasher {
		PortableHasher(self.0.build_hasher())
	}
}

/// Hasher state returned by [`PortableBuildHasher`].  Delegates everything to
/// the inner SipHash-based hasher but overrides `write_usize` and `write_isize`
/// so that platform-width integers are always 8 bytes regardless of pointer
/// width.
#[derive(Clone)]
struct PortableHasher(<BloomDefaultHasher as BuildHasher>::Hasher);

impl Hasher for PortableHasher {
	#[inline]
	fn finish(&self) -> u64 {
		self.0.finish()
	}

	#[inline]
	fn write(&mut self, bytes: &[u8]) {
		self.0.write(bytes);
	}

	#[inline]
	fn write_usize(&mut self, i: usize) {
		// Always write as 8-byte little-endian so that `wasm32` (4-byte
		// usize) and 64-bit targets produce the same hash.
		self.0.write(&(i as u64).to_le_bytes());
	}

	#[inline]
	fn write_isize(&mut self, i: isize) {
		// Always write as 8-byte little-endian for the same reason as
		// `write_usize`.
		self.0.write(&(i as i64).to_le_bytes());
	}
}

/// Wire representation of a bloom filter.
#[derive(Encode, Decode)]
struct EncodedBloomFilter {
	// Seed used for hashing items in the bloom filter. Needed for the peer to reconstruct the same
	// bloom filter.
	seed: u128,
	// Number of hash functions used in the bloom filter. Needed for the peer to reconstruct the
	// same bloom filter.
	num_hashes: u32,
	// Bloom filter bits as a vector of u64. The bloom filter is reconstructed by the peer using
	// these bits.
	bits: Vec<u64>,
}

impl TryFrom<EncodedBloomFilter> for AffinityFilter {
	type Error = &'static str;

	fn try_from(encoded: EncodedBloomFilter) -> Result<Self, Self::Error> {
		if encoded.bits.is_empty() {
			return Err("bloom filter bits must not be empty");
		}
		if encoded.bits.len() * u64::BITS as usize > MAX_BLOOM_BITS {
			return Err("bloom filter bits exceed maximum allowed size");
		}
		if encoded.num_hashes == 0 || encoded.num_hashes > MAX_NUM_HASHES {
			return Err("num_hashes out of allowed range");
		}
		let bloom = BloomFilter::from_vec(encoded.bits)
			.hasher(PortableBuildHasher::seeded(encoded.seed))
			.hashes(encoded.num_hashes);
		Ok(AffinityFilter { bloom, seed: encoded.seed })
	}
}

#[derive(Debug)]
pub struct AffinityFilter {
	/// Bloom filter bytes representing the topics this peer is interested in.
	bloom: BloomFilter<PortableBuildHasher>,
	/// Seed used for hashing items in the bloom filter.
	seed: u128,
}

impl AffinityFilter {
	#[cfg(test)]
	pub(crate) fn new(seed: u128, false_pos: f64, expected_items: usize) -> Self {
		let bloom = BloomFilter::with_false_pos(false_pos)
			.hasher(PortableBuildHasher::seeded(seed))
			.expected_items(expected_items);
		AffinityFilter { bloom, seed }
	}

	/// Insert a topic into the bloom filter.
	#[cfg(test)]
	pub(crate) fn insert(&mut self, topic: &[u8; 32]) {
		self.bloom.insert(topic);
	}

	/// Check if a topic is likely present in the bloom filter.
	pub(crate) fn contains(&self, topic: &[u8; 32]) -> bool {
		self.bloom.contains(topic)
	}

	/// Check if a statement matches this affinity filter.
	///
	/// A statement matches if any of its topics is present in the bloom filter.
	/// Statements with no topics always match (they are broadcast statements).
	pub(crate) fn matches_statement(&self, statement: &Statement) -> bool {
		let topics = statement.topics();
		if topics.is_empty() {
			return true;
		}
		topics.iter().any(|topic| self.contains(topic))
	}
}

impl Encode for AffinityFilter {
	fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
		let encoded = EncodedBloomFilter {
			seed: self.seed,
			num_hashes: self.bloom.num_hashes(),
			bits: self.bloom.as_slice().to_vec(),
		};
		encoded.encode_to(dest);
	}
}

impl Decode for AffinityFilter {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let encoded = EncodedBloomFilter::decode(input)?;
		AffinityFilter::try_from(encoded).map_err(|e| codec::Error::from(e))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Default seed used for bloom filters in tests.
	const BLOOM_SEED: u128 = 0x5EED_5EED_5EED_5EED;

	/// Maximum u64 words derived from [`MAX_BLOOM_BITS`] for use in tests.
	const MAX_BLOOM_WORDS: usize = MAX_BLOOM_BITS / u64::BITS as usize;

	#[test]
	fn affinity_filter_encode_decode_roundtrip() {
		const TOTAL: usize = 100_000;
		const SET_COUNT: usize = TOTAL / 10; // 10% inserted

		// Generate 100k unique [u8; 32] items from their index.
		let items: Vec<[u8; 32]> = (0..TOTAL)
			.map(|i| {
				let mut key = [0u8; 32];
				key[..8].copy_from_slice(&(i as u64).to_le_bytes());
				key
			})
			.collect();

		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, SET_COUNT);

		// Insert first 10% of items.
		for item in &items[..SET_COUNT] {
			filter.insert(item);
		}

		// Record expected check result for every item before serialization.
		let expected: Vec<bool> = items.iter().map(|item| filter.contains(item)).collect();

		// Inserted items must always be present.
		for i in 0..SET_COUNT {
			assert!(expected[i], "inserted item {i} must be present");
		}

		let encoded = filter.encode();
		let decoded =
			AffinityFilter::decode(&mut encoded.as_slice()).expect("decoding should succeed");

		// Every item must give the same answer as before serialization.
		for (i, item) in items.iter().enumerate() {
			assert_eq!(decoded.contains(item), expected[i], "mismatch for item {i}");
		}

		// Re-encoding must produce identical bytes.
		assert_eq!(encoded, decoded.encode(), "re-encoding should produce identical bytes");
	}

	/// Snapshot test for AffinityFilter wire format with 10 000 items.
	///
	/// Verifies that the encoding length, header, and trailer match a known
	/// snapshot and that decoding preserves bloom filter contents.
	/// If this test breaks, the wire format has changed and needs a migration.
	#[test]
	fn affinity_filter_encoding_snapshot() {
		const ITEM_COUNT: usize = 10_000;

		let items: Vec<[u8; 32]> = (0..ITEM_COUNT)
			.map(|i| {
				let mut key = [0u8; 32];
				key[..8].copy_from_slice(&(i as u64).to_le_bytes());
				key
			})
			.collect();

		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, ITEM_COUNT);
		for item in &items {
			filter.insert(item);
		}

		let encoded = filter.encode();

		// Fixed snapshot — if this changes the wire format has been modified.
		assert_eq!(
			sp_crypto_hashing::blake2_256(&encoded),
			[
				180, 34, 58, 78, 198, 24, 137, 83, 154, 127, 9, 152, 171, 50, 197, 27, 242, 158,
				30, 79, 143, 192, 53, 151, 174, 106, 132, 105, 20, 145, 133, 0
			],
			"blake2_256 digest of encoded bytes must match snapshot"
		);

		// Verify the snapshot decodes correctly and all items round-trip.
		let decoded =
			AffinityFilter::decode(&mut encoded.as_slice()).expect("snapshot must decode");
		for (i, item) in items.iter().enumerate() {
			assert!(decoded.contains(item), "item {i} must be present after decoding");
		}

		// A non-inserted item should not match.
		let absent: [u8; 32] = [0xFF; 32];
		assert!(!decoded.contains(&absent), "absent item must not match");
	}

	#[test]
	fn matches_statement_no_topics_always_matches() {
		let filter = AffinityFilter::new(BLOOM_SEED, 0.01, 10);

		let mut stmt = Statement::new();
		stmt.set_plain_data(b"broadcast".to_vec());
		assert!(filter.matches_statement(&stmt));
	}

	#[test]
	fn matches_statement_single_matching_topic() {
		let topic: [u8; 32] = [0xAA; 32];
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 10);
		filter.insert(&topic);

		let mut stmt = Statement::new();
		stmt.set_plain_data(b"matching".to_vec());
		stmt.set_topic(0, topic.into());
		assert!(filter.matches_statement(&stmt));
	}

	#[test]
	fn matches_statement_single_non_matching_topic() {
		let topic_in_filter: [u8; 32] = [0xAA; 32];
		let topic_on_stmt: [u8; 32] = [0xBB; 32];
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 10);
		filter.insert(&topic_in_filter);

		let mut stmt = Statement::new();
		stmt.set_plain_data(b"not matching".to_vec());
		stmt.set_topic(0, topic_on_stmt.into());
		assert!(!filter.matches_statement(&stmt));
	}

	#[test]
	fn matches_statement_multiple_topics_any_semantics() {
		let topic_aa: [u8; 32] = [0xAA; 32];
		let topic_bb: [u8; 32] = [0xBB; 32];
		let topic_cc: [u8; 32] = [0xCC; 32];

		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 10);
		filter.insert(&topic_bb);

		let mut stmt = Statement::new();
		stmt.set_plain_data(b"multi topic".to_vec());
		stmt.set_topic(0, topic_aa.into());
		stmt.set_topic(1, topic_bb.into());
		assert!(filter.matches_statement(&stmt), "should match when ANY topic is in the filter");

		let mut stmt2 = Statement::new();
		stmt2.set_plain_data(b"no match multi".to_vec());
		stmt2.set_topic(0, topic_aa.into());
		stmt2.set_topic(1, topic_cc.into());
		assert!(
			!filter.matches_statement(&stmt2),
			"should not match when NO topic is in the filter"
		);
	}

	#[test]
	fn decode_rejects_empty_bits() {
		let encoded = EncodedBloomFilter { seed: BLOOM_SEED, num_hashes: 7, bits: vec![] };
		let bytes = encoded.encode();
		assert!(AffinityFilter::decode(&mut bytes.as_slice()).is_err());
	}

	#[test]
	fn decode_rejects_oversized_bits() {
		let encoded = EncodedBloomFilter {
			seed: BLOOM_SEED,
			num_hashes: 7,
			bits: vec![0u64; MAX_BLOOM_WORDS + 1],
		};
		let bytes = encoded.encode();
		assert!(AffinityFilter::decode(&mut bytes.as_slice()).is_err());
	}

	#[test]
	fn decode_rejects_zero_num_hashes() {
		let encoded = EncodedBloomFilter { seed: BLOOM_SEED, num_hashes: 0, bits: vec![0u64; 16] };
		let bytes = encoded.encode();
		assert!(AffinityFilter::decode(&mut bytes.as_slice()).is_err());
	}

	#[test]
	fn decode_rejects_excessive_num_hashes() {
		let encoded =
			EncodedBloomFilter { seed: BLOOM_SEED, num_hashes: u32::MAX, bits: vec![0u64; 16] };
		let bytes = encoded.encode();
		assert!(AffinityFilter::decode(&mut bytes.as_slice()).is_err());
	}

	#[test]
	fn decode_accepts_valid_bounds() {
		let encoded = EncodedBloomFilter {
			seed: BLOOM_SEED,
			num_hashes: MAX_NUM_HASHES,
			bits: vec![0u64; MAX_BLOOM_WORDS],
		};
		let bytes = encoded.encode();
		assert!(AffinityFilter::decode(&mut bytes.as_slice()).is_ok());
	}
}
