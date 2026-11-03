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

//! Tests for fork-aware transaction pool.

use fatp_common::{
	finalized_block_event, invalid_hash, new_best_block_event, pool, pool_with_api,
	test_chain_with_forks, LOG_TARGET, SOURCE,
};
use futures::{executor::block_on, task::Poll, FutureExt, StreamExt};
use sc_transaction_pool::ChainApi;
use sc_transaction_pool_api::{
	error::Error as TxPoolError, ChainEvent, MaintainedTransactionPool, TransactionPool,
	TransactionStatus,
};
use sp_runtime::transaction_validity::InvalidTransaction;
use std::{sync::Arc, time::Duration};
use substrate_test_runtime_client::{
	runtime::{Extrinsic, Hash},
	Sr25519Keyring::*,
};
use substrate_test_runtime_transaction_pool::{uxt, TestApi};
use tracing::debug;

pub mod fatp_common;

// Some ideas for tests:
// - view.ready iterator
// - stale transaction submission when there is single view only (expect error)
// - stale transaction submission when there are more views (expect ok if tx is ok for at least one
//   view)
// - view count (e.g. same new block notified twice)
// - invalid with many views (different cases)
//
// review (from old pool) and maybe re-use:
// fn import_notification_to_pool_maintain_works()
// fn prune_tags_should_work()
// fn should_ban_invalid_transactions()
// fn should_correctly_prune_transactions_providing_more_than_one_tag()

#[test]
fn fatp_no_view_future_and_ready_submit_one_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_one(header.hash(), SOURCE, xt0.clone()),
		pool.submit_one(header.hash(), SOURCE, xt1.clone()),
	];

	let results = block_on(futures::future::join_all(submissions));

	assert!(results.iter().all(|r| { r.is_ok() }));
}

#[test]
fn fatp_no_view_future_and_ready_submit_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);

	let xts0 = (200..205).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts1 = (205..210).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts2 = (215..220).map(|i| uxt(Alice, i)).collect::<Vec<_>>();

	let submissions = vec![
		pool.submit_at(header.hash(), SOURCE, xts0.clone()),
		pool.submit_at(header.hash(), SOURCE, xts1.clone()),
		pool.submit_at(header.hash(), SOURCE, xts2.clone()),
	];

	let results = block_on(futures::future::join_all(submissions));

	assert!(results.into_iter().flat_map(|x| x.unwrap()).all(|r| { r.is_ok() }));
}

#[test]
fn fatp_no_view_submit_already_imported_reports_error() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);

	let xts0 = (215..220).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts1 = xts0.clone();

	let submission_ok = pool.submit_at(header.hash(), SOURCE, xts0.clone());
	let results = block_on(submission_ok);
	assert!(results.unwrap().into_iter().all(|r| r.is_ok()));

	let submission_failing = pool.submit_at(header.hash(), SOURCE, xts1.clone());
	let results = block_on(submission_failing);

	assert!(results
		.unwrap()
		.into_iter()
		.all(|r| { matches!(r.unwrap_err().0, TxPoolError::AlreadyImported(_)) }));
}

#[test]
fn fatp_one_view_future_and_ready_submit_one_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);
	// let header01b = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_one(header.hash(), SOURCE, xt0.clone()),
		pool.submit_one(header.hash(), SOURCE, xt1.clone()),
	];

	block_on(futures::future::join_all(submissions));

	assert_pool_status!(header.hash(), &pool, 1, 1);
}

#[test]
fn fatp_one_view_future_and_ready_submit_many_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);
	// let header01b = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header.hash());
	block_on(pool.maintain(event));

	let xts0 = (200..205).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts1 = (205..210).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts2 = (215..220).map(|i| uxt(Alice, i)).collect::<Vec<_>>();

	let submissions = vec![
		pool.submit_at(header.hash(), SOURCE, xts0.clone()),
		pool.submit_at(header.hash(), SOURCE, xts1.clone()),
		pool.submit_at(header.hash(), SOURCE, xts2.clone()),
	];

	block_on(futures::future::join_all(submissions));

	assert_pool_status!(header.hash(), &pool, 10, 5);
}

#[test]
fn fatp_one_view_stale_submit_one_fails() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 100);
	let submissions = vec![pool.submit_one(invalid_hash(), SOURCE, xt0.clone())];
	let results = block_on(futures::future::join_all(submissions));

	// xt0 should be stale
	assert!(matches!(
		&results[0].as_ref().unwrap_err().0,
		TxPoolError::InvalidTransaction(InvalidTransaction::Stale,)
	));

	assert_pool_status!(header.hash(), &pool, 0, 0);
}

#[test]
fn fatp_one_view_stale_submit_many_fails() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header.hash());
	block_on(pool.maintain(event));

	let xts0 = (100..105).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts1 = (105..110).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts2 = (195..201).map(|i| uxt(Alice, i)).collect::<Vec<_>>();

	let submissions = vec![
		pool.submit_at(header.hash(), SOURCE, xts0.clone()),
		pool.submit_at(header.hash(), SOURCE, xts1.clone()),
		pool.submit_at(header.hash(), SOURCE, xts2.clone()),
	];

	let results = block_on(futures::future::join_all(submissions));

	// xts2 contains one ready transaction (nonce:200)
	let mut results = results.into_iter().flat_map(|x| x.unwrap()).collect::<Vec<_>>();
	debug!(?results, "Results debug output");
	assert!(results.pop().unwrap().is_ok());
	assert!(results.into_iter().all(|r| {
		matches!(
			&r.as_ref().unwrap_err().0,
			TxPoolError::InvalidTransaction(InvalidTransaction::Stale,)
		)
	}));

	assert_pool_status!(header.hash(), &pool, 1, 0);
}

#[test]
fn fatp_one_view_future_turns_to_ready_works() {
	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);
	let at = header.hash();
	let event = new_best_block_event(&pool, None, at);
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 201);
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	assert!(pool.ready().count() == 0);
	assert_pool_status!(at, &pool, 0, 1);

	let xt1 = uxt(Alice, 200);
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let ready: Vec<_> = pool.ready().map(|v| (*v.data).clone()).collect();
	assert_eq!(ready, vec![xt1, xt0]);
	assert_pool_status!(at, &pool, 2, 0);
}

#[test]
fn fatp_one_view_ready_gets_pruned() {
	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);
	let block1 = header.hash();
	let event = new_best_block_event(&pool, None, block1);
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let pending: Vec<_> = pool.ready().map(|v| (*v.data).clone()).collect();
	assert_eq!(pending, vec![xt0.clone()]);
	assert_eq!(pool.status_all()[&block1].ready, 1);

	let header = api.push_block(2, vec![xt0], true);
	let block2 = header.hash();
	let event = new_best_block_event(&pool, Some(block1), block2);
	block_on(pool.maintain(event));
	assert_pool_status!(block2, &pool, 0, 0);
	assert!(pool.ready().count() == 0);
}

#[test]
fn fatp_one_view_ready_turns_to_stale_works() {
	let (pool, api, _) = pool();

	let header = api.push_block(1, vec![], true);
	let block1 = header.hash();
	let event = new_best_block_event(&pool, None, block1);
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let pending: Vec<_> = pool.ready().map(|v| (*v.data).clone()).collect();
	assert_eq!(pending, vec![xt0.clone()]);
	assert_eq!(pool.status_all()[&block1].ready, 1);

	let header = api.push_block(2, vec![], true);
	let block2 = header.hash();
	// tricky: typically the block2 shall contain conflicting transaction for Alice. In this test we
	// want to check revalidation, so we manually adjust nonce.
	api.set_nonce(block2, Alice.into(), 201);
	let event = new_best_block_event(&pool, Some(block1), block2);
	// note: blocking revalidation (w/o background worker) which is used in this test will detect
	// xt0 is stale
	block_on(pool.maintain(event));
	// todo: should it work at all? (it requires better revalidation: mempool keeping validated txs)
	// assert_pool_status!(block2, &pool, 0, 0);
	// assert!(pool.ready(block2).unwrap().count() == 0);
}

#[test]
fn fatp_two_views_future_and_ready_submit_one() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let genesis = api.genesis_hash();
	let header01a = api.push_block(1, vec![], true);
	let header01b = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01a.hash());
	block_on(pool.maintain(event));

	let event = new_best_block_event(&pool, None, header01b.hash());
	block_on(pool.maintain(event));

	api.set_nonce(header01b.hash(), Alice.into(), 202);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_one(genesis, SOURCE, xt0.clone()),
		pool.submit_one(genesis, SOURCE, xt1.clone()),
	];

	block_on(futures::future::join_all(submissions));

	assert_pool_status!(header01a.hash(), &pool, 1, 1);
	assert_pool_status!(header01b.hash(), &pool, 1, 0);
}

#[test]
fn fatp_two_views_future_and_ready_submit_many() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01a = api.push_block(1, vec![], true);
	let header01b = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01a.hash());
	block_on(pool.maintain(event));

	let event = new_best_block_event(&pool, None, header01b.hash());
	block_on(pool.maintain(event));

	api.set_nonce(header01b.hash(), Alice.into(), 215);

	let xts0 = (200..205).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts1 = (205..210).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	let xts2 = (215..220).map(|i| uxt(Alice, i)).collect::<Vec<_>>();

	let submissions = vec![
		pool.submit_at(invalid_hash(), SOURCE, xts0.clone()),
		pool.submit_at(invalid_hash(), SOURCE, xts1.clone()),
		pool.submit_at(invalid_hash(), SOURCE, xts2.clone()),
	];

	block_on(futures::future::join_all(submissions));

	debug!(target: LOG_TARGET, status = ?pool.status_all(), "stats");

	assert_pool_status!(header01a.hash(), &pool, 10, 5);
	assert_pool_status!(header01b.hash(), &pool, 5, 0);
}

#[test]
fn fatp_two_views_submit_many_variations() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt0 = uxt(Alice, 206);
	let xt1 = uxt(Alice, 206);

	let result = block_on(pool.submit_one(invalid_hash(), SOURCE, xt1.clone()));
	assert!(result.is_ok());

	let header01a = api.push_block(1, vec![xt0.clone()], true);
	let header01b = api.push_block(1, vec![xt0.clone()], true);

	api.set_nonce(header01a.hash(), Alice.into(), 201);
	api.set_nonce(header01b.hash(), Alice.into(), 202);

	let event = new_best_block_event(&pool, None, header01a.hash());
	block_on(pool.maintain(event));

	let event = new_best_block_event(&pool, None, header01b.hash());
	block_on(pool.maintain(event));

	let mut xts = (199..204).map(|i| uxt(Alice, i)).collect::<Vec<_>>();
	xts.push(xt0);
	xts.push(xt1);

	let results = block_on(pool.submit_at(invalid_hash(), SOURCE, xts.clone())).unwrap();

	debug!(target: LOG_TARGET, ?results, "res");
	debug!(target: LOG_TARGET, pool_status = ?pool.status_all(), "stats");

	(0..2).for_each(|i| {
		assert!(matches!(
			results[i].as_ref().unwrap_err().0,
			TxPoolError::InvalidTransaction(InvalidTransaction::Stale,)
		));
	});
	// note: tx at 2 is valid at header01a and invalid at header01b
	(2..5).for_each(|i| {
		assert_eq!(*results[i].as_ref().unwrap(), api.hash_and_length(&xts[i]).0);
	});
	// xt0 at index 5 (transaction from the imported block, gets banned when pruned)
	assert!(matches!(results[5].as_ref().unwrap_err().0, TxPoolError::TemporarilyBanned));
	// xt1 at index 6
	assert!(matches!(results[6].as_ref().unwrap_err().0, TxPoolError::AlreadyImported(_)));
}

