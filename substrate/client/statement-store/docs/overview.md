# Statement Store

The statement store is an off-chain, decentralized data store for cryptographically signed
statements in the Polkadot SDK. It lets accounts publish arbitrary data that can be queried and
propagated across the network without consuming on-chain storage.

Key characteristics:

- **Off-chain storage** — keeps the data off-chain to avoid blockchain state bloat.
- **Gossip-based distribution** — statements propagate node-to-node over a notification protocol.
- **Cryptographic verification** — every statement is signed and verified before acceptance.
- **Topic-based indexing** — up to four topics per statement for efficient querying.
- **Account-based limits** — per-account quotas bound how much an account may store.
- **Automatic eviction** — lower-priority statements are evicted when limits are reached.
- **Multiple access methods** — reachable over JSON-RPC for applications and via the runtime API
  (offchain workers) for on-chain logic.

## Design principles

The service is built around two pillars — **scalability** and **graceful degradation** — and
deliberately avoids the properties of a centralized data structure, prioritizing privacy,
scalability, and decentralization by reducing other guarantees:

- **No centralized-system guarantees.** The store is weakly coherent: it does not guarantee message
  delivery or specific delivery times, operating on a best-effort basis. Under saturation, quality
  of service (latency, propagation speed) degrades gracefully rather than the system halting.
- **Generic transport layer.** It does not natively guarantee data confidentiality, integrity, or
  authenticity; applications must layer those properties on top (e.g. their own encryption).
- **Local enforcement.** Global quotas cannot be enforced instantly in a decentralized system, so
  nodes enforce quotas locally and global consistency is only eventual. Node operators may also
  apply their own storage limits or quality-of-service.

Because delivery is not guaranteed — notably under full network saturation, malicious nodes, or
network partitioning/ISP failures — the store is best thought of as closer to UDP than TCP, and
applications should use fail-safe mechanisms that confirm a message was delivered before proceeding.

## Architecture

The implementation is split across several crates:

- **`sp-statement-store`** — the [`Statement`] structure, core types, the [`StatementStore`] runtime
  and client interfaces, and cryptographic primitives.
- **`sc-statement-store`** — the disk-backed (ParityDB) store: constraint management, the on-disk
  indexes with their bounded in-memory caches, and subscription handling.
- **`sc-network-statement`** — gossip-based propagation, per-peer state, and topic affinity.
- **`pallet-statement`** — the runtime pallet: it turns on-chain statement events into statements
  (via an offchain worker) and defines the bounds used to compute account allowances.
- **`sc-rpc-api` / `sc-rpc`** — the JSON-RPC API (a statement subscription and a submit method) used
  by external clients; see
  [`StatementApiClient`](../sc_rpc_api/statement/trait.StatementApiClient.html).

## Account quota

An account's budget is a `StatementAllowance { max_count, max_size }`, enforced on two axes at the
same time:

- `max_count` — the maximum number of statements the account may have stored.
- `max_size` — the maximum total **data** size in bytes (the length of each statement's `data`
  field, not its full SCALE-encoded size).

Because both are hard limits, an account may spend its budget as a few large statements or many
small ones, up to whichever limit it reaches first. For example, with `max_size` = 50 KiB and
`max_count` = 50: an account could store two 25 KiB statements (filling the size budget but using
only 2 of 50 slots), or fifty 1 KiB statements (reaching both limits at once).

When a submission would exceed either limit, the account's lowest-priority statements (those with
the lowest expiry) are evicted to make room. If a single statement's data exceeds `max_size` it is
rejected outright; and if, even after evicting every lower-priority statement, the new one still
does not fit, it is rejected too. Global limits apply on top of per-account limits: the store holds
at most `DEFAULT_MAX_TOTAL_STATEMENTS` statements and `DEFAULT_MAX_TOTAL_SIZE` of data.

Allowances are not fixed in code: they are held in chain state (keyed under
`STATEMENT_ALLOWANCE_PREFIX`) and granted or revoked by the runtime; an account with no allowance —
or a depleted one — cannot store statements. The bounds within which a runtime may set allowances
are configured by `pallet-statement`.

## Channels

A channel is an optional per-account identifier (`Option<Channel>`, 32 bytes) used for message
replacement: only one statement per `(account, channel)` pair is stored at a time.

Submitting a new statement on a channel that already has one replaces the previous statement from
the same account **only if the new statement has a strictly higher expiry** (priority). Replacing
frees the old statement's size; if the new statement is larger and replacing the old one is not
enough to stay within the account quota, additional lowest-priority statements from the same
account are evicted, and if it still does not fit the submission is rejected.

A statement with no channel is never subject to channel uniqueness — it is only ever removed by
priority-based eviction or expiry. The store does not prescribe how a channel id is generated.

## Topics

Topics are the primary attribute used to query and filter statements. A statement carries up to
`MAX_TOPICS` (4) topics, each a 32-byte identifier, and the store maintains a `topic -> statements`
index for fast lookups.

Subscriptions filter by topic in one of three ways:

- `Any` — match every statement.
- `MatchAll(topics)` — match statements that include **all** of the given topics (up to `MAX_TOPICS`
  = 4 topics; an empty set matches everything).
- `MatchAny(topics)` — match statements that include **any** of the given topics (up to
  `MAX_ANY_TOPICS` = 128 topics).

Topics are neither private nor reserved: any statement may use any topic, so it is the
application's responsibility to filter or validate them. At the network layer, peers can
additionally advertise a topic *affinity* (a bloom filter) so that only statements matching their
interests are propagated to them.

## Best practices

- **Do not use the store for synchronous communication.** It is designed for asynchronous hand-offs
  and signaling, not real-time exchange between nodes or apps.
- **Do not route peer-to-peer data through the store.** High-bandwidth or continuous peer-to-peer
  traffic should go over a direct channel (e.g. WebRTC); use the statement store only to signal the
  setup of such a connection, not to carry its traffic.
- **Do not rely on timely or guaranteed delivery.** Network congestion can delay or drop
  propagation — treat the store as closer to UDP than TCP.
