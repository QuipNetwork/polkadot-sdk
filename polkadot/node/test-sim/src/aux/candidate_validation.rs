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

//! Controllable stub for `candidate-validation`.
//!
//! Real `candidate-validation` runs WASM PVF execution — disqualified for deterministic
//! tests. The stub answers `CandidateValidationMessage::ValidateFromExhaustive` requests
//! synchronously based on a [`Verdict`] callback that the test owns.
//!
//! # Default behaviour
//!
//! [`CandidateValidationStub::always_valid`] returns `ValidationResult::Valid` with
//! canned-but-consistent commitments derived from the request.
//!
//! # Customising
//!
//! [`CandidateValidationStub::with_verdict`] takes a closure
//! `Fn(&CandidateReceipt, &PoV) -> Verdict` for tests that need to reject specific
//! candidates or simulate execution errors.

use crate::harness::router::{RouteAttempt, SubsystemSlot};
use futures::{channel::mpsc, future::BoxFuture, FutureExt, SinkExt};
use polkadot_node_primitives::{InvalidCandidate, PoV, ValidationResult};
use polkadot_node_subsystem::{
	messages::{AllMessages, CandidateValidationMessage},
	FromOrchestra, OverseerSignal,
};
use polkadot_primitives::{
	CandidateCommitments, CandidateHash, CandidateReceiptV2 as CandidateReceipt,
	PersistedValidationData,
};
use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, Mutex},
};

/// Shared registry mapping `CandidateHash` → `(commitments, pvd)`. The
/// [`CandidateValidationStub::always_valid`] stub consults this table when validating —
/// candidates registered here get their bespoke commitments and PVD echoed back, which is
/// what real backing then seconds.
///
/// Without this, every candidate validates with empty `default_commitments`, which makes
/// the backing pipeline ignore `head_data` threading and breaks fragment-chain scenarios.
#[derive(Clone, Default)]
pub struct CandidateOutputs {
	inner: Arc<Mutex<HashMap<CandidateHash, (CandidateCommitments, PersistedValidationData)>>>,
}

impl CandidateOutputs {
	/// Register `(commitments, pvd)` for `hash`. Subsequent validation calls for that
	/// candidate will reproduce these outputs.
	pub fn insert(
		&self,
		hash: CandidateHash,
		commitments: CandidateCommitments,
		pvd: PersistedValidationData,
	) {
		self.inner
			.lock()
			.expect("CandidateOutputs lock")
			.insert(hash, (commitments, pvd));
	}

	fn get(&self, hash: &CandidateHash) -> Option<(CandidateCommitments, PersistedValidationData)> {
		self.inner.lock().expect("CandidateOutputs lock").get(hash).cloned()
	}
}

/// Shared set of candidate hashes the validation stub should reject. The
/// [`CandidateValidationStub::valid_unless`] constructor consults it on every request,
/// so tests can mark candidates invalid before or after spawning the aux graph.
#[derive(Clone, Default)]
pub struct SharedInvalidSet {
	inner: Arc<Mutex<HashSet<CandidateHash>>>,
}

impl SharedInvalidSet {
	/// Mark `hash` as invalid: subsequent validation requests for it return
	/// `ValidationResult::Invalid`.
	pub fn insert(&self, hash: CandidateHash) {
		self.inner.lock().expect("SharedInvalidSet lock").insert(hash);
	}

	fn contains(&self, hash: &CandidateHash) -> bool {
		self.inner.lock().expect("SharedInvalidSet lock").contains(hash)
	}
}

/// Verdict the stub returns for a single validate request.
pub enum Verdict {
	/// Candidate validates with the given commitments and PVD.
	Valid(CandidateCommitments, PersistedValidationData),
	/// Candidate is invalid; the wrapped reason surfaces in `ValidationResult::Invalid`.
	Invalid(InvalidCandidate),
}

type VerdictFn = dyn FnMut(&CandidateReceipt, &PoV) -> Verdict + Send + 'static;

/// Stub subsystem for `candidate-validation`. Answers the oneshot in
/// `CandidateValidationMessage::ValidateFromExhaustive` immediately based on the configured
/// verdict.
pub struct CandidateValidationStub {
	inbound_tx: mpsc::Sender<FromOrchestra<CandidateValidationMessage>>,
}