#[test]
fn fatp_linear_progress() {
	sp_tracing::try_init_simple();

	let (api, forks) = test_chain_with_forks::chain(None);
	let (pool, _) = pool_with_api(api.clone());

	let f11 = forks[1][1].hash();
	let f13 = forks[1][3].hash();

	let event = new_best_block_event(&pool, None, f11);
	block_on(pool.maintain(event));

	let xt0 = uxt(Bob, 203);
	let submissions = vec![pool.submit_one(invalid_hash(), SOURCE, xt0.clone())];

	block_on(futures::future::join_all(submissions));

	let event = new_best_block_event(&pool, Some(f11), f13);
	debug!(target: LOG_TARGET, ?event, "event");
	block_on(pool.maintain(event));

	// note: we only keep tip of the fork
	assert_eq!(pool.active_views_count(), 1);
	assert_pool_status!(f13, &pool, 1, 0);
}

#[test]
fn fatp_linear_old_ready_becoming_stale() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	// Our initial transactions
	let xts = vec![uxt(Alice, 300), uxt(Alice, 301), uxt(Alice, 302)];

	let header01 = api.push_block(1, vec![], true);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	xts.into_iter().for_each(|xt| {
		block_on(pool.submit_one(invalid_hash(), SOURCE, xt)).unwrap();
	});
	assert_eq!(pool.status_all()[&header01.hash()].ready, 0);
	assert_eq!(pool.status_all()[&header01.hash()].future, 3);

	// Import enough blocks to make our transactions stale (longevity is 64)
	let mut prev_header = header01;
	for n in 2..66 {
		let header = api.push_block(n, vec![], true);
		let event = new_best_block_event(&pool, Some(prev_header.hash()), header.hash());
		block_on(pool.maintain(event));

		if n == 65 {
			assert_eq!(pool.status_all()[&header.hash()].ready, 0);
			assert_eq!(pool.status_all()[&header.hash()].future, 0);
		} else {
			assert_eq!(pool.status_all()[&header.hash()].ready, 0);
			assert_eq!(pool.status_all()[&header.hash()].future, 3);
		}
		prev_header = header;
	}
}

#[test]
fn fatp_proper_cleanup_after_mortal_tx_becoming_invalid() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xts = vec![uxt(Alice, 200), uxt(Alice, 201), uxt(Alice, 202)];

	api.set_valid_till(&xts[0], 66);
	api.set_valid_till(&xts[1], 66);
	api.set_valid_till(&xts[2], 66);

	let header01 = api.push_block(1, vec![], true);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	xts.into_iter().for_each(|xt| {
		block_on(pool.submit_one(invalid_hash(), SOURCE, xt)).unwrap();
	});
	assert_eq!(pool.status_all()[&header01.hash()].ready, 3);
	assert_eq!(pool.status_all()[&header01.hash()].future, 0);

	// Import enough blocks to make our transactions stale (longevity is 64)
	let mut prev_header = header01;
	for n in 2..67 {
		let header = api.push_block_with_parent(prev_header.hash(), vec![], true);
		let event = new_best_block_event(&pool, Some(prev_header.hash()), header.hash());
		block_on(pool.maintain(event));

		if n == 66 {
			assert_eq!(pool.status_all()[&header.hash()].ready, 0);
			assert_eq!(pool.status_all()[&header.hash()].future, 0);
		} else {
			assert_eq!(pool.status_all()[&header.hash()].ready, 3);
			assert_eq!(pool.status_all()[&header.hash()].future, 0);
		}
		prev_header = header;
	}

	let header = api.push_block_with_parent(prev_header.hash(), vec![], true);
	let event = finalized_block_event(&pool, prev_header.hash(), header.hash());
	block_on(pool.maintain(event));
	assert_eq!(pool.import_notification_sink_len(), 0);
}

#[test]
fn fatp_fork_reorg() {
	sp_tracing::try_init_simple();

	let (api, forks) = test_chain_with_forks::chain(None);
	let (pool, _) = pool_with_api(api.clone());

	let f03 = forks[0][3].hash();
	let f13 = forks[1][3].hash();

	let event = new_best_block_event(&pool, None, f03);
	block_on(pool.maintain(event));

	let xt0 = uxt(Bob, 203);
	let xt1 = uxt(Bob, 204);
	let xt2 = uxt(Alice, 203);
	let submissions = vec![
		pool.submit_one(invalid_hash(), SOURCE, xt0.clone()),
		pool.submit_one(invalid_hash(), SOURCE, xt1.clone()),
		pool.submit_one(invalid_hash(), SOURCE, xt2.clone()),
	];

	block_on(futures::future::join_all(submissions));

	let event = new_best_block_event(&pool, Some(f03), f13);
	debug!(target: LOG_TARGET, ?event, "event");
	block_on(pool.maintain(event));

	assert_pool_status!(f03, &pool, 1, 2);
	assert_pool_status!(f13, &pool, 6, 0);

	// check if ready for block[1][3] contains resubmitted transactions
	let mut expected = forks[0]
		.iter()
		.take(4)
		.flat_map(|h| block_on(api.block_body(h.hash())).unwrap().unwrap())
		.collect::<Vec<_>>();
	expected.extend_from_slice(&[xt0, xt1, xt2]);

	let ready_f13 = pool.ready().collect::<Vec<_>>();
	expected.iter().for_each(|e| {
		assert!(ready_f13.iter().any(|v| *v.data == *e));
	});
	assert_eq!(expected.len(), ready_f13.len());
}

/// Builds two competing forks rooted at genesis and returns
/// `(api, [ta0, tb0, ta1, tb1], retracted_tip, enacted_tip)`.
///
/// The fork that will be retracted interleaves Alice's and Bob's transactions, in
/// execution order: ta0, tb0, ta1, tb1. The competing (enacted) fork is empty, so both
/// accounts' nonces remain at 200 on it and all four transactions are valid to resubmit:
///
///   retracted:  G - R1[ta0] - R2[tb0] - R3[ta1] - R4[tb1]
///   enacted:    G - E1 - E2 - E3 - E4
///
/// Stale-check is enabled in TestApi.
fn fork_with_interleaved_retracted_txs() -> (Arc<TestApi>, [Extrinsic; 4], Hash, Hash) {
	let api = Arc::from(TestApi::empty().enable_stale_check());
	let genesis = api.genesis_hash();
	api.set_nonce(genesis, Alice.into(), 200);
	api.set_nonce(genesis, Bob.into(), 200);

	let ta0 = uxt(Alice, 200);
	let tb0 = uxt(Bob, 200);
	let ta1 = uxt(Alice, 201);
	let tb1 = uxt(Bob, 201);

	// fork that will be retracted (execution order: ta0, tb0, ta1, tb1)
	let r1 = api.push_block_with_parent(genesis, vec![ta0.clone()], true);
	api.set_nonce(r1.hash(), Alice.into(), 201);
	let r2 = api.push_block_with_parent(r1.hash(), vec![tb0.clone()], true);
	api.set_nonce(r2.hash(), Bob.into(), 201);
	let r3 = api.push_block_with_parent(r2.hash(), vec![ta1.clone()], true);
	api.set_nonce(r3.hash(), Alice.into(), 202);
	let r4 = api.push_block_with_parent(r3.hash(), vec![tb1.clone()], true);
	api.set_nonce(r4.hash(), Bob.into(), 202);

	// competing empty fork; nonces stay at 200 for both accounts (inherited from parent)
	let e1 = api.push_block_with_parent(genesis, vec![], true);
	let e2 = api.push_block_with_parent(e1.hash(), vec![], true);
	let e3 = api.push_block_with_parent(e2.hash(), vec![], true);
	let e4 = api.push_block_with_parent(e3.hash(), vec![], true);

	(api, [ta0, tb0, ta1, tb1], r4.hash(), e4.hash())
}

#[test]
fn fatp_fork_reorg_preserves_retracted_tx_order() {
	sp_tracing::try_init_simple();

	// On a re-org, transactions from retracted blocks are resubmitted in execution order
	// (common ancestor -> old best). With equal priority and longevity the ready queue's
	// FIFO tie-break then preserves the original on-chain ordering, even when nonces from
	// different accounts are interleaved across the retracted blocks.
	let (api, [ta0, tb0, ta1, tb1], retracted_tip, enacted_tip) =
		fork_with_interleaved_retracted_txs();
	let (pool, _) = pool_with_api(api.clone());

	// establish the retracted fork as the current best
	block_on(pool.maintain(new_best_block_event(&pool, None, retracted_tip)));

	// re-org onto the empty fork: ta0, tb0, ta1, tb1 are resubmitted from retracted blocks
	block_on(pool.maintain(new_best_block_event(&pool, Some(retracted_tip), enacted_tip)));

	assert_pool_status!(enacted_tip, &pool, 4, 0);
	// execution order is preserved (without the fix the order would be [tb0, tb1, ta0, ta1])
	assert_ready_iterator!(enacted_tip, &pool, [ta0, tb0, ta1, tb1]);
}

#[test]
fn fatp_fork_reorg_respects_priority_over_order() {
	sp_tracing::try_init_simple();

	// Order preservation on re-org only holds while priorities are equal: priority is the
	// dominant ready-queue ordering key. Here distinct priorities reshuffle the inter-account
	// interleaving (ta1 overtakes tb0), while the nonce dependency still forces ta1 after its
	// parent ta0 despite ta1 having the highest priority.
	let (api, [ta0, tb0, ta1, tb1], retracted_tip, enacted_tip) =
		fork_with_interleaved_retracted_txs();
	let (pool, _) = pool_with_api(api.clone());

	api.set_priority(&ta0, 10);
	api.set_priority(&tb0, 10);
	api.set_priority(&ta1, 20);
	api.set_priority(&tb1, 1);

	block_on(pool.maintain(new_best_block_event(&pool, None, retracted_tip)));
	block_on(pool.maintain(new_best_block_event(&pool, Some(retracted_tip), enacted_tip)));

	assert_pool_status!(enacted_tip, &pool, 4, 0);
	// chain order is [ta0, tb0, ta1, tb1]; priority reorders it to:
	assert_ready_iterator!(enacted_tip, &pool, [ta0, ta1, tb0, tb1]);
}

