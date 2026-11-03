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

//! The simulation driver: stitches together [`MockClock`], [`Executor`], [`Recorder`],
//! [`AnswerQuery`] and one or more spawned subsystems into a single test harness.
//!
//! In Phase H.1 only the unit under test is spawned; auxiliary subsystem slots are an empty
//! vector. Real `prospective-parachains` and `candidate-backing` are wired into the same
//! infrastructure in later phases (H.3 / H.4) without changing the public `Sim` API.

use crate::{
	contract::{Effect, RequestId},
	harness::{
		dispatcher::AnswerQuery,
		pending_fetches::{PendingFetches, RawResponse},
		router::{self, RouteAttempt, SubsystemSlot, UutRoute, UutSlot},
		Recorder,
	},
	report::TimelineReport,
	runtime::{Executor, LocalPoolSpawner, MockClock},
};
use futures::future::BoxFuture;
use polkadot_node_clock::Clock;
use polkadot_node_subsystem::{messages::AllMessages, FromOrchestra, OverseerSignal, SpawnGlue};
use polkadot_node_subsystem_test_helpers::{
	make_subsystem_context, TestSubsystemContext, TestSubsystemContextHandle,
};
use polkadot_overseer::AssociateOutgoing;
use std::{sync::Arc, time::Duration};

/// Configuration for a single simulation run.
#[derive(Default)]
pub struct SimConfig {}

/// A subsystem that can be driven by the test framework.
///
/// Implementations construct the subsystem (via its public API) and return its main-loop future
/// boxed for the executor to drive.
pub trait SubsystemUnderTest: 'static
where
	AllMessages: From<<Self::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<Self::Message>,
{
	/// The subsystem's incoming message type. Drives the type of the test context handle.
	type Message: AssociateOutgoing + std::fmt::Debug + Send + 'static;

	/// Construct the subsystem and return its main-loop future, ready to be spawned on the
	/// pool. The provided `clock` is the deterministic clock the framework drives.
	fn spawn(
		ctx: TestSubsystemContext<Self::Message, SpawnGlue<LocalPoolSpawner>>,
		clock: Arc<MockClock>,
	) -> BoxFuture<'static, ()>;

	/// Try to extract `Self::Message` from an `AllMessages` value addressed to this subsystem.
	///
	/// Used by the router to deliver outbound messages from one auxiliary subsystem (e.g.
	/// `candidate-backing` emitting `CollatorProtocolMessage::Seconded`) into the UUT's
	/// inbound channel.
	///
	/// Returns `Ok(inner)` if the message targets this subsystem, or `Err(msg)` to let the
	/// router try other slots / fall through to classification.
	fn try_extract_inbound(msg: AllMessages) -> Result<Self::Message, AllMessages>;

	/// Build the subsystem-message envelope this subsystem would receive from the
	/// production network bridge when the local node's view changes to `view`.
	///
	/// Mirrors the production fan-out: one `NetworkBridgeEvent::OurViewChange(view)`
	/// gets wrapped per-subsystem as `<Self::Message>::NetworkBridgeUpdate(focused
	/// event)`. The adapter (which knows the subsystem's wire-protocol type) does the
	/// wrapping.
	///
	/// Default returns `None` — subsystems that don't consume network-bridge events
	/// (e.g. `prospective-parachains`) leave this. Adapters for subsystems that DO
	/// consume them override it; the framework's [`crate::world_base::BlockBuilder`]
	/// automatically publishes the view on `.activate()` when this returns `Some`.
	fn our_view_change(_view: polkadot_node_network_protocol::OurView) -> Option<Self::Message> {
		None
	}
}

/// A running simulation around `S`.
pub struct Sim<S: SubsystemUnderTest>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	clock: Arc<MockClock>,
	executor: Executor,
	recorder: Recorder,
	responder: Box<dyn AnswerQuery>,
	/// Shared spawner used by every subsystem context the harness builds. Aux subsystem
	/// constructors clone this so their spawned tasks land on the same `LocalPool`.
	spawner: LocalPoolSpawner,
	uut: UutSlot<S::Message>,
	/// Outbound `AllMessages` channels: the UUT's plus one per registered auxiliary
	/// subsystem. Drained round-robin in registration order on every settle pass.
	outbound_rxs: Vec<futures::channel::mpsc::UnboundedReceiver<AllMessages>>,
	/// Subsystem slots registered with the harness. Index 0 corresponds to the UUT outbound
	/// rx at `outbound_rxs[0]`; index `i+1` corresponds to `outbound_rxs[i+1]`. The UUT slot
	/// itself is stored in `uut`; it does not consume `AllMessages` (test code injects
	/// typed stimuli directly), so it does not appear in this vector.
	aux: Vec<Box<dyn SubsystemSlot>>,
	/// Side table of `oneshot::Sender`s extracted from outbound fetch requests. Tests
	/// resolve them via `Sim::respond_fetch`.
	pending_fetches: PendingFetches,
}

