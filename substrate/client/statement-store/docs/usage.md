# Integrating the statement store

This guide covers what an application developer needs to know to integrate the statement store. It
focuses on integration-specific guidance; for the concepts and mechanics it refers to the crate
overview above and the API docs ([`StatementStore`], [`Statement`], and the RPC client
[`StatementApiClient`](../sc_rpc_api/statement/trait.StatementApiClient.html)) rather than repeating
them.

## Authorization and signing

There is no separate authentication step. Every statement must be **signed by the submitting
account's key**, and the store authorizes submission by checking that the account has a non-zero
allowance ([`StatementAllowance`]); an account with no allowance cannot store statements.

## Channels and topics in practice

**Channels** act as a single-slot mailbox per `(account, channel)` pair: only one statement per
channel is kept, and a new one replaces the old one **only if it has strictly higher priority**
(a greater `expiry`). This makes them ideal for replaceable state, keeping only the latest value on
a channel. A lower-or-equal-priority submission on an existing channel is rejected with
[`RejectionReason::ChannelPriorityTooLow`]. There is no built-in way to fetch statements by
channel — retrieval is by topic only.

**Topics** are network-wide, public, and unreserved: anyone may post to any topic, and every
subscriber listening on a topic receives matching statements. They are the main element for
grouping related statements. The store does not prescribe how a `TopicID` (32 bytes) is chosen;
two common patterns and their trade-offs:

- `hash(app_name + public_key)` — convenient, but since the public key is known this lets an
  observer correlate all of an application's statements for that key (metadata leakage).
- `hash(public_key + entropy)` — avoids that correlation where no metadata leakage is acceptable.

It is the application's responsibility to filter and validate topics (see
[Privacy and security](#privacy-and-security)).

## Expiry and priority

A statement's `expiry` is a single `u64` that packs an expiration timestamp (seconds since the
UNIX epoch) in the **high 32 bits** and a sequence number in the **low 32 bits**; a higher value is
higher priority. Eviction is purely priority-based and treats channel and non-channel statements
uniformly — there is no separate per-channel eviction.

For example, to keep only the latest value on a channel, use a fixed future expiry timestamp with
an incrementing sequence number:

```rust
// Two successive updates share the same channel and the same future expiry timestamp, but the
// sequence number increments so the newer one outranks the older.
let expires_at: u64 = 0x69F5_4B54; // ~30 days in the future, seconds since the UNIX epoch

let first: u64 = (expires_at << 32) | 1;
let second: u64 = (expires_at << 32) | 2;

// Higher priority, so submitting `second` on the same channel replaces the `first` statement.
assert!(second > first);
assert_eq!(second, 0x69F5_4B54_0000_0002);
```

A caveat for client design: within a shared application namespace, a statement with a very high
expiry can crowd out others, and one with a very low expiry is easily evicted — choose priorities
deliberately.

## Submission outcomes and errors

Submitting returns a `SubmitResult`: `new` (accepted), `known` / `knownExpired` (already seen), or a
failure. Failures are of two kinds.

Rejections ([`RejectionReason`]) — the statement is valid but does not fit:

- **`channelPriorityTooLow`** ([`RejectionReason::ChannelPriorityTooLow`]) — a statement already
  exists on this `(account, channel)` and the new one is not strictly higher priority. Because a
  channel is a single-slot mailbox, replacement requires a strictly higher `expiry`; this stops an
  attacker from cheaply overwriting an existing channel message.
- **`dataTooLarge`** — the data exceeds the account's remaining size allowance.
- **`accountFull`** — the account is at its statement limit and the new expiry is too low to evict
  an existing one.
- **`storeFull`** — the global store is full.
- **`noAllowance`** — the account has no allowance to use the store.

Validation failures ([`InvalidReason`]) — the statement is malformed or unusable:

- **`noProof`** / **`badProof`** — missing or invalid signature.
- **`encodingTooLarge`** — the encoded statement exceeds the network size limit.
- **`alreadyExpired`** — the expiry timestamp is already in the past.

## Duplicates, retries, and delivery

The store does **not** guarantee delivery or timing (see the overview's design principles). In
practice:

- **Deduplicate on the client.** On reconnecting with a filter you receive *all* currently matching
  statements again, so applications must handle duplicates.
- **Handle retries and fail-safes.** Under network saturation a statement may not propagate;
  applications should retry and confirm delivery according to their own logic before proceeding.

## Privacy and security

As a transport layer the store provides no privacy or integrity guarantees of its own; applications
build those on top. The main considerations:

- **Metadata leakage (IP).** Submitting statements to a node reveals network metadata such as the
  client's IP address. Mitigation: end users should use a VPN.
- **No confidentiality or integrity.** The store neither encrypts nor verifies application data.
  Mitigation: the application must encrypt and sign its payloads.
- **Identity correlation.** A signing key does not directly identify a person, but reusing one
  keypair across interactions (including with other services) allows correlation. Mitigation: use a
  voucher-based privacy mechanism provided by the identity layer.
- **Unverified statement injection.** A client trusts the node it queries to return only valid
  statements, and because topics are open, a malicious node can inject spam — even spam encrypted
  to the user's public key. The application must therefore filter unwelcome statements and validate
  that topics are relevant and senders are trusted.

For general usage do's and don'ts (asynchronous use only, not routing peer-to-peer traffic through
the store), see the best-practices section of the overview.