#[test]
fn fatp_fork_do_resubmit_same_tx() {
	let xt = uxt(Alice, 200);

	let (pool, api, _) = pool();
	let header01 = api.push_block(1, vec![], true);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	block_on(pool.submit_one(api.expect_hash_from_number(0), SOURCE, xt.clone())).unwrap();
	assert_eq!(pool.status_all()[&header01.hash()].ready, 1);

	let header02a = api.push_block(1, vec![xt.clone()], true);
	let header02b = api.push_block(1, vec![xt], true);

	let event = new_best_block_event(&pool, Some(header02a.hash()), header02b.hash());
	api.set_nonce(header02a.hash(), Alice.into(), 201);
	block_on(pool.maintain(event));
	assert_eq!(pool.status_all()[&header02b.hash()].ready, 0);

	let event = new_best_block_event(&pool, Some(api.genesis_hash()), header02b.hash());
	api.set_nonce(header02b.hash(), Alice.into(), 201);
	block_on(pool.maintain(event));

	assert_eq!(pool.status_all()[&header02b.hash()].ready, 0);
}

#[test]
fn fatp_fork_stale_rejected() {
	sp_tracing::try_init_simple();

	// note: there are no xts in blocks on fork 0!
	let (api, forks) = test_chain_with_forks::chain(Some(&|f, b| match (f, b) {
		(0, _) => false,
		_ => true,
	}));
	let (pool, _) = pool_with_api(api.clone());

	let f03 = forks[0][3].hash();
	let f13 = forks[1][3].hash();

	//     n:201   n:202   n:203    <-- alice nonce
	//     F01   - F02   - F03      <-- xt2 is stale
	//    /
	// F00
	//    \
	//     F11[t0] - F12[t1] - F13[t2]
	//     n:201     n:202    n:203    <-- bob nonce
	//
	//   t0 = uxt(Bob,200)
	//   t1 = uxt(Bob,201)
	//   t2 = uxt(Bob,201)
	//  xt0 = uxt(Bob, 203)
	//  xt1 = uxt(Bob, 204)
	//  xt2 = uxt(Alice, 201);

	let event = new_best_block_event(&pool, None, f03);
	block_on(pool.maintain(event));

	let xt0 = uxt(Bob, 203);
	let xt1 = uxt(Bob, 204);
	let xt2 = uxt(Alice, 201);
	let submissions = vec![
		pool.submit_one(invalid_hash(), SOURCE, xt0.clone()),
		pool.submit_one(invalid_hash(), SOURCE, xt1.clone()),
		pool.submit_one(invalid_hash(), SOURCE, xt2.clone()),
	];
	let submission_results = block_on(futures::future::join_all(submissions));
	let futures_f03 = pool.futures();

	// xt2 should be stale
	assert!(matches!(
		&submission_results[2].as_ref().unwrap_err().0,
		TxPoolError::InvalidTransaction(InvalidTransaction::Stale,)
	));

	let event = new_best_block_event(&pool, Some(f03), f13);
	debug!(target: LOG_TARGET, ?event, "event");
	block_on(pool.maintain(event));

	assert_pool_status!(f03, &pool, 0, 2);

	// xt2 was removed from the pool, it is not becoming future:
	// note: theoretically we could keep xt2 in the pool, even if it was reported as stale. But it
	// seems to be an unnecessary complication.
	assert_pool_status!(f13, &pool, 2, 0);

	let futures_f13 = pool.futures();
	let ready_f13 = pool.ready().collect::<Vec<_>>();
	assert!(futures_f13.iter().next().is_none());
	assert!(futures_f03.iter().any(|v| *v.data == xt0));
	assert!(futures_f03.iter().any(|v| *v.data == xt1));
	assert!(ready_f13.iter().any(|v| *v.data == xt0));
	assert!(ready_f13.iter().any(|v| *v.data == xt1));
}

#[test]
fn fatp_fork_no_xts_ready_switch_to_future() {
	// this scenario w/o xts is not likely to happen, but similar thing (xt changing from ready to
	// future) could occur e.g. when runtime was updated on fork1.
	sp_tracing::try_init_simple();

	// note: there are no xts in blocks!
	let (api, forks) = test_chain_with_forks::chain(Some(&|_, _| false));
	let (pool, _) = pool_with_api(api.clone());

	let f03 = forks[0][3].hash();
	let f12 = forks[1][2].hash();

	let event = new_best_block_event(&pool, None, f03);
	block_on(pool.maintain(event));

	// xt0 is ready on f03, but future on f12, f13
	let xt0 = uxt(Alice, 203);
	let submissions = vec![pool.submit_one(invalid_hash(), SOURCE, xt0.clone())];
	block_on(futures::future::join_all(submissions));

	let event = new_best_block_event(&pool, Some(f03), f12);
	block_on(pool.maintain(event));

	assert_pool_status!(f03, &pool, 1, 0);
	// f12 was not updated - xt0 is still ready there
	// (todo: can we do better? shall we revalidate all future xts?)
	assert_pool_status!(f12, &pool, 1, 0);

	// xt0 becomes future, and this may only happen after view revalidation (which happens on
	// finalization). So trigger it.
	let event = finalized_block_event(&pool, api.genesis_hash(), f12);
	block_on(pool.maintain(event));

	// f03 still dangling
	assert_eq!(pool.active_views_count(), 2);

	// wait 10 blocks for revalidation and 1 extra for applying revalidation results
	let mut prev_header = forks[1][2].clone();
	for _ in 3..=12 {
		let header = api.push_block_with_parent(prev_header.hash(), vec![], true);
		let event = finalized_block_event(&pool, prev_header.hash(), header.hash());
		block_on(pool.maintain(event));
		prev_header = header;
	}

	assert_pool_status!(prev_header.hash(), &pool, 0, 1);
}

#[test]
fn fatp_ready_at_does_not_trigger() {
	sp_tracing::try_init_simple();

	let (api, forks) = test_chain_with_forks::chain(None);
	let (pool, _) = pool_with_api(api.clone());

	let f03 = forks[0][3].hash();
	let f13 = forks[1][3].hash();

	assert!(pool.ready_at(f03).now_or_never().is_none());
	assert!(pool.ready_at(f13).now_or_never().is_none());
}

#[test]
fn fatp_ready_at_does_not_trigger_after_submit() {
	sp_tracing::try_init_simple();

	let (api, forks) = test_chain_with_forks::chain(None);
	let (pool, _) = pool_with_api(api.clone());

	let xt0 = uxt(Alice, 200);
	let _ = block_on(pool.submit_one(invalid_hash(), SOURCE, xt0));

	let f03 = forks[0][3].hash();
	let f13 = forks[1][3].hash();

	assert!(pool.ready_at(f03).now_or_never().is_none());
	assert!(pool.ready_at(f13).now_or_never().is_none());
}

#[test]
fn fatp_ready_at_triggered_by_maintain() {
	// this scenario w/o xts is not likely to happen, but similar thing (xt changing from ready to
	// future) could occur e.g. when runtime was updated on fork1.
	sp_tracing::try_init_simple();
	let (api, forks) = test_chain_with_forks::chain(Some(&|_, _| false));
	let (pool, _) = pool_with_api(api.clone());

	let f03 = forks[0][3].hash();
	let f13 = forks[1][3].hash();

	assert!(pool.ready_at(f03).now_or_never().is_none());

	let event = new_best_block_event(&pool, None, f03);
	block_on(pool.maintain(event));

	assert!(pool.ready_at(f03).now_or_never().is_some());

	let xt0 = uxt(Alice, 203);
	let submissions = vec![pool.submit_one(invalid_hash(), SOURCE, xt0.clone())];
	block_on(futures::future::join_all(submissions));

	let event = new_best_block_event(&pool, Some(f03), f13);
	debug!(target: LOG_TARGET, ?event, "event");
	assert!(pool.ready_at(f13).now_or_never().is_none());
	block_on(pool.maintain(event));
	assert!(pool.ready_at(f03).now_or_never().is_some());
	assert!(pool.ready_at(f13).now_or_never().is_some());
}

#[test]
fn fatp_ready_at_triggered_by_maintain2() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let xt0 = uxt(Alice, 200);
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	// let (pool, api, _guard) = maintained_pool();
	// let header = api.push_block(1, vec![], true);
	//
	// let xt1 = uxt(Alice, 209);
	//
	// block_on(pool.submit_one(api.expect_hash_from_number(1), SOURCE, xt1.clone()))
	// 	.expect("1. Imported");

	let noop_waker = futures::task::noop_waker();
	let mut context = futures::task::Context::from_waker(&noop_waker);

	let mut ready_set_future = pool.ready_at(header01.hash());
	if ready_set_future.poll_unpin(&mut context).is_ready() {
		panic!("Ready set should not be ready before block update!");
	}

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));
	// block_on(pool.maintain(block_event(header)));

	match ready_set_future.poll_unpin(&mut context) {
		Poll::Pending => {
			panic!("Ready set should become ready after block update!");
		},
		Poll::Ready(iterator) => {
			let data = iterator.collect::<Vec<_>>();
			assert_eq!(data.len(), 1);
		},
	}
}

#[test]
fn fatp_linear_progress_finalization() {
	sp_tracing::try_init_simple();

	let (api, forks) = test_chain_with_forks::chain(None);
	let (pool, _) = pool_with_api(api.clone());

	let f00 = forks[0][0].hash();
	let f12 = forks[1][2].hash();
	let f14 = forks[1][4].hash();
	let f15 = forks[1][5].hash();

	let event = new_best_block_event(&pool, None, f00);
	block_on(pool.maintain(event));

	let xt0 = uxt(Bob, 205);
	let submissions = vec![pool.submit_one(invalid_hash(), SOURCE, xt0.clone())];
	block_on(futures::future::join_all(submissions));

	let event = new_best_block_event(&pool, Some(f00), f12);
	block_on(pool.maintain(event));
	assert_pool_status!(f12, &pool, 0, 1);
	assert_eq!(pool.active_views_count(), 1);

	debug!(target: LOG_TARGET, pool_status = ?pool.status_all(), "stats");

	block_on(pool.maintain(new_best_block_event(&pool, Some(f12), f15)));
	block_on(pool.maintain(finalized_block_event(&pool, f00, f14)));

	debug!(target: LOG_TARGET, pool_status = ?pool.status_all(), "stats");

	assert_eq!(pool.active_views_count(), 1);
	assert_pool_status!(f15, &pool, 1, 0);
}

