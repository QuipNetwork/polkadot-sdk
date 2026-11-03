# Substrate statement store implementation

> License: GPL-3.0-or-later WITH Classpath-exception-2.0

The statement store is an off-chain, decentralized data store for cryptographically signed
statements. It enables accounts to publish arbitrary data that can be queried and propagated across
the network without consuming on-chain storage.

**Unlike centralized services, the Statement Store is a weakly coherent system and does not
guarantee message delivery or specific delivery times.**

## Documentation

- [Overview](docs/overview.md) — concepts, design principles, architecture, account quotas,
  channels, and topics.
- [Usage guide](docs/usage.md) — integrating the store: signing, expiry and priority, submission
  errors, delivery semantics, and privacy.

## How do I run a local statement-store node for development?

> This starts a standalone Substrate node with StatementStore turned on. Quota is set directly in
> storage in the next step, without an individuality runtime.

1. Build Substrate node

    ```bash
    cargo build --profile production --locked --bin substrate-node --target x86_64-unknown-linux-gnu
    ```

2. Run it with:

    ```bash
    RUST_LOG=info,statement-store=trace ./target/x86_64-unknown-linux-gnu/debug/substrate-node
    ```

3. To set a quota using sudo, you can use the following
  - 3.1. Identify the account you use, it should be an [account public key][statement-allowance-code].
  - 3.2. Obtain the storage key by running this Python code. You would need your account ID from the
    previous step.
    ```python
      >>> statement_allowance_key = lambda account_id_hex: "0x" + (b":statement_allowance:" + bytes.fromhex(account_id_hex.removeprefix ("0x"))).hex()
      >>> statement_allowance_key("YOUR_ACCOUNT_BYTES_IN_HEX")
    ```
  - 3.3. Using `https://polkadot.js.org/apps/`
    call `Developer->sudo->system->set_storage` with obtained account key and `StatementAllowance`, SCALE encoded.
    For example, to allow an account to store 10 statements and a maximum size of 20 KiB you can use
    `0x0a00000000500000`.
    `0x0a00000000500000` is SCALE for:
    `StatementAllowance { max_count: 10, max_size: 20480 }`
    `0a000000 -> 10 (max_count)`
    `00500000 -> 20480 bytes (max_size, i.e. 20 KiB)`
    > LLMs know how to answer this question if you want to use a different
    quota.

> **Warning:** Use carefully. Do not set big quotas on test environments with SUDO because then
> they will not match production quotas.

## How to use SS with [subxt](https://github.com/paritytech/subxt)

### Submission

`fn submit(&self, encoded: Bytes) -> RpcResult<()>;`

```rust
let result: SubmitResult = rpc_client
    .request("statement_submit", rpc_params![encoded])
    .await
    .with_context(|| format!("Client {client_id}: Failed to submit statement"))?;
```

### Subscription

`fn subscribe_statement(&self, topic_filter: TopicFilter);`

Parameters:

- topic_filter: Which topics to match.

- Use `TopicFilter::Any` to match all topics, `TopicFilter::MatchAll(vec)` to match statements
  that include all provided topics, or `TopicFilter::MatchAny(vec)` to match statements that
  include any of the provided topics.

```rust
let mut subscription: Subscription<StatementEvent> = rpc_client
    .subscribe(
        "statement_subscribeStatement",
        rpc_params![TopicFilter::MatchAny(bounded_topics)],
        "statement_unsubscribeStatement",
    )
    .await
    .with_context(|| format!("Client {client_id}: Failed to subscribe"))?;
```

### Returns

When there are no matching statements in the store, you first receive an empty array. As new
matching statements arrive in the node they are forwarded to the client.

```json
{
  "jsonrpc": "2.0",
  "method": "statement_statement",
  "params": {
    "subscription": 4851578855668545,
    "result": {
      "event": "newStatements",
      "data": {
        "statements": [],
        "remaining": 0
      }
    }
  }
}
```

If there are matching statements in the store you receive them in batches of `newStatements`
events, with `remaining` telling you how many statements remain. This guarantees that the
subscription will receive at least this amount of statements.

```json
{
  "jsonrpc": "2.0",
  "method": "statement_statement",
  "params": {
    "subscription": 1710164133533157,
    "result": {
      "event": "newStatements",
      "data": {
        "statements": [
          "0x1000010000",
          "0x100001000000"
        ],
        "remaining": 10
      }
    }
  }
}
```

If new statements arrive in the store they are delivered as they are, without any remaining
information.

```json
{
  "jsonrpc": "2.0",
  "method": "statement_statement",
  "params": {
    "subscription": 2661920166788434,
    "result": {
      "event": "newStatements",
      "data": {
        "statements": [
          "0x100001000...000"
        ]
      }
    }
  }
}
```

## Using the statement store from TrUAPI

[TrUAPI](https://github.com/paritytech/truapi) is the API surface that Polkadot hosts (e.g. the
Polkadot Desktop Browser) expose to product apps running inside them. It includes a dedicated
statement-store API domain (`subscribe`, `create_proof`, `create_proof_authorized`, `submit`), so
products use the store through the host instead of talking JSON-RPC to a node directly — the host
handles signing and proof creation with the product account's key.

To get started, use the `@parity/truapi` TypeScript client or the higher-level
[`product-sdk`](https://github.com/paritytech/product-sdk). See the Rust API docs at
[paritytech.github.io/truapi](https://paritytech.github.io/truapi) and try the live playground at
[truapi-playground.dot.li](https://truapi-playground.dot.li/).

[statement-allowance-code]: https://github.com/paritytech/polkadot-sdk/blob/cac11f4a5325b217ca74b0c339459597daf03838/substrate/primitives/statement-store/src/lib.rs#L217