/// Install a process-wide `tracing` subscriber the first time any `Sim` starts. Without
/// this, `gum::trace!` / `tracing::*` events fired by the real subsystems (prospective,
/// backing) go nowhere — debugging a "fetch fired but no second" failure becomes
/// guesswork.
///
/// Driven by `RUST_LOG`; defaults to `off` so unrelated test runs aren't spammed. Typical
/// usage: `RUST_LOG=parachain=trace cargo test ...`.
fn install_tracing_subscriber() {
	use std::sync::Once;
	use tracing_subscriber::{fmt, EnvFilter};
	static INIT: Once = Once::new();
	INIT.call_once(|| {
		let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"));
		let _ = fmt::Subscriber::builder()
			.with_env_filter(filter)
			.with_test_writer()
			.with_target(true)
			.try_init();
	});
}

impl<S: SubsystemUnderTest> Sim<S>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	/// Spin up the simulation. Constructs a `MockClock`, a single-threaded executor, a
	/// `TestSubsystemContext`, and spawns the subsystem's main loop. Returns a handle the test
	/// uses to drive stimuli and observe effects.
	pub fn start<R>(_cfg: SimConfig, responder: R) -> Self
	where
		R: AnswerQuery + 'static,
	{
		install_tracing_subscriber();
		let clock = Arc::new(MockClock::default());
		let mut executor = Executor::new();

		let spawner = LocalPoolSpawner::new();
		executor.set_spawn_drain(spawner.drain_handle());
		let (ctx, handle) = make_subsystem_context::<S::Message, _>(spawner.clone());

		let uut = UutSlot { name: "uut", inbound_tx: handle.tx.clone() };
		let TestSubsystemContextHandle { rx: uut_outbound_rx, .. } = handle;

		let fut = S::spawn(ctx, clock.clone());
		executor.spawn(fut);
		// First poll lets the subsystem reach its initial parked state before any stimulus.
		executor.poll_until_pending();

		Self {
			clock,
			executor,
			recorder: Recorder::new(),
			responder: Box::new(responder),
			spawner,
			uut,
			outbound_rxs: vec![uut_outbound_rx],
			aux: Vec::new(),
			pending_fetches: PendingFetches::new(),
		}
	}

	/// The shared `LocalPoolSpawner` used to build subsystem contexts. Aux constructors
	/// clone this so their spawned background tasks land on the same `LocalPool`.
	pub fn spawner(&self) -> LocalPoolSpawner {
		self.spawner.clone()
	}

	/// Direct access to the executor, for spawning auxiliary subsystem futures during
	/// registration helpers. Tests don't usually need this.
	pub fn executor_mut(&mut self) -> &mut Executor {
		&mut self.executor
	}

	/// Access to the deterministic clock.
	pub fn clock(&self) -> &Arc<MockClock> {
		&self.clock
	}

	/// Access to the recorder. Tests can inspect entries directly when convenient.
	pub fn recorder(&self) -> &Recorder {
		&self.recorder
	}

	/// Inject a typed message into the UUT and settle. Drives the subsystem until it parks,
	/// draining any outbound messages produced into recorder/responder/aux slots.
	pub fn inject(&mut self, msg: FromOrchestra<S::Message>) {
		self.executor.run_until(self.uut.send_typed(msg));
		self.drain();
	}

	/// Inject an `OverseerSignal`. The signal is broadcast to the UUT and every registered
	/// auxiliary subsystem. Settles after delivery.
	pub fn signal(&mut self, signal: OverseerSignal) {
		// UUT first so its handler runs before aux subsystems may produce dependent messages.
		self.executor.run_until(self.uut.send_signal(signal.clone()));
		for slot in &self.aux {
			let fut = slot.send_signal(signal.clone());
			self.executor.run_until(fut);
		}
		self.drain();
	}

	/// Inject a regular subsystem message into the UUT and settle.
	pub fn send(&mut self, msg: S::Message) {
		self.inject(FromOrchestra::Communication { msg });
	}

	/// Advance simulated time by `dur`. Iteratively resolves wakeups until either the time
	/// budget is exhausted or no further wakeup falls inside the remaining window.
	///
	/// Plain `MockClock::advance(d)` only fires wakeups whose deadline already exists at the
	/// time of the call. Tick streams (e.g. `tick_stream`) re-register a new wakeup every
	/// time the previous one fires; without iteration `Sim::advance(25s)` would only
	/// surface one tick. Settling between sub-steps lets every wakeup land.
	pub fn advance(&mut self, dur: Duration) {
		let target = self.clock.now() + dur;
		loop {
			let now = self.clock.now();
			if now >= target {
				break;
			}
			let remaining = target - now;
			match self.clock.next_wakeup_in() {
				Some(d) if d <= remaining => {
					self.clock.advance_to_next_wakeup();
				},
				Some(_) | None => {
					// Either the next wakeup is past the target, or no wakeups are pending.
					// Step the clock to the target in one go.
					self.clock.advance(remaining);
				},
			}
			self.executor.poll_until_pending();
			self.drain();
		}
	}

	/// Wait for an effect matching `predicate` to appear in the recorder, advancing the clock
	/// as needed up to `within`. Searches the entire observation log so a stimulus that
	/// produced its effect synchronously before this call still matches. Panics with a
	/// [`TimelineReport`] on timeout.
	#[track_caller]
	pub fn expect<F>(&mut self, predicate: F, within: Duration, expected: &str) -> Effect
	where
		F: Fn(&Effect) -> bool,
	{
		let location = std::panic::Location::caller();
		let at_str = format!("{}:{}", location.file(), location.line());
		let start_sim_t = self.now_sim_t();

		self.drain();
		if let Some(eff) = self.find_match(&predicate) {
			return eff;
		}

		loop {
			let elapsed_in_window = self.now_sim_t().saturating_sub(start_sim_t);
			if elapsed_in_window >= within {
				let report = TimelineReport {
					expected: expected.to_string(),
					actual: format!("timed out at sim_t = {}ms", self.now_sim_t().as_millis()),
					window_start: start_sim_t,
					window: within,
					recorder: &self.recorder,
					replay_seed: None,
					at: Some(&at_str),
					hint: None,
				};
				panic!("expectation failed:\n{}", report);
			}

			let remaining = within - elapsed_in_window;
			match self.clock.next_wakeup_in() {
				Some(d) if d <= remaining => {
					self.clock.advance_to_next_wakeup();
				},
				Some(_) | None => {
					self.clock.advance(remaining);
				},
			};
			self.executor.poll_until_pending();
			self.drain();
			if let Some(eff) = self.find_match(&predicate) {
				return eff;
			}
		}
	}

	/// Register an auxiliary subsystem slot whose outbound stream the harness should drain.
	///
	/// `slot` is the [`SubsystemSlot`] for routing inbound messages and signals. `outbound_rx`
	/// is the receiver side of the test-context the auxiliary subsystem was constructed
	/// with; the harness polls it on every settle pass and feeds outbound messages back into
	/// the router.
	pub fn register_aux<A: SubsystemSlot + 'static>(
		&mut self,
		slot: A,
		outbound_rx: futures::channel::mpsc::UnboundedReceiver<AllMessages>,
	) {
		self.aux.push(Box::new(slot));
		self.outbound_rxs.push(outbound_rx);
	}

	/// Register an auxiliary subsystem slot only — for use by slots that do not produce
	/// outbound `AllMessages` (e.g. a no-op test fixture).
	pub fn register_aux_slot_only<A: SubsystemSlot + 'static>(&mut self, slot: A) {
		self.aux.push(Box::new(slot));
	}

	/// Resolve an outstanding fetch by [`RequestId`] with `response`. The corresponding
	/// `oneshot::Sender` (parked by the harness when the subsystem fired
	/// `NetworkBridgeTxMessage::SendRequests`) is consumed and the subsystem's await unblocks.
	///
	/// Settles the executor afterwards so the subsystem can react to the response.
	///
	/// Panics if `request_id` is unknown (already responded, or no fetch with this id).
	///
	/// [`RequestId`]: crate::contract::RequestId
	pub fn respond_fetch(&mut self, request_id: RequestId, response: RawResponse) {
		let sender = self.pending_fetches.take(request_id).unwrap_or_else(|| {
			panic!(
				"Sim::respond_fetch: no outstanding fetch for {:?} (already responded? unknown id?)",
				request_id
			)
		});
		// `send` consumes the sender and may fail if the receiver was dropped — the
		// subsystem giving up on the fetch is a legitimate outcome the test may want to
		// observe via a subsequent effect, so don't panic on send failure.
		let _ = sender.send(response);
		self.executor.poll_until_pending();
		self.drain();
	}

	/// Number of outstanding fetches awaiting a response. Useful for assertions like "exactly
	/// one fetch was fired" before delivering the response.
	pub fn pending_fetches(&self) -> usize {
		self.pending_fetches.len()
	}

	/// Drop the response sender for `request_id`, which makes the awaiting `oneshot`
	/// receiver resolve with `Canceled`. From the subsystem's POV this is the equivalent
	/// of a network-level timeout / cancellation. Experimental's collation-fetch path
	/// classifies this as `RequestError::Canceled` (`is_timed_out() == true`) and applies
	/// `FAILED_FETCH_SLASH` to the responding peer's reputation.
	///
	/// Panics if `request_id` is unknown.
	pub fn cancel_fetch(&mut self, request_id: RequestId) {
		let sender = self.pending_fetches.take(request_id).unwrap_or_else(|| {
			panic!(
				"Sim::cancel_fetch: no outstanding fetch for {:?} (already responded? unknown id?)",
				request_id
			)
		});
		// Dropping the sender resolves the receiver's await with `Canceled`.
		drop(sender);
		self.executor.poll_until_pending();
		self.drain();
	}

	/// Assert that NO effect matching `predicate` is observed within `within` from this call.
	/// Panics with a [`TimelineReport`] showing the offending effect if one is found.
	///
	/// "From this call" is enforced by an entry-index barrier, not a `sim_t` cutoff: only
	/// effects recorded after `expect_no` is entered count. Effects already in the log —
	/// including ones recorded earlier in the very same simulated instant — are ignored, so
	/// `expect_no` never false-fails on a prior step's effect that happens to share the
	/// current `sim_t`.
	#[track_caller]
	pub fn expect_no<F>(&mut self, predicate: F, within: Duration, expected_absence: &str)
	where
		F: Fn(&Effect) -> bool,
	{
		let location = std::panic::Location::caller();
		let at_str = format!("{}:{}", location.file(), location.line());
		let start_sim_t = self.now_sim_t();

		// Drain anything already pending, then snapshot the log length. Everything recorded
		// from here on is what this assertion is about.
		self.drain();
		let barrier = self.recorder.len();

		let panic_on_match = |this: &Self, eff: &Effect| -> ! {
			let report = TimelineReport {
				expected: format!("absence of: {}", expected_absence),
				actual: format!(
					"found a matching effect at sim_t = {}ms: {}",
					this.now_sim_t().as_millis(),
					crate::report::format_effect(eff),
				),
				window_start: start_sim_t,
				window: within,
				recorder: &this.recorder,
				replay_seed: None,
				at: Some(&at_str),
				hint: None,
			};
			panic!("expect_no failed:\n{}", report);
		};

		if let Some(eff) = self.recorder.find_effect_from(barrier, &predicate).cloned() {
			panic_on_match(self, &eff);
		}

		// Advance through the window; bail at the first newly-recorded match.
		loop {
			let elapsed = self.now_sim_t().saturating_sub(start_sim_t);
			if elapsed >= within {
				return;
			}
			let remaining = within - elapsed;
			match self.clock.next_wakeup_in() {
				Some(d) if d <= remaining => {
					self.clock.advance_to_next_wakeup();
				},
				Some(_) | None => {
					self.clock.advance(remaining);
				},
			}
			self.executor.poll_until_pending();
			self.drain();
			if let Some(eff) = self.recorder.find_effect_from(barrier, &predicate).cloned() {
				panic_on_match(self, &eff);
			}
		}
	}

	/// Count the number of recorded effects matching `predicate`. Useful for "exactly N
	/// fetches in flight" assertions.
	pub fn count_effects<F: Fn(&Effect) -> bool>(&self, predicate: F) -> usize {
		self.recorder
			.entries()
			.iter()
			.filter(|o| match o {
				crate::harness::observation::Observation::Effect(s) => predicate(&s.value),
			})
			.count()
	}

	/// Convenience: assert exactly `expected` effects matching `predicate` are recorded
	/// right now. Panics with timeline on mismatch.
	#[track_caller]
	pub fn expect_count<F: Fn(&Effect) -> bool>(
		&self,
		predicate: F,
		expected: usize,
		description: &str,
	) {
		let actual = self.count_effects(predicate);
		assert_eq!(
			actual,
			expected,
			"expected exactly {} {} (got {}):\n\n{}",
			expected,
			description,
			actual,
			crate::report::format_timeline(&self.recorder),
		);
	}

	/// Like [`Self::expect_count_after`], but asserts `actual >= at_least` instead of
	/// equality. Use when the contract specifies a lower bound — e.g. "after the timeout
	/// at least one new fetch fires" — and the upper bound depends on subsystem-internal
	/// scheduling decisions tests shouldn't lock to.
	#[track_caller]
	pub fn expect_at_least_after<F: Fn(&Effect) -> bool>(
		&self,
		since: Duration,
		predicate: F,
		at_least: usize,
		description: &str,
	) {
		let actual = self
			.recorder
			.entries()
			.iter()
			.filter(|o| match o {
				crate::harness::observation::Observation::Effect(s) => {
					s.sim_t >= since && predicate(&s.value)
				},
			})
			.count();
		assert!(
			actual >= at_least,
			"expected at least {} {} since sim_t={}ms (got {}):\n\n{}",
			at_least,
			description,
			since.as_millis(),
			actual,
			crate::report::format_timeline(&self.recorder),
		);
	}

	/// Variant of [`Self::expect_count`] that only counts effects with `sim_t >= since`.
	/// Tests use this with [`Self::now_sim_t`] to bound a count to a specific window
	/// — e.g. "exactly 1 SendRequest fired between this point and end of test."
	#[track_caller]
	pub fn expect_count_after<F: Fn(&Effect) -> bool>(
		&self,
		since: Duration,
		predicate: F,
		expected: usize,
		description: &str,
	) {
		let actual = self
			.recorder
			.entries()
			.iter()
			.filter(|o| match o {
				crate::harness::observation::Observation::Effect(s) => {
					s.sim_t >= since && predicate(&s.value)
				},
			})
			.count();
		assert_eq!(
			actual,
			expected,
			"expected exactly {} {} since sim_t={}ms (got {}):\n\n{}",
			expected,
			description,
			since.as_millis(),
			actual,
			crate::report::format_timeline(&self.recorder),
		);
	}

	/// Conclude every spawned subsystem, drain remaining work, return all recorded
	/// observations.
	pub fn finish(mut self) -> Recorder {
		self.executor.run_until(self.uut.send_signal(OverseerSignal::Conclude));
		for slot in &self.aux {
			let fut = slot.send_signal(OverseerSignal::Conclude);
			self.executor.run_until(fut);
		}
		self.executor.poll_until_pending();
		self.drain();
		self.recorder
	}

	/// Current simulation time as a `Duration` since the sim started. Tests use this as a
	/// barrier to filter the recorder for effects that fire after a known point.
	pub fn now_sim_t(&self) -> Duration {
		Duration::from_millis(self.clock.duration_since_epoch().as_millis() as u64)
	}

	/// Find the first effect anywhere in the log matching `predicate`. Used by `expect`,
	/// which matches the whole log so an effect a stimulus produced synchronously (before
	/// `expect` was called) still counts.
	fn find_match<F: Fn(&Effect) -> bool>(&self, predicate: &F) -> Option<Effect> {
		self.recorder.find_effect_from(0, predicate).cloned()
	}

	fn drain(&mut self) {
		loop {
			// Stamp effects with simulated time elapsed since sim start — the same origin
			// `now_sim_t` reports — so windowed assertions and recorded `sim_t` agree.
			let sim_t = self.now_sim_t();
			let mut progressed = false;
			for idx in 0..self.outbound_rxs.len() {
				match self.outbound_rxs[idx].try_next() {
					Ok(Some(msg)) => {
						progressed = true;
						let uut_route = UutRouteFor::<S> { uut: &self.uut };
						let aux = self.aux.as_slice();
						let recorder = &mut self.recorder;
						let responder = &mut *self.responder;
						let pending = &mut self.pending_fetches;
						self.executor.run_until(router::route(
							sim_t,
							msg,
							Some(&uut_route),
							aux,
							recorder,
							responder,
							pending,
						));
						// Other subsystems may now have work to do (forwarded messages
						// reached their inboxes; oneshot replies unblocked someone).
						self.executor.poll_until_pending();
					},
					Ok(None) | Err(_) => {},
				}
			}
			if !progressed {
				break;
			}
		}
	}
}

/// Type-tagged adapter that lets the router call back into the UUT slot for inbound delivery.
struct UutRouteFor<'a, S: SubsystemUnderTest>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	uut: &'a UutSlot<S::Message>,
}

impl<'a, S: SubsystemUnderTest> UutRoute for UutRouteFor<'a, S>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	fn try_route(&self, msg: AllMessages) -> RouteAttempt {
		match S::try_extract_inbound(msg) {
			Ok(inner) => {
				let fut =
					self.uut.send_typed(polkadot_node_subsystem::FromOrchestra::Communication {
						msg: inner,
					});
				RouteAttempt::Accepted(fut)
			},
			Err(other) => RouteAttempt::Declined(other),
		}
	}
}