#[test]
fn fatp_fork_finalization_removes_stale_views() {
	sp_tracing::try_init_simple();

	let (api, forks) = test_chain_with_forks::chain(None);
	let (pool, _) = pool_with_api(api.clone());

	let f00 = forks[0][0].hash();
	let f12 = forks[1][2].hash();
	let f14 = forks[1][4].hash();
	let f02 = forks[0][2].hash();
	let f03 = forks[0][3].hash();
	let f04 = forks[0][4].hash();

	let xt0 = uxt(Bob, 203);
	let submissions = vec![pool.submit_one(invalid_hash(), SOURCE, xt0.clone())];
	block_on(futures::future::join_all(submissions));

	let event = new_best_block_event(&pool, Some(f00), f12);
	block_on(pool.maintain(event));
	let event = new_best_block_event(&pool, Some(f00), f14);
	block_on(pool.maintain(event));
	let event = new_best_block_event(&pool, Some(f00), f02);
	block_on(pool.maintain(event));

	// only views at the tips of the forks are kept
	assert_eq!(pool.active_views_count(), 2);

	debug!(target: LOG_TARGET, pool_status = ?pool.status_all(), "stats");

	let event = ChainEvent::Finalized { hash: f03, tree_route: Arc::from(vec![]) };
	block_on(pool.maintain(event));
	debug!(target: LOG_TARGET, pool_status = ?pool.status_all(), "stats");
	// note: currently the pruning views only cleans views with block number less than finalized
	// block. views with higher number on other forks are not cleaned (will be done in next round).
	assert_eq!(pool.active_views_count(), 2);

	let event = ChainEvent::Finalized { hash: f04, tree_route: Arc::from(vec![]) };
	block_on(pool.maintain(event));
	assert_eq!(pool.active_views_count(), 1);
}

#[test]
fn fatp_watcher_future() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 202);
	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	assert_pool_status!(header01.hash(), &pool, 0, 1);

	let header02 = api.push_block_with_parent(header01.hash(), vec![], true);
	let header03 = api.push_block_with_parent(header02.hash(), vec![], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header01.hash()), header03.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header02.hash())));

	assert_pool_status!(header03.hash(), &pool, 0, 1);

	let xt0_events = block_on(xt0_watcher.take(1).collect::<Vec<_>>());
	assert_eq!(xt0_events, vec![TransactionStatus::Future]);
}

#[test]
fn fatp_watcher_ready() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	assert_pool_status!(header01.hash(), &pool, 1, 0);

	let header02 = api.push_block(2, vec![], true);
	let event = ChainEvent::Finalized {
		hash: header02.hash(),
		tree_route: Arc::from(vec![header01.hash()]),
	};
	block_on(pool.maintain(event));

	assert_pool_status!(header02.hash(), &pool, 1, 0);

	let xt0_events = block_on(xt0_watcher.take(1).collect::<Vec<_>>());
	assert_eq!(xt0_events, vec![TransactionStatus::Ready]);
}

#[test]
fn fatp_watcher_finalized() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	assert_pool_status!(header01.hash(), &pool, 1, 0);

	let header02 = api.push_block(2, vec![xt0], true);
	let event = ChainEvent::Finalized {
		hash: header02.hash(),
		tree_route: Arc::from(vec![header01.hash()]),
	};
	block_on(pool.maintain(event));

	assert_pool_status!(header02.hash(), &pool, 0, 0);

	let xt0_events = block_on(xt0_watcher.collect::<Vec<_>>());
	assert_eq!(
		xt0_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_in_block() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	assert_pool_status!(header01.hash(), &pool, 1, 0);

	let header02 = api.push_block(2, vec![xt0], true);

	let event = new_best_block_event(&pool, Some(header01.hash()), header02.hash());
	block_on(pool.maintain(event));
	let xt0_events = block_on(xt0_watcher.take(2).collect::<Vec<_>>());
	assert_eq!(
		xt0_events,
		vec![TransactionStatus::Ready, TransactionStatus::InBlock((header02.hash(), 0)),]
	);
}

#[test]
fn fatp_watcher_future_and_finalized() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone()),
		pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone()),
	];

	let mut submissions = block_on(futures::future::join_all(submissions));
	let xt1_watcher = submissions.remove(1).unwrap();
	let xt0_watcher = submissions.remove(0).unwrap();

	assert_pool_status!(header01.hash(), &pool, 1, 1);

	let header02 = api.push_block_with_parent(header01.hash(), vec![xt0], true);
	let header03 = api.push_block_with_parent(header02.hash(), vec![], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header01.hash()), header03.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header02.hash())));

	assert_pool_status!(header03.hash(), &pool, 0, 1);

	let xt1_status = block_on(xt1_watcher.take(1).collect::<Vec<_>>());
	assert_eq!(xt1_status, vec![TransactionStatus::Future]);
	let xt0_status = block_on(xt0_watcher.collect::<Vec<_>>());
	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_two_finalized_in_different_block() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Bob, 200);
	let xt3 = uxt(Dave, 200);

	let submissions = vec![
		pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone()),
		pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone()),
		pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone()),
	];
	let mut submissions = block_on(futures::future::join_all(submissions));
	let xt2_watcher = submissions.remove(2).unwrap();
	let xt1_watcher = submissions.remove(1).unwrap();
	let xt0_watcher = submissions.remove(0).unwrap();

	assert_pool_status!(header01.hash(), &pool, 3, 0);

	let header02 = api.push_block(2, vec![xt3.clone(), xt2.clone(), xt0.clone()], true);
	api.set_nonce(header02.hash(), Alice.into(), 201);
	// note: no maintain for block02 (!)

	let header03 = api.push_block(3, vec![xt1.clone()], true);
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header03.hash())));

	assert_pool_status!(header03.hash(), &pool, 0, 0);

	let xt1_status = futures::executor::block_on_stream(xt1_watcher).collect::<Vec<_>>();

	assert_eq!(
		xt1_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03.hash(), 0)),
			TransactionStatus::Finalized((header03.hash(), 0))
		]
	);

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 2)),
			TransactionStatus::Finalized((header02.hash(), 2))
		]
	);

	let xt2_status = futures::executor::block_on_stream(xt2_watcher).collect::<Vec<_>>();
	debug!(target: LOG_TARGET, ?xt2_status, "xt2_status");

	assert_eq!(
		xt2_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 1)),
			TransactionStatus::Finalized((header02.hash(), 1))
		]
	);
}

#[test]
fn fatp_no_view_pool_watcher_two_finalized_in_different_block() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);

	let header01 = api.push_block(1, vec![], true);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Bob, 200);
	let xt3 = uxt(Dave, 200);

	let submissions = vec![
		pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone()),
		pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone()),
		pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone()),
	];
	let mut submissions = block_on(futures::future::join_all(submissions));
	let xt2_watcher = submissions.remove(2).unwrap();
	let xt1_watcher = submissions.remove(1).unwrap();
	let xt0_watcher = submissions.remove(0).unwrap();

	let header02 = api.push_block(2, vec![xt3.clone(), xt2.clone(), xt0.clone()], true);
	api.set_nonce(header02.hash(), Alice.into(), 201);
	api.set_nonce(header02.hash(), Bob.into(), 201);
	api.set_nonce(header02.hash(), Dave.into(), 201);
	// note: no maintain for block02 (!)

	let header03 = api.push_block(3, vec![xt1.clone()], true);
	api.set_nonce(header03.hash(), Alice.into(), 202);
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header03.hash())));

	assert_pool_status!(header03.hash(), &pool, 0, 0);

	let xt1_status = futures::executor::block_on_stream(xt1_watcher).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt1_status, "xt1_status");

	assert_eq!(
		xt1_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03.hash(), 0)),
			TransactionStatus::Finalized((header03.hash(), 0))
		]
	);

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 2)),
			TransactionStatus::Finalized((header02.hash(), 2))
		]
	);

	let xt2_status = futures::executor::block_on_stream(xt2_watcher).collect::<Vec<_>>();
	debug!(target: LOG_TARGET, ?xt2_status, "xt2_status");

	assert_eq!(
		xt2_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 1)),
			TransactionStatus::Finalized((header02.hash(), 1))
		]
	);
}

#[test]
fn fatp_watcher_in_block_across_many_blocks() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Alice, 202);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	assert_pool_status!(header01.hash(), &pool, 2, 0);

	let header02 = api.push_block(2, vec![], true);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02.hash());
	block_on(pool.maintain(event));

	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	// note 1: transaction is not submitted to views that are not at the tip of the fork
	assert_eq!(pool.active_views_count(), 1);
	assert_eq!(pool.inactive_views_count(), 2); // gensis + 01
	assert_pool_status!(header02.hash(), &pool, 3, 0);

	let header03 = api.push_block(3, vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(header02.hash()), header03.hash());
	block_on(pool.maintain(event));

	assert_pool_status!(header03.hash(), &pool, 2, 0);

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).take(2).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");
	assert_eq!(
		xt0_status,
		vec![TransactionStatus::Ready, TransactionStatus::InBlock((header03.hash(), 0)),]
	);
}

#[test]
fn fatp_watcher_in_block_across_many_blocks2() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Alice, 202);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let xt1_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	assert_pool_status!(header01.hash(), &pool, 2, 0);

	let header02 = api.push_block(2, vec![], true);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02.hash());
	block_on(pool.maintain(event));

	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	// note 1: transaction is not submitted to views that are not at the tip of the fork
	assert_eq!(pool.active_views_count(), 1);
	assert_eq!(pool.inactive_views_count(), 2); // genesis + 01
	assert_pool_status!(header02.hash(), &pool, 3, 0);

	let header03 = api.push_block(3, vec![xt0.clone()], true);
	let header04 = api.push_block(4, vec![xt1.clone()], true);
	let event = new_best_block_event(&pool, Some(header02.hash()), header04.hash());
	block_on(pool.maintain(event));

	assert_pool_status!(header04.hash(), &pool, 1, 0);

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).take(2).collect::<Vec<_>>();
	let xt1_status = futures::executor::block_on_stream(xt1_watcher).take(2).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");
	debug!(target: LOG_TARGET, ?xt1_status, "xt1_status");
	assert_eq!(
		xt0_status,
		vec![TransactionStatus::Ready, TransactionStatus::InBlock((header03.hash(), 0)),]
	);
	assert_eq!(
		xt1_status,
		vec![TransactionStatus::Ready, TransactionStatus::InBlock((header04.hash(), 0)),]
	);
}

#[test]
fn fatp_watcher_dropping_listener_should_work() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);

	// intentionally drop the listener - nothing should panic.
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	assert_pool_status!(header01.hash(), &pool, 1, 0);

	let header02 = api.push_block(2, vec![], true);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02.hash());
	block_on(pool.maintain(event));
}

#[test]
fn fatp_watcher_fork_retract_and_finalize() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	assert_pool_status!(header01.hash(), &pool, 1, 0);

	let header02a = api.push_block_with_parent(header01.hash(), vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02a.hash(), &pool, 0, 0);

	let header02b = api.push_block_with_parent(header01.hash(), vec![xt0.clone()], true);
	let event = ChainEvent::Finalized {
		hash: header02b.hash(),
		tree_route: Arc::from(vec![header01.hash()]),
	};
	block_on(pool.maintain(event));
	assert_pool_status!(header02b.hash(), &pool, 0, 0);
	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02a.hash(), 0)),
			TransactionStatus::InBlock((header02b.hash(), 0)),
			TransactionStatus::Finalized((header02b.hash(), 0)),
		]
	);
}