impl CandidateValidationStub {
	/// Stub that approves every candidate. Commitments default to the framework's empty
	/// shape (no upward messages, no horizontal messages, head data the candidate descriptor
	/// already implies). For fragment-chain scenarios, register specific outputs via
	/// [`CandidateOutputs`] — the registry takes precedence over the default.
	pub fn always_valid<S>(sim: &mut crate::harness::Sim<S>, outputs: CandidateOutputs) -> Self
	where
		S: crate::harness::SubsystemUnderTest,
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		Self::with_verdict(sim, move |receipt, _| {
			if let Some((commitments, pvd)) = outputs.get(&receipt.hash()) {
				return Verdict::Valid(commitments, pvd);
			}
			let pvd = crate::builders::fixtures::dummy_pvd();
			Verdict::Valid(default_commitments(), pvd)
		})
	}

	/// Like [`Self::always_valid`], but candidates in `invalid` fail validation with
	/// `InvalidCandidate::InvalidOutputs`. Everything else follows the `always_valid`
	/// logic: registry lookup first, canned defaults otherwise. This is the real-verdict
	/// path for "fetched fine, failed validation" scenarios — backing reacts to the
	/// invalid result exactly as in production.
	pub fn valid_unless<S>(
		sim: &mut crate::harness::Sim<S>,
		outputs: CandidateOutputs,
		invalid: SharedInvalidSet,
	) -> Self
	where
		S: crate::harness::SubsystemUnderTest,
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		Self::with_verdict(sim, move |receipt, _| {
			if invalid.contains(&receipt.hash()) {
				return Verdict::Invalid(InvalidCandidate::InvalidOutputs);
			}
			if let Some((commitments, pvd)) = outputs.get(&receipt.hash()) {
				return Verdict::Valid(commitments, pvd);
			}
			let pvd = crate::builders::fixtures::dummy_pvd();
			Verdict::Valid(default_commitments(), pvd)
		})
	}

	/// Stub with a caller-provided verdict closure. Spawn the stub's worker future on the
	/// harness executor; the worker pulls inbound messages and replies on each request's
	/// oneshot synchronously.
	pub fn with_verdict<S, F>(sim: &mut crate::harness::Sim<S>, verdict: F) -> Self
	where
		S: crate::harness::SubsystemUnderTest,
		F: FnMut(&CandidateReceipt, &PoV) -> Verdict + Send + 'static,
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		let (inbound_tx, mut inbound_rx) =
			mpsc::channel::<FromOrchestra<CandidateValidationMessage>>(0);

		let mut verdict: Box<VerdictFn> = Box::new(verdict);
		let fut = async move {
			use futures::StreamExt;
			while let Some(msg) = inbound_rx.next().await {
				match msg {
					FromOrchestra::Signal(OverseerSignal::Conclude) => break,
					FromOrchestra::Signal(_) => {},
					FromOrchestra::Communication { msg } => match msg {
						CandidateValidationMessage::ValidateFromExhaustive {
							candidate_receipt,
							pov,
							response_sender,
							..
						} => {
							let result = match verdict(&candidate_receipt, &pov) {
								Verdict::Valid(commitments, pvd) => {
									ValidationResult::Valid(commitments, pvd)
								},
								Verdict::Invalid(reason) => ValidationResult::Invalid(reason),
							};
							let _ = response_sender.send(Ok(result));
						},
						other => panic!(
							"CandidateValidationStub: unhandled CandidateValidationMessage \
							 variant: {:?}. Extend the stub when a new message family becomes \
							 relevant.",
							other
						),
					},
				}
			}
		};
		sim.executor_mut().spawn(fut.boxed());
		sim.executor_mut().poll_until_pending();
		Self { inbound_tx }
	}
}

impl SubsystemSlot for CandidateValidationStub {
	fn name(&self) -> &'static str {
		"candidate-validation-stub"
	}

	fn send_signal(&self, signal: OverseerSignal) -> BoxFuture<'static, ()> {
		let mut tx = self.inbound_tx.clone();
		async move {
			let _ = tx.send(FromOrchestra::Signal(signal)).await;
		}
		.boxed()
	}

	fn try_route(&self, msg: AllMessages) -> RouteAttempt {
		match msg {
			AllMessages::CandidateValidation(inner) => {
				let mut tx = self.inbound_tx.clone();
				let fut = async move {
					let _ = tx.send(FromOrchestra::Communication { msg: inner }).await;
				}
				.boxed();
				RouteAttempt::Accepted(fut)
			},
			other => RouteAttempt::Declined(other),
		}
	}
}

/// Convenience: empty commitments. The candidate descriptor implies the head data; the rest
/// is empty.
fn default_commitments() -> CandidateCommitments {
	CandidateCommitments {
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		new_validation_code: None,
		head_data: polkadot_primitives::HeadData(vec![1, 2, 3]),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	}
}