#[test]
fn fatp_retract_all_forks() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);

	let header02a = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header02a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02a.hash(), &pool, 0, 0);

	let header02b = api.push_block_with_parent(genesis, vec![xt1.clone()], true);
	let event = new_best_block_event(&pool, Some(header02a.hash()), header02b.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02b.hash(), &pool, 1, 0);

	let header02c = api.push_block_with_parent(genesis, vec![], true);
	let event =
		ChainEvent::Finalized { hash: header02c.hash(), tree_route: Arc::from(vec![genesis]) };
	block_on(pool.maintain(event));
	assert_pool_status!(header02c.hash(), &pool, 2, 0);
}

#[test]
fn fatp_watcher_finalizing_forks() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);
	api.set_nonce(api.genesis_hash(), Eve.into(), 200);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);
	let xt3 = uxt(Dave, 200);
	let xt4 = uxt(Eve, 200);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let header01 = api.push_block(1, vec![xt0.clone()], true);
	block_on(pool.maintain(new_best_block_event(&pool, None, header01.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), header01.hash())));

	let xt1_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let header02a = api.push_block_with_parent(header01.hash(), vec![xt1.clone()], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header01.hash()), header02a.hash())));

	let xt2_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	let header03a = api.push_block_with_parent(header02a.hash(), vec![xt2.clone()], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header02a.hash()), header03a.hash())));

	let xt3_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt3.clone())).unwrap();
	let header02b = api.push_block_with_parent(header01.hash(), vec![xt3.clone()], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header01.hash()), header02b.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header02b.hash())));

	let xt4_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt4.clone())).unwrap();
	let header03b = api.push_block_with_parent(header02b.hash(), vec![xt4.clone()], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header02b.hash()), header03b.hash())));

	let header04b =
		api.push_block_with_parent(header03b.hash(), vec![xt1.clone(), xt2.clone()], true);
	block_on(pool.maintain(new_best_block_event(&pool, Some(header03b.hash()), header04b.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header02b.hash(), header04b.hash())));

	//=======================

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();
	let xt1_status = futures::executor::block_on_stream(xt1_watcher).collect::<Vec<_>>();
	let xt2_status = futures::executor::block_on_stream(xt2_watcher).collect::<Vec<_>>();
	let xt3_status = futures::executor::block_on_stream(xt3_watcher).collect::<Vec<_>>();
	let xt4_status = futures::executor::block_on_stream(xt4_watcher).collect::<Vec<_>>();

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header01.hash(), 0)),
			TransactionStatus::Finalized((header01.hash(), 0)),
		]
	);

	assert_eq!(
		xt1_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02a.hash(), 0)),
			TransactionStatus::InBlock((header04b.hash(), 0)),
			TransactionStatus::Finalized((header04b.hash(), 0)),
		]
	);
	assert_eq!(
		xt2_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03a.hash(), 0)),
			TransactionStatus::InBlock((header04b.hash(), 1)),
			TransactionStatus::Finalized((header04b.hash(), 1)),
		]
	);
	assert_eq!(
		xt3_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02b.hash(), 0)),
			TransactionStatus::Finalized((header02b.hash(), 0)),
		]
	);
	assert_eq!(
		xt4_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03b.hash(), 0)),
			TransactionStatus::Finalized((header03b.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_best_block_after_finalized() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	let header01 = api.push_block(1, vec![], true);
	let event = finalized_block_event(&pool, api.genesis_hash(), header01.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	// todo: shall we submit to finalized views? (if it is at the tip of the fork then yes?)
	// assert_pool_status!(header01.hash(), &pool, 1, 0);

	let header02 = api.push_block(2, vec![xt0.clone()], true);

	let event = finalized_block_event(&pool, header01.hash(), header02.hash());
	block_on(pool.maintain(event));
	let event = new_best_block_event(&pool, Some(header01.hash()), header02.hash());
	block_on(pool.maintain(event));

	let xt0_events = block_on(xt0_watcher.collect::<Vec<_>>());
	assert_eq!(
		xt0_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_best_block_after_finalized2() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt0 = uxt(Alice, 200);
	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	let header01 = api.push_block(1, vec![xt0.clone()], true);

	let event = finalized_block_event(&pool, api.genesis_hash(), header01.hash());
	block_on(pool.maintain(event));
	let event = new_best_block_event(&pool, Some(api.genesis_hash()), header01.hash());
	block_on(pool.maintain(event));

	let xt0_events = block_on(xt0_watcher.collect::<Vec<_>>());
	assert_eq!(
		xt0_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header01.hash(), 0)),
			TransactionStatus::Finalized((header01.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_switching_fork_multiple_times_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	api.set_nonce(api.genesis_hash(), Bob.into(), 200);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let header01a = api.push_block(1, vec![xt0.clone()], true);

	let xt1_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let header01b = api.push_block(1, vec![xt0.clone(), xt1.clone()], true);

	// note: finalized block here must be header01b.
	// It is because of how the order in which MultiViewListener is processing tx events and view
	// events. tx events from single view are processed first, then view commands are handled. If
	// finalization happens in first view reported then no events from others views will be
	// processed.

	block_on(pool.maintain(new_best_block_event(&pool, None, header01a.hash())));
	block_on(pool.maintain(new_best_block_event(&pool, Some(header01a.hash()), header01b.hash())));
	block_on(pool.maintain(new_best_block_event(&pool, Some(header01b.hash()), header01a.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), header01b.hash())));

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();
	let xt1_status = futures::executor::block_on_stream(xt1_watcher).take(2).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");
	debug!(target: LOG_TARGET, ?xt1_status, "xt1_status");

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header01a.hash(), 0)),
			TransactionStatus::InBlock((header01b.hash(), 0)),
			TransactionStatus::Finalized((header01b.hash(), 0)),
		]
	);

	assert_eq!(
		xt1_status,
		vec![TransactionStatus::Ready, TransactionStatus::InBlock((header01b.hash(), 1)),]
	);
}

#[test]
fn fatp_watcher_two_blocks_delayed_finalization_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);

	let header01 = api.push_block(1, vec![], true);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let header02 = api.push_block_with_parent(header01.hash(), vec![xt0.clone()], true);

	let xt1_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let header03 = api.push_block_with_parent(header02.hash(), vec![xt1.clone()], true);

	let xt2_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	let header04 = api.push_block_with_parent(header03.hash(), vec![xt2.clone()], true);

	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), header01.hash())));
	block_on(pool.maintain(new_best_block_event(&pool, None, header04.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header03.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header03.hash(), header04.hash())));

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();
	let xt1_status = futures::executor::block_on_stream(xt1_watcher).collect::<Vec<_>>();
	let xt2_status = futures::executor::block_on_stream(xt2_watcher).collect::<Vec<_>>();

	// todo: double events.
	// view for header04 reported InBlock for all xts.
	// Then finalization comes for header03. We need to create a view to sent finalization events.
	// But in_block are also sent because of pruning - normal process during view creation.
	//
	// Do not know what solution should be in this case?
	// - just jeep two events,
	// - block pruning somehow (seems like excessive additional logic not really needed)
	// - build view from recent best block? (retracting instead of enacting?)
	// - de-dup events in listener (implemented)

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0)),
		]
	);
	assert_eq!(
		xt1_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03.hash(), 0)),
			TransactionStatus::Finalized((header03.hash(), 0)),
		]
	);
	assert_eq!(
		xt2_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header04.hash(), 0)),
			TransactionStatus::Finalized((header04.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_delayed_finalization_does_not_retract() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);

	let header01 = api.push_block(1, vec![], true);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let header02 = api.push_block_with_parent(header01.hash(), vec![xt0.clone()], true);

	let xt1_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let header03 = api.push_block_with_parent(header02.hash(), vec![xt1.clone()], true);

	block_on(pool.maintain(new_best_block_event(&pool, None, header02.hash())));
	block_on(pool.maintain(new_best_block_event(&pool, Some(header02.hash()), header03.hash())));

	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), header02.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header02.hash(), header03.hash())));

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();
	let xt1_status = futures::executor::block_on_stream(xt1_watcher).collect::<Vec<_>>();

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0)),
		]
	);
	assert_eq!(
		xt1_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03.hash(), 0)),
			TransactionStatus::Finalized((header03.hash(), 0)),
		]
	);
}

#[test]
fn fatp_watcher_best_block_after_finalization_does_not_retract() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);

	let header01 = api.push_block(1, vec![], true);

	let xt0_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();
	let header02 = api.push_block_with_parent(header01.hash(), vec![xt0.clone()], true);

	let xt1_watcher = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let header03 = api.push_block_with_parent(header02.hash(), vec![xt1.clone()], true);

	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), header01.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header03.hash())));
	block_on(pool.maintain(new_best_block_event(&pool, Some(api.genesis_hash()), header02.hash())));

	let xt0_status = futures::executor::block_on_stream(xt0_watcher).collect::<Vec<_>>();
	let xt1_status = futures::executor::block_on_stream(xt1_watcher).collect::<Vec<_>>();

	debug!(target: LOG_TARGET, ?xt0_status, "xt0_status");
	debug!(target: LOG_TARGET, ?xt1_status, "xt1_status");

	assert_eq!(
		xt0_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0)),
		]
	);
	assert_eq!(
		xt1_status,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header03.hash(), 0)),
			TransactionStatus::Finalized((header03.hash(), 0)),
		]
	);
}

#[test]
fn fatp_transactions_purging_stale_on_finalization_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt1 = uxt(Alice, 200);
	let xt2 = uxt(Alice, 201);
	let xt3 = uxt(Alice, 202);

	let header01 = api.push_block(1, vec![], true);
	block_on(pool.maintain(new_best_block_event(&pool, None, header01.hash())));

	let watcher1 = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let watcher2 = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt3.clone())).unwrap();

	assert_eq!(api.validation_requests().len(), 3);
	assert_eq!(pool.status_all()[&header01.hash()].ready, 3);
	assert_eq!(block_on(pool.mempool_len()), (1, 2));

	let header02 = api.push_block(2, vec![xt1.clone(), xt2.clone(), xt3.clone()], true);
	api.set_nonce(header02.hash(), Alice.into(), 203);
	block_on(pool.maintain(finalized_block_event(&pool, header01.hash(), header02.hash())));

	assert_eq!(pool.status_all()[&header02.hash()].ready, 0);
	assert_eq!(block_on(pool.mempool_len()), (0, 0));

	let xt1_events = futures::executor::block_on_stream(watcher1).collect::<Vec<_>>();
	let xt2_events = futures::executor::block_on_stream(watcher2).collect::<Vec<_>>();
	assert_eq!(
		xt1_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 0)),
			TransactionStatus::Finalized((header02.hash(), 0))
		],
	);
	assert_eq!(
		xt2_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header02.hash(), 1)),
			TransactionStatus::Finalized((header02.hash(), 1))
		],
	);
}

#[test]
fn import_sink_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let genesis = api.genesis_hash();
	let header01a = api.push_block(1, vec![], true);
	let header01b = api.push_block(1, vec![], true);

	let import_stream = pool.import_notification_stream();

	let event = new_best_block_event(&pool, None, header01a.hash());
	block_on(pool.maintain(event));

	let event = new_best_block_event(&pool, None, header01b.hash());
	block_on(pool.maintain(event));

	api.set_nonce(header01b.hash(), Alice.into(), 202);

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_one(genesis, SOURCE, xt0.clone()),
		pool.submit_one(genesis, SOURCE, xt1.clone()),
	];

	block_on(futures::future::join_all(submissions));

	assert_pool_status!(header01a.hash(), &pool, 1, 1);
	assert_pool_status!(header01b.hash(), &pool, 1, 0);

	let import_events =
		futures::executor::block_on_stream(import_stream).take(2).collect::<Vec<_>>();

	let expected_import_events = vec![api.hash_and_length(&xt0).0, api.hash_and_length(&xt1).0];
	assert!(import_events.iter().all(|v| expected_import_events.contains(v)));
}

#[test]
fn import_sink_works2() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let genesis = api.genesis_hash();
	let header01a = api.push_block(1, vec![], true);
	let header01b = api.push_block(1, vec![], true);

	let import_stream = pool.import_notification_stream();

	let event = new_best_block_event(&pool, None, header01a.hash());
	block_on(pool.maintain(event));

	let event = new_best_block_event(&pool, None, header01b.hash());
	block_on(pool.maintain(event));

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_one(genesis, SOURCE, xt0.clone()),
		pool.submit_one(genesis, SOURCE, xt1.clone()),
	];

	block_on(futures::future::join_all(submissions));

	assert_pool_status!(header01a.hash(), &pool, 1, 1);
	assert_pool_status!(header01b.hash(), &pool, 1, 1);

	let import_events =
		futures::executor::block_on_stream(import_stream).take(1).collect::<Vec<_>>();

	let expected_import_events = vec![api.hash_and_length(&xt0).0];
	assert_eq!(import_events, expected_import_events);
}

#[test]
fn import_sink_works3() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let import_stream = pool.import_notification_stream();
	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 202);

	let submissions = vec![
		pool.submit_one(genesis, SOURCE, xt0.clone()),
		pool.submit_one(genesis, SOURCE, xt1.clone()),
	];

	block_on(futures::future::join_all(submissions));

	let header01a = api.push_block(1, vec![], true);
	let header01b = api.push_block(1, vec![], true);

	let event = new_best_block_event(&pool, None, header01a.hash());
	block_on(pool.maintain(event));

	let event = new_best_block_event(&pool, None, header01b.hash());
	block_on(pool.maintain(event));

	assert_pool_status!(header01a.hash(), &pool, 1, 1);
	assert_pool_status!(header01b.hash(), &pool, 1, 1);

	let import_events =
		futures::executor::block_on_stream(import_stream).take(1).collect::<Vec<_>>();

	let expected_import_events = vec![api.hash_and_length(&xt0).0];
	assert_eq!(import_events, expected_import_events);
}

#[test]
fn fatp_avoid_stuck_transaction() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Alice, 202);
	let xt3 = uxt(Alice, 203);
	let xt4 = uxt(Alice, 204);
	let xt4i = uxt(Alice, 204);
	let xt4i_watcher =
		block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt4i.clone())).unwrap();

	assert_eq!(block_on(pool.mempool_len()), (0, 1));

	let header01 = api.push_block(1, vec![xt0], true);
	api.set_nonce(header01.hash(), Alice.into(), 201);
	let header02 = api.push_block(2, vec![xt1], true);
	api.set_nonce(header02.hash(), Alice.into(), 202);
	let header03 = api.push_block(3, vec![xt2], true);
	api.set_nonce(header03.hash(), Alice.into(), 203);

	let header04 = api.push_block(4, vec![], true);
	api.set_nonce(header04.hash(), Alice.into(), 203);

	let header05 = api.push_block(5, vec![], true);
	api.set_nonce(header05.hash(), Alice.into(), 203);

	let event = new_best_block_event(&pool, None, header05.hash());
	block_on(pool.maintain(event));

	let event = finalized_block_event(&pool, api.genesis_hash(), header03.hash());
	block_on(pool.maintain(event));

	assert_pool_status!(header05.hash(), &pool, 0, 1);

	let header06 = api.push_block(6, vec![xt3, xt4], true);
	api.set_nonce(header06.hash(), Alice.into(), 205);
	let event = new_best_block_event(&pool, None, header06.hash());
	block_on(pool.maintain(event));

	assert_pool_status!(header06.hash(), &pool, 0, 0);

	let header07 = api.push_block(7, vec![], true);
	let event = finalized_block_event(&pool, header03.hash(), header07.hash());
	block_on(pool.maintain(event));

	let xt4i_events = futures::executor::block_on_stream(xt4i_watcher).collect::<Vec<_>>();
	debug!(target: LOG_TARGET, ?xt4i_events, "xt4i_events");
	assert_eq!(xt4i_events, vec![TransactionStatus::Future, TransactionStatus::Dropped]);
	assert_eq!(block_on(pool.mempool_len()), (0, 0));
}

#[test]
fn fatp_future_is_pruned_by_conflicting_tags() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Alice, 202);
	let xt2i = uxt(Alice, 202);
	debug!(target: LOG_TARGET, xt0 = ?api.hash_and_length(&xt0).0, "xt0");
	debug!(target: LOG_TARGET, xt1 = ?api.hash_and_length(&xt1).0, "xt1");
	debug!(target: LOG_TARGET, xt2 = ?api.hash_and_length(&xt2).0, "xt2");
	debug!(target: LOG_TARGET, xt2i = ?api.hash_and_length(&xt2i).0, "xt2i");
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2i.clone())).unwrap();

	assert_eq!(block_on(pool.mempool_len()), (0, 1));

	let header01 = api.push_block(1, vec![], true);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01.hash(), &pool, 0, 1);

	let header02 = api.push_block(2, vec![xt0, xt1, xt2], true);
	api.set_nonce(header02.hash(), Alice.into(), 203);

	let event = new_best_block_event(&pool, None, header02.hash());
	block_on(pool.maintain(event));

	assert_pool_status!(header02.hash(), &pool, 0, 0);
}

#[test]
fn fatp_prune_based_on_inactive_views_tags() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Alice, 202);
	let xt3 = uxt(Alice, 203);
	let xt4 = uxt(Alice, 204);
	let xt5 = uxt(Alice, 205);
	let xt6 = uxt(Alice, 206);
	let xt7 = uxt(Alice, 207);
	let xt8 = uxt(Alice, 208);
	let xt9 = uxt(Alice, 209);
	let xt10 = uxt(Alice, 210);
	let xt11 = uxt(Alice, 211);

	// Push an empty common block.
	let header01 = api.push_block_with_parent(api.genesis_hash(), vec![], true);
	let event = new_best_block_event(&pool, Some(api.genesis_hash()), header01.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01.hash(), &pool, 0, 0);
	assert_eq!(api.validation_requests().len(), 0);

	// Submit two txs to the txpool.
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	assert_pool_status!(header01.hash(), &pool, 0, 2);
	assert_eq!(api.validation_requests().len(), 2);

	// Push the first retracted fork block, with the ready tx.
	let header02a = api.push_block_with_parent(header01.hash(), vec![xt0.clone()], true);
	api.set_nonce(header02a.hash(), Alice.into(), 200);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02a.hash(), &pool, 2, 0);
	assert_eq!(api.validation_requests().len(), 5);

	// Submit a second ready tx.
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt3.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt4.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt5.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt11.clone())).unwrap();
	assert_pool_status!(header02a.hash(), &pool, 5, 1);
	assert_eq!(api.validation_requests().len(), 9);

	// Push the second retracted fork block, containing xt1.
	let header03a = api.push_block_with_parent(header02a.hash(), vec![xt1.clone()], true);
	api.set_nonce(header03a.hash(), Alice.into(), 201);
	let event = new_best_block_event(&pool, Some(header02a.hash()), header03a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header03a.hash(), &pool, 4, 1);
	assert_eq!(api.validation_requests().len(), 13);

	// Submit another batch of future txs.
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt9.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt8.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt7.clone())).unwrap();
	assert_pool_status!(header03a.hash(), &pool, 4, 4);
	assert_eq!(api.validation_requests().len(), 16);

	// Push the third retracted fork block, containing xt2.
	let header04a = api.push_block_with_parent(header03a.hash(), vec![xt2.clone()], true);
	api.set_nonce(header04a.hash(), Alice.into(), 202);
	let event = new_best_block_event(&pool, Some(header03a.hash()), header04a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header04a.hash(), &pool, 3, 4);
	assert_eq!(api.validation_requests().len(), 19);

	let header02b = api.push_block_with_parent(
		header01.hash(),
		vec![xt0.clone(), xt1.clone(), xt2.clone()],
		true,
	);
	api.set_nonce(header02b.hash(), Alice.into(), 202);

	let header03b = api.push_block_with_parent(
		header02b.hash(),
		vec![xt3.clone(), xt4.clone(), xt5.clone()],
		true,
	);
	api.set_nonce(header03b.hash(), Alice.into(), 205);

	let header04b = api.push_block_with_parent(
		header03b.hash(),
		vec![xt6.clone(), xt7.clone(), xt8.clone()],
		true,
	);
	api.set_nonce(header04b.hash(), Alice.into(), 208);

	let header05b = api.push_block_with_parent(
		header04b.hash(),
		vec![xt9.clone(), xt10.clone(), xt11.clone()],
		true,
	);
	api.set_nonce(header05b.hash(), Alice.into(), 211);

	// Notify a whole new fork.
	let event = new_best_block_event(&pool, Some(header04b.hash()), header05b.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header05b.hash(), &pool, 0, 0);
	// There are extra 3 validations for the enacted fork:
	// 1. xt0 - which is the tx missing from the views, so makes sense to validate it.
	// 2. xt6 - not submitted to the retracted fork before, so makes sense to validate it.
	// 3. xt10 - same as for xt6.
	// The rest of the txs which are not validated anymore to get their 'provides' tags are x1 & x2,
	// which can be found in the inactive views.
	assert_eq!(api.validation_requests().len(), 22);
}

#[test]
fn fatp_dangling_ready_gets_revalidated() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt2 = uxt(Alice, 202);
	debug!(target: LOG_TARGET, xt2 = ?api.hash_and_length(&xt2).0, "xt2");

	let header01 = api.push_block(1, vec![], true);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01.hash(), &pool, 0, 0);

	let header02a = api.push_block_with_parent(header01.hash(), vec![], true);
	api.set_nonce(header02a.hash(), Alice.into(), 202);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02a.hash());
	block_on(pool.maintain(event));

	// send xt2 - it will become ready on block 02a.
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	assert_pool_status!(header02a.hash(), &pool, 1, 0);
	assert_eq!(block_on(pool.mempool_len()), (0, 1));

	// xt2 is still ready: view was just cloned (revalidation executed in background)
	let header02b = api.push_block_with_parent(header01.hash(), vec![], true);
	let event = new_best_block_event(&pool, Some(header02a.hash()), header02b.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02b.hash(), &pool, 1, 0);

	// xt2 is now future - view revalidation worked.
	let header03b = api.push_block_with_parent(header02b.hash(), vec![], true);
	let event = new_best_block_event(&pool, Some(header02b.hash()), header03b.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header03b.hash(), &pool, 0, 1);
}

#[test]
fn fatp_ready_txs_are_provided_in_valid_order() {
	// this test checks if recently_pruned tags are cleared for views cloned from retracted path
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Alice, 201);
	let xt2 = uxt(Alice, 202);
	debug!(target: LOG_TARGET, xt0 = ?api.hash_and_length(&xt0).0, "xt0");
	debug!(target: LOG_TARGET, xt1 = ?api.hash_and_length(&xt1).0, "xt1");
	debug!(target: LOG_TARGET, xt2 = ?api.hash_and_length(&xt2).0, "xt2");

	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt2.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt1.clone())).unwrap();
	let _ = block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt0.clone())).unwrap();

	let header01 = api.push_block(1, vec![xt0], true);
	api.set_nonce(header01.hash(), Alice.into(), 201);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01.hash(), &pool, 2, 0);

	let header02a =
		api.push_block_with_parent(header01.hash(), vec![xt1.clone(), xt2.clone()], true);
	api.set_nonce(header02a.hash(), Alice.into(), 203);
	let event = new_best_block_event(&pool, Some(header01.hash()), header02a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02a.hash(), &pool, 0, 0);

	let header02b = api.push_block_with_parent(header01.hash(), vec![], true);
	api.set_nonce(header02b.hash(), Alice.into(), 201);
	let event = new_best_block_event(&pool, Some(header02a.hash()), header02b.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header02b.hash(), &pool, 2, 0);
	assert_ready_iterator!(header02b.hash(), pool, [xt1, xt2]);
}

// todo: add test: check len of filter after finalization (!)
// todo: broadcasted test?

#[test]
fn fatp_ready_light_empty_on_unmaintained_fork() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);

	let header01a = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header01a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01a.hash(), &pool, 0, 0);

	let header01b = api.push_block_with_parent(genesis, vec![xt1.clone()], true);

	let mut ready_iterator = pool.ready_at_light(header01b.hash()).now_or_never().unwrap();
	assert!(ready_iterator.next().is_none());
}

#[test]
fn fatp_ready_light_misc_scenarios_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);

	// fork A
	let header01a = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header01a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01a.hash(), &pool, 0, 0);

	// fork B
	let header01b = api.push_block_with_parent(genesis, vec![xt1.clone()], true);
	let event = new_best_block_event(&pool, Some(header01a.hash()), header01b.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01b.hash(), &pool, 1, 0);

	// new block at fork B
	let header02b = api.push_block_with_parent(header01b.hash(), vec![xt1.clone()], true);

	// test 1:
	// ready light returns just txs from view @header01b (which contains retracted xt0)
	let mut ready_iterator = pool.ready_at_light(header02b.hash()).now_or_never().unwrap();
	let ready01 = ready_iterator.next();
	assert_eq!(ready01.unwrap().hash, api.hash_and_length(&xt0).0);
	assert!(ready_iterator.next().is_none());

	// test 2:
	// submit new transaction to all views
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt2.clone())).unwrap();

	// new block at fork A, not yet notified to pool
	let header02a = api.push_block_with_parent(header01a.hash(), vec![], true);

	// ready light returns just txs from view @header01a (which contains newly submitted xt2)
	let mut ready_iterator = pool.ready_at_light(header02a.hash()).now_or_never().unwrap();
	let ready01 = ready_iterator.next();
	assert_eq!(ready01.unwrap().hash, api.hash_and_length(&xt2).0);
	assert!(ready_iterator.next().is_none());

	// test 3:
	let mut ready_iterator = pool.ready_at_light(header02b.hash()).now_or_never().unwrap();
	let ready01 = ready_iterator.next();
	assert_eq!(ready01.unwrap().hash, api.hash_and_length(&xt0).0);
	let ready02 = ready_iterator.next();
	assert_eq!(ready02.unwrap().hash, api.hash_and_length(&xt2).0);
	assert!(ready_iterator.next().is_none());

	// test 4:
	// new block at fork B, not yet notified to pool
	let header03b =
		api.push_block_with_parent(header02b.hash(), vec![xt0.clone(), xt2.clone()], true);
	// ready light @header03b will be empty: as new block contains xt0/xt2
	let mut ready_iterator = pool.ready_at_light(header03b.hash()).now_or_never().unwrap();
	assert!(ready_iterator.next().is_none());
}

#[test]
fn fatp_ready_light_long_fork_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);
	api.set_nonce(api.genesis_hash(), Eve.into(), 200);

	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);
	let xt3 = uxt(Dave, 200);
	let xt4 = uxt(Eve, 200);

	let submissions = vec![pool.submit_at(
		genesis,
		SOURCE,
		vec![xt0.clone(), xt1.clone(), xt2.clone(), xt3.clone(), xt4.clone()],
	)];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(Result::is_ok));

	let header01 = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header01.hash());
	block_on(pool.maintain(event));

	let header02 = api.push_block_with_parent(header01.hash(), vec![xt1.clone()], true);
	let header03 = api.push_block_with_parent(header02.hash(), vec![xt2.clone()], true);
	let header04 = api.push_block_with_parent(header03.hash(), vec![xt3.clone()], true);

	let mut ready_iterator = pool.ready_at_light(header04.hash()).now_or_never().unwrap();
	let ready01 = ready_iterator.next();
	assert_eq!(ready01.unwrap().hash, api.hash_and_length(&xt4).0);
	assert!(ready_iterator.next().is_none());
}

#[test]
fn fatp_ready_light_most_recent_view_long_fork_retracted_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);
	api.set_nonce(api.genesis_hash(), Eve.into(), 200);

	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);
	let xt3 = uxt(Dave, 200);
	let xt4 = uxt(Eve, 200);

	let submissions = vec![pool.submit_at(
		genesis,
		SOURCE,
		vec![xt0.clone(), xt1.clone(), xt2.clone(), xt3.clone()],
	)];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(|r| { r.is_ok() }));

	// dirty hack to remove genesis view, so we can check fallback to most-recent-view at header03b.
	let header01a = api.push_block_with_parent(genesis, vec![], true);
	let event = finalized_block_event(&pool, genesis, header01a.hash());
	block_on(pool.maintain(event));

	let header02a = api.push_block_with_parent(header01a.hash(), vec![xt4.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header02a.hash());
	block_on(pool.maintain(event));

	let header01b = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let header02b = api.push_block_with_parent(header01b.hash(), vec![xt1.clone()], true);
	let header03b = api.push_block_with_parent(header02b.hash(), vec![xt2.clone()], true);

	// Returns the most recent view (`header02a`) ready transactions set.
	let ready_iterator = pool.ready_at_light(header03b.hash()).now_or_never().unwrap();
	assert_eq!(ready_iterator.count(), 4);
}

#[test]
fn fatp_ready_light_long_fork_retracted_works() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);
	api.set_nonce(api.genesis_hash(), Eve.into(), 200);

	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);
	let xt3 = uxt(Dave, 200);
	let xt4 = uxt(Eve, 200);

	let submissions = vec![pool.submit_at(
		genesis,
		SOURCE,
		vec![xt0.clone(), xt1.clone(), xt2.clone(), xt3.clone()],
	)];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(|r| { r.is_ok() }));

	let header01a = api.push_block_with_parent(genesis, vec![xt4.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header01a.hash());
	block_on(pool.maintain(event));

	let header01b = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let header02b = api.push_block_with_parent(header01b.hash(), vec![xt1.clone()], true);
	let header03b = api.push_block_with_parent(header02b.hash(), vec![xt2.clone()], true);

	// Returns the genesis view ready transactions set with fork txs removed.
	let ready_iterator = pool.ready_at_light(header03b.hash()).now_or_never().unwrap();
	assert_eq!(ready_iterator.count(), 1);

	let event = new_best_block_event(&pool, Some(header01a.hash()), header01b.hash());
	block_on(pool.maintain(event));

	let mut ready_iterator = pool.ready_at_light(header03b.hash()).now_or_never().unwrap();
	let ready01 = ready_iterator.next();
	assert_eq!(ready01.unwrap().hash, api.hash_and_length(&xt3).0);
	let ready02 = ready_iterator.next();
	assert_eq!(ready02.unwrap().hash, api.hash_and_length(&xt4).0);
	assert!(ready_iterator.next().is_none());
}

#[test]
fn fatp_ready_light_fallback_for_most_recent_view_gets_triggered() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);
	api.set_nonce(api.genesis_hash(), Eve.into(), 200);

	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);
	let xt3 = uxt(Dave, 200);
	let xt4 = uxt(Eve, 200);

	let submissions = vec![pool.submit_at(genesis, SOURCE, vec![xt0.clone(), xt1.clone()])];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(|r| { r.is_ok() }));

	// dirty hack to remove genesis view, so we can check fallback to most-recent-view at header01b.
	let header01a = api.push_block_with_parent(genesis, vec![], true);
	let event = finalized_block_event(&pool, genesis, header01a.hash());
	block_on(pool.maintain(event));

	let header02a = api.push_block_with_parent(header01a.hash(), vec![xt4.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header02a.hash());
	block_on(pool.maintain(event));

	let header01b = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	// Call `ready_at_light` at genesis direct descendent, even if not notified as best or
	// finalized. Should still return ready txs based on the most recent view processed by the
	// txpool.
	let ready_iterator = pool.ready_at_light(header01b.hash()).now_or_never().unwrap();
	assert_eq!(ready_iterator.count(), 2);

	let header02b = api.push_block_with_parent(header01b.hash(), vec![xt1.clone()], true);
	let header03b = api.push_block_with_parent(header02b.hash(), vec![xt2.clone()], true);

	// Submit a few more tx to the pool.
	let submissions = vec![pool.submit_at(
		// `at` is ignored.
		genesis,
		SOURCE,
		vec![xt2.clone(), xt3.clone()],
	)];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(|r| { r.is_ok() }));

	// Calling `ready_at_light` now on the last block of a fork, with no block notified as best.
	// We should still get the ready txs from the most recent view processed by the txpool,
	// but now with a few more txs which were submitted previously.
	assert_ready_at_light_iterator!(header03b.hash(), pool, [xt0, xt1, xt2, xt3]);
}

#[test]
fn fatp_ready_light_fallback_gets_triggered() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	api.set_nonce(api.genesis_hash(), Dave.into(), 200);
	api.set_nonce(api.genesis_hash(), Eve.into(), 200);

	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);
	let xt2 = uxt(Charlie, 200);
	let xt3 = uxt(Dave, 200);
	let xt4 = uxt(Eve, 200);

	let submissions = vec![pool.submit_at(genesis, SOURCE, vec![xt0.clone(), xt1.clone()])];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(|r| { r.is_ok() }));

	let header01a = api.push_block_with_parent(genesis, vec![xt4.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header01a.hash());
	block_on(pool.maintain(event));

	let header01b = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	// Call `ready_at_light` at genesis direct descendent, even if not notified as best or
	// finalized. Should still return ready txs based on the view for genesis, taking into account
	// transactions from 01b block.
	let ready_iterator = pool.ready_at_light(header01b.hash()).now_or_never().unwrap();
	assert_eq!(ready_iterator.count(), 1);

	let header02b = api.push_block_with_parent(header01b.hash(), vec![xt1.clone()], true);
	let header03b = api.push_block_with_parent(header02b.hash(), vec![xt2.clone()], true);

	// Submit a few more tx to the pool.
	let submissions = vec![pool.submit_at(
		// `at` is ignored.
		genesis,
		SOURCE,
		vec![xt2.clone(), xt3.clone()],
	)];
	let results = block_on(futures::future::join_all(submissions));
	assert!(results.iter().all(|r| { r.is_ok() }));

	let event = finalized_block_event(&pool, header01a.hash(), header01b.hash());
	block_on(pool.maintain(event));

	// Calling `ready_at_light` on the new block (`header03b`) should consider its fork up to
	// the finalized block for the search of the best view, and coincidentaly, that's the only view
	// of the tree route, being the view created for NBB `header01b`. The returned ready txs are the
	// ones left in the best view's pool after prunning the txs.
	assert_ready_at_light_iterator!(header03b.hash(), pool, [xt3, xt4]);
}

#[test]
fn fatp_ready_at_with_timeout_works_for_misc_scenarios() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	api.set_nonce(api.genesis_hash(), Bob.into(), 200);
	api.set_nonce(api.genesis_hash(), Charlie.into(), 200);
	let genesis = api.genesis_hash();

	let xt0 = uxt(Alice, 200);
	let xt1 = uxt(Bob, 200);

	let header01a = api.push_block_with_parent(genesis, vec![xt0.clone()], true);
	let event = new_best_block_event(&pool, Some(genesis), header01a.hash());
	block_on(pool.maintain(event));
	assert_pool_status!(header01a.hash(), &pool, 0, 0);

	let header01b = api.push_block_with_parent(genesis, vec![xt1.clone()], true);

	let mut ready_at_future =
		pool.ready_at_with_timeout(header01b.hash(), Duration::from_secs(36000));

	let noop_waker = futures::task::noop_waker();
	let mut context = futures::task::Context::from_waker(&noop_waker);

	if ready_at_future.poll_unpin(&mut context).is_ready() {
		panic!("Ready set should not be ready before maintenance on block update!");
	}

	let event = new_best_block_event(&pool, Some(header01a.hash()), header01b.hash());
	block_on(pool.maintain(event));

	// ready should now be triggered:
	let mut ready_at = ready_at_future.now_or_never().unwrap();
	assert_eq!(ready_at.next().unwrap().hash, api.hash_and_length(&xt0).0);
	assert!(ready_at.next().is_none());

	let header02a = api.push_block_with_parent(header01a.hash(), vec![], true);
	let xt2 = uxt(Charlie, 200);
	block_on(pool.submit_one(invalid_hash(), SOURCE, xt2.clone())).unwrap();

	// ready light should now be triggered:
	let mut ready_at2 = block_on(pool.ready_at_with_timeout(header02a.hash(), Duration::ZERO));
	assert_eq!(ready_at2.next().unwrap().hash, api.hash_and_length(&xt2).0);
	assert!(ready_at2.next().is_none());
}

#[test]
fn fatp_tx_is_not_prematurely_revalidated() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	let genesis = api.genesis_hash();

	let mut hashes = vec![];
	let mut prev_hash = genesis;
	hashes.push(genesis);
	for n in 1..=40 {
		let header = api.push_block_with_parent(prev_hash, vec![], true);
		if n <= 21 {
			api.set_nonce(header.hash(), Alice.into(), 199);
			block_on(pool.maintain(new_best_block_event(&pool, Some(prev_hash), header.hash())));
		} else {
			// not realistic, we only want tx to be invalid, stale is currently the only way in
			// TestApi
			api.set_nonce(header.hash(), Alice.into(), 199);
		}
		hashes.push(header.hash());
		prev_hash = header.hash();
	}

	let xt0 = uxt(Alice, 199);

	// note: tx is validated at block 20 (recent best block):
	let xt0_watcher = block_on(pool.submit_and_watch(hashes[21], SOURCE, xt0.clone())).unwrap();

	let header41 = api.push_block_with_parent(hashes[40], vec![xt0.clone()], true);

	// note: tx is still valid at block 21
	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), hashes[5])));
	block_on(pool.maintain(finalized_block_event(&pool, hashes[5], hashes[10])));
	block_on(pool.maintain(finalized_block_event(&pool, hashes[10], hashes[19])));
	block_on(pool.maintain(finalized_block_event(&pool, hashes[19], header41.hash())));

	let xt0_events = block_on(xt0_watcher.collect::<Vec<_>>());
	assert_eq!(
		xt0_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((header41.hash(), 0)),
			TransactionStatus::Finalized((header41.hash(), 0)),
		]
	);
}

#[test]
fn fatp_tx_is_revalidated_by_mempool_revalidation() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();
	let genesis = api.genesis_hash();

	let mut hashes = vec![];
	let mut prev_hash = genesis;
	hashes.push(genesis);
	for n in 1..=40 {
		let header = api.push_block_with_parent(prev_hash, vec![], true);
		if n >= 22 {
			// not realistic, we only want tx to be invalid, stale is currently the only way in
			// TestApi
			api.set_nonce(header.hash(), Alice.into(), 210);
		} else {
			api.set_nonce(header.hash(), Alice.into(), 199);
			let event = new_best_block_event(&pool, Some(prev_hash), header.hash());
			block_on(pool.maintain(event));
		}
		hashes.push(header.hash());
		prev_hash = header.hash();
	}

	let xt0 = uxt(Alice, 199);

	// note: tx is validated at block 20 (recent best block):
	let xt0_watcher = block_on(pool.submit_and_watch(hashes[21], SOURCE, xt0.clone())).unwrap();

	// note: tx is still valid at block 21
	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), hashes[21])));
	// note: TXMEMPOOL_REVALIDATION_PERIOD passed, tx is stale on block 32:
	block_on(pool.maintain(finalized_block_event(&pool, hashes[21], hashes[32])));

	let xt0_events = block_on(xt0_watcher.collect::<Vec<_>>());
	assert_eq!(xt0_events, vec![TransactionStatus::Ready, TransactionStatus::Invalid,]);
}

/// Ready event lost in manual-seal mode.
///
/// This test simulates the scenario:
/// 1. A block is created and immediately finalized
/// 2. A transaction is submitted AFTER finalization (to the finalized view)
/// 3. The Ready event should be delivered immediately (before any new block)
// see: https://github.com/paritytech/polkadot-sdk/issues/10332
#[test]
fn fatp_watcher_ready_event_after_instant_finalization() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	// Create block 1 (empty)
	let header01 = api.push_block(1, vec![], true);

	// Notify new best and immediately finalize
	block_on(pool.maintain(new_best_block_event(&pool, None, header01.hash())));
	block_on(pool.maintain(finalized_block_event(&pool, api.genesis_hash(), header01.hash())));

	let xt0 = uxt(Alice, 200);
	let mut xt0_watcher =
		block_on(pool.submit_and_watch(header01.hash(), SOURCE, xt0.clone())).unwrap();

	// Poll for the Ready event with retries (up to 2 seconds total)
	let mut ready_event = None;
	for _ in 0..20 {
		std::thread::sleep(Duration::from_millis(100));
		if let Some(event) = xt0_watcher.next().now_or_never().flatten() {
			ready_event = Some(event);
			break;
		}
	}

	assert!(
		ready_event.is_some(),
		"Ready event should be available after submission on active finalized view."
	);

	assert_eq!(
		ready_event.unwrap(),
		TransactionStatus::Ready,
		"First event should be Ready, delivered from the finalized view"
	);
}

#[test]
fn fatp_watcher_replaced_tx_not_reported_as_inblock() {
	sp_tracing::try_init_simple();

	let (pool, api, _) = pool();

	let header01 = api.push_block(1, vec![], true);
	let event = new_best_block_event(&pool, None, header01.hash());
	block_on(pool.maintain(event));

	// Create two transactions with same sender+nonce (different hashes due to Sr25519).
	let xt_original = uxt(Alice, 200);
	let xt_replacement = uxt(Alice, 200);
	api.set_priority(&xt_replacement, 10);

	// Submit original, then replacement — original gets usurped.
	let xt_original_watcher =
		block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt_original.clone())).unwrap();
	let xt_replacement_watcher =
		block_on(pool.submit_and_watch(invalid_hash(), SOURCE, xt_replacement.clone())).unwrap();

	// Verify original was usurped.
	let xt_original_events = futures::executor::block_on_stream(xt_original_watcher)
		.take(2)
		.collect::<Vec<_>>();
	assert_eq!(
		xt_original_events,
		vec![
			TransactionStatus::Ready,
			TransactionStatus::Usurped(api.hash_and_length(&xt_replacement).0),
		]
	);

	// Only replacement should be in the pool now.
	assert_pool_status!(header01.hash(), &pool, 1, 0);

	// Create a block containing the ORIGINAL tx — simulates another node including it
	// before the replacement reached that node.
	let header02 = api.push_block(2, vec![xt_original.clone()], true);
	api.set_nonce(header02.hash(), Alice.into(), 201);

	// Finalize and produce enough blocks for mempool revalidation to trigger.
	let mut prev_header = header01;
	for n in 2..=11 {
		let header = if n == 2 { header02.clone() } else { api.push_block(n, vec![], true) };
		api.set_nonce(header.hash(), Alice.into(), 201);
		let event = finalized_block_event(&pool, prev_header.hash(), header.hash());
		block_on(pool.maintain(event));
		prev_header = header;
	}

	// The replacement tx was never in any block. It must NOT get InBlock or Finalized.
	// It should only get Ready, then Invalid (purged as stale on finalization).
	let xt_replacement_events = block_on(xt_replacement_watcher.collect::<Vec<_>>());
	assert_eq!(xt_replacement_events, vec![TransactionStatus::Ready, TransactionStatus::Invalid,]);
}
