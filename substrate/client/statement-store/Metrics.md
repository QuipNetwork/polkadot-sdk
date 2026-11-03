# Statement Store & Network Dashboards

Document provides a reference for every metric displayed in the two
Statement Store Grafana dashboards. For each metric, you will find: what it measures,
why it matters how to read it and what problems it helps diagnose.

## Table of Contents

- [Dashboard 1: Substrate Statement Store](#dashboard-1-substrate-statement-store)
  - [Storage Overview](#storage-overview)
  - [Throughput & Operations](#throughput--operations-use-method)
  - [Errors & Rejections](#errors--rejections-red-method)
  - [Capacity & Limits](#capacity--limits)
- [Dashboard 2: Substrate Statement Network](#dashboard-2-substrate-statement-network)
  - [Network Overview](#network-overview)
  - [Network Throughput & Bandwidth](#network-throughput--bandwidth)
  - [Operation Latency](#operation-latency)
  - [Network Health Indicators](#network-health-indicators)
  - [Statement Expiration & Cleanup](#statement-expiration--cleanup)
  - [Initial Sync](#initial-sync)
---

## Dashboard 1: Substrate Statement Store

**UID:** `substrate-statement-store`

This dashboard covers the storage engine: capacity, throughput, errors, and lifecycle
of statements in the database

### Storage Overview

#### Total Statements
- **Metric:** `substrate_sub_statement_store_statements_total`
- **Type:** Gauge
- **What it measures:** Count of statements persisted in the ParityDB store.
- **Why it matters:** Primary capacity indicator.
- **How to read:** Single stat panel. Green = healthy, yellow = 70%+ of capacity, red = 90%+.

#### Storage Used (Bytes)
- **Metric:** `substrate_sub_statement_store_bytes_total`
- **Type:** Gauge
- **What it measures:** Total bytes consumed by statement data in the store.
- **Why it matters:** Metric detects byte-level saturation independently of count.
- **How to read:** Color thresholds match statement count: green/yellow/red at 70%/90% of capacity.

#### Unique Accounts
- **Metric:** `substrate_sub_statement_store_accounts_total`
- **Type:** Gauge
- **What it measures:** Number of distinct accounts (public keys) that have at least one statement
  in the store. Counted at startup and refreshed each time the periodic allowance sweep completes
  a full pass over the account index, so the value may lag the exact count by up to one sweep
  cycle.
- **Why it matters:** Indicates the diversity of statement authors. A single account flooding
  the store is a sign of abuse or misconfiguration. A healthy network shows many accounts.
- **How to read:** Single stat panel. No threshold coloring (informational).
- **Problems it solves:**
  - Detect single-account flooding (one account with thousands of statements).
  - Monitor adoption: how many unique participants are using the statement store.

#### Expired Statements
- **Metric:** `substrate_sub_statement_store_expired_total`
- **Type:** Gauge
- **What it measures:** Statements that have been marked as expired but not yet purged (purge happens after 48h by default).
- **Why it matters:** A growing backlog of expired statements means the cleanup process
  is falling behind, or the expiration rate is very high.
- **How to read:** Single stat. Green = 0-9, yellow = 10-99, red = 100+.
- **Problems it solves:**
  - Detect stalled or slow cleanup processes per node.
  - Identify nodes where expired statements accumulate (possible disk I/O issue).

---

### Throughput & Operations

#### Submission Rate (Successful)
- **Metric:** `rate(substrate_sub_statement_store_submitted_statements[$__rate_interval])`
- **Type:** Counter (displayed as rate)
- **What it measures:** Successful new statement submissions per second.
- **Why it matters:** Throughput metric it tells you how fast the system
  is processing valid work. Drops indicate upstream issues (fewer clients, network problems);
  spikes indicate bursts of activity.
- **How to read:** Line chart showing submissions/sec. Legend shows mean/max/sum.
- **Problems it solves:**
  - Detect throughput degradation after a release (compare before/after deployment).

#### Throughput vs Errors
- **Metrics:**
  - `rate(submitted_statements)` (green = successful)
  - `sum(rate(validations_invalid))` (red = invalid, summed over `reason` label)
  - `sum(rate(rejections_total))` (orange = rejected, summed over `reason` label)
- **What it measures:** Comparison of successful vs failed operations.
- **Why it matters:** A healthy system shows green >> red + orange.
- **How to read:** Overlapping lines ratio of green to red/orange is the key signal.
- **Problems it solves:**
  - Detect capacity saturation.
  - Compare error ratios across releases to ensure quality.

---

### Errors & Rejections

#### Invalid Validations Rate
- **Metric:** `rate(substrate_sub_statement_store_validations_invalid[$__rate_interval])` with label `reason`
- **Type:** CounterVec (displayed as rate; use `sum by (reason)` for breakdown)
- **Labels:** `reason` = `no_proof` | `bad_proof` | `encoding_too_large` | `already_expired`
- **What it measures:** Statements that failed validation during submission, broken down by reason:
  - **`no_proof`**: Statement has no account / missing proof so it cannot be attributed.
  - **`bad_proof`**: Signature verification failed — tampered or malformed proof.
  - **`encoding_too_large`**: Encoded statement exceeds `MAX_STATEMENT_SIZE`.
  - **`already_expired`**: Statement's expiration timestamp was already in the past at submission time.
- **Why it matters:** Invalid statements consume validation resources
  (CPU for signature verification) without producing useful work. High rates indicate
  malicious actors submitting garbage, network corruption, or client bugs. Breakdown by
  reason tells you which class dominates.
- **How to read:** Line chart. Any sustained rate above 0 warrants investigation. Use
  `sum by (reason)` to identify the dominant failure mode.
- **Problems it solves:**
  - Detect ongoing attacks (high `bad_proof`)
  - Detect client bugs (high `encoding_too_large` or `no_proof`)
  - Detect network corruption
  - Detect misconfigured clients submitting already-expired statements

#### Rejection Reasons Breakdown
- **Metric:** `rate(substrate_sub_statement_store_rejections_total[$__rate_interval])` with label `reason`
- **Type:** CounterVec (displayed as stacked rate)
- **Labels:** `reason` = `data_too_large` | `channel_priority_too_low` | `account_full` | `store_full` | `no_allowance`
- **What it measures:** Statements that passed validation but were rejected by the store
- **Why it matters:** Each rejection reason points to a different problem:
  - **`store_full`** (red): The global store is at capacity. All new submissions are rejected.
    Action: increase capacity or reduce statement lifetimes.
  - **`account_full`** (orange): A specific account has hit its per-account quota. The account
    owner needs to remove old statements before submitting new ones.
  - **`channel_priority_too_low`** (yellow): A statement tried to replace a higher-priority
    statement in the same channel. The submitter should increase priority.
  - **`data_too_large`** (purple): Statement data exceeds `max_size`. The client is submitting
    oversized data.
  - **`no_allowance`** (grey): The account has no statement allowance set by the runtime.
    The runtime must grant an allowance before the account can submit statements.
- **How to read:** Bar chart color of the dominant bar tells you the primary
  rejection cause.

---

### Capacity & Limits

#### Statement Capacity vs Usage
- **Metrics:** `capacity_statements` (dashed red line), `statements_total` (solid blue line)
- **What it measures:** Absolute statement count plotted against the configured maximum.
- **Why it matters:** Visual gap between the two lines shows remaining headroom.
- **How to read:** Two lines gap remaining capacity.
- **Problems it solves:**
  - Capacity configuration changes took effect after a deployment

#### Byte Capacity vs Usage
- **Metrics:** `capacity_bytes` (dashed red line), `bytes_total` (solid purple line)
- **What it measures:** Same as above, but for byte-level capacity.

---

## Dashboard 2: Substrate Statement Network

**UID:** `substrate-statement-network`

Dashboard covers the gossip/networking layer: peer connectivity, statement propagation,
bandwidth, latency, and network health indicators.

### Network Overview

#### Peers Connected (Stat)
- **Metric:** `substrate_sync_statement_peers_connected`
- **Type:** Gauge
- **What it measures:** Current number of peers connected via the statement gossip protocol.
- **Why it matters:** Zero peers means complete isolation: the node cannot send or receive
  statements. Low peer counts (<3) mean slow propagation and possible partitioning.
- **How to read:** Single stat.
- **Problems it solves:**
  - Detect degraded connectivity: few peers mean slow statement propagation.

#### Pending Validations (Stat)
- **Metric:** `substrate_sync_pending_statement_validations`
- **Type:** Gauge
- **What it measures:** Number of statements waiting in the validation queue.
- **Why it matters:** metric for the validation pipeline.
- **How to read:** Single stat.
- **Problems it solves:**
  - Detect validation bottleneck: if consistently high

#### Statements Received vs Propagated (Time Series)
- **Metrics:**
  - `rate(statements_received)` (blue = received from peers)
  - `rate(propagated_statements)` (green = sent to peers)
- **What it measures:** Inbound vs outbound statement rates over time.
- **Why it matters:** Primary indicator of network participation balance:
  - **Received ≈ Propagated**: Balanced node. Receiving and forwarding roughly equally.
  - **Received >> Propagated**: Mostly consuming. The node may be new or catching up.
  - **Propagated >> Received**: Mostly producing. The node is generating more than it receives
- **How to read:** Two-line chart. Mean and max values in the legend table.
- **Problems it solves:**
  - Compare network activity across releases to detect propagation regressions.
  - Identify nodes that are not propagating (broken outbound path).

#### Peers Connected Over Time
- **Metric:** `statement_peers_connected` (time series)
- **What it measures:** Peer count connected over time.
- **Why it matters:** Drops indicate network instability for example node restarts
- **How to read:** Line chart with mean/max/last in lgend. Look for step changes and trends.
- **Problems it solves:**
  - Detect network partitioning (drop to 0).
  - Detect gradual peer loss

---

### Network Throughput & Bandwidth

#### Network Bandwidth (Statement Protocol)
- **Metrics:**
  - `rate(statement_bytes_sent_total)` (orange = bytes sent/sec)
  - `rate(statement_bytes_received_total)` (blue = bytes received/sec)
- **Type:** Counter
- **What it measures:** Actual network bandwidth consumed by the statement protocol.
- **Why it matters:** Metric for sizing network requirements.
- **How to read:** Two-line chart in bytes/sec. Compare sent vs received for balance.
- **Problems it solves:**
  - Detect bandwidth issues
  - Compare bandwidth across releases to measure protocol efficiency improvements

---

### Operation Latency

Good for performance comparison across releases. Each shows p50/p90/p99
percentiles, making it easy to spot tail latency regressions.

#### Submit Latency (Total)
- **Metric:** `substrate_sub_statement_store_submit_duration_seconds` (histogram)
- **Buckets:** 1μs, 10μs, 100μs, 1ms, 10ms, 100ms, 1s
- **What it measures:** Time to submit a statement, including signature verification,
  runtime validation, and database write.
- **Why it matters:** This is the single most important latency metric it affects
  how fast the node can process incoming statements. If p99 > SLO per sec node will build
  up backpressure under load.
- **How to read:** Three lines (green=p50, yellow=p90, red=p99 gap between p50 and p99 shows tail latency.
- **Problems it solves:**
  - Detect latency regressions after deployments (compare p50/p90/p99 before and after).
  - Identify which sub-operation is slow by comparing with verify/DB write panels.
  - Set SLOs: "p99 submit latency < (N)ms".

#### Statement Send Latency (Percentiles)
- **Metric:** `substrate_sync_statement_sent_latency_seconds` (histogram)
- **Buckets:** 1μs, 10μs, 100μs, 1ms, 10ms, 100ms, 1s
- **What it measures:** Time to send a statement notification to a peer via the network layer.
- **Why it matters:** Measures network-level latenc high values indicate network
  congestion, slow peers
- **How to read:** Three lines (p50/p90/p99)
- **Problems it solves:**
  - Detect network congestion between specific peers
  - Identify slow peers
  - Compare send latency across releases to measure protocol improvements
  - Set SLOs: "p99 send latency < 2s"

---

### Network Health Indicators

---

### Statement Expiration & Cleanup

#### Expiration Check Latency
- **Metric:** `substrate_sub_statement_store_check_expiration_duration_seconds` (histogram)
- **Buckets:** 1μs, 10μs, 100μs, 1ms, 10ms, 100ms, 1s
- **What it measures:** Time spent in each expiration check cycle. Expiration periodically reaps
  due statements off the on-disk expiry index and walks a bounded batch of accounts to enforce
  their allowances.
- **Why it matters:** Expiration checks run on the main store thread. If they take too long,
  they block statement submissions
- **How to read:** Three lines (p50/p90/p99). Should typically be sub-millisec.
- **Problems it solves:**
  - Detect expiration performance degradation as the store grows
  - Identify if expiration is blocking the submission path

#### Statements Expiration Rate
- **Metric:** `rate(substrate_sub_statement_store_statements_expired_total[$__rate_interval])`
- **What it measures:** Rate at which statements are expired (marked for later purge).
- **Why it matters:** A sudden spike means many statements expired
  at once
- **How to read:** Line chart showing expired/sec. Legend shows mean/max/sum.
- **Problems it solves:**
  - Verify that the expirtion system is working
  - Detect mass expiration events

---

### Initial Sync

#### Initial Sync Active Peers
- **Metric:** `substrate_sync_initial_sync_peers_active`
- **Type:** Gauge
- **What it measures:** Number of peers currently being synced via the initial sync burst mechanism
- **Why it matters:** Each active initial sync consumes bandwidth and CPU. Too many concurrent
  syncs can starve normal gossip propagation
- **How to read:** Single stat. Green = 0-4, yellow = 5-19, red = 20+.
- **Problems it solves:**
  - Detect excessive initial syncs
  - Capacity planning for nodes that frequently accept new peers

#### Initial Sync Statements Sent
- **Metric:** `rate(substrate_sync_initial_sync_statements_sent[$__rate_interval])`
- **Type:** Counter (displayed as rate)
- **What it measures:** Rate of statements sent to peers during initial sync bursts.
- **Why it matters:** High rates indicate many new peers are connecting and receiving full
  statement sets. Impacts network bandwidth
- **How to read:** Line chart showing statements/sec. Legend shows mean/max
- **Problems it solves:**
  - Help to understand initial sync's share of network traffic

#### Initial Sync Burst Rate
- **Metric:** `rate(substrate_sync_initial_sync_bursts_total[$__rate_interval])`
- **Type:** Counter (displayed as rate)
- **What it measures:** Rate of initial sync burst rounds being processed.
- **Why it matters:** Each burst sends one batch of statements to one peer (round-robin)
  burst rate combined with statements-per-burst gives the effective sync throughput
- **How to read:** Line chart showing bursts/sec. Legend shows mean/max.
- **Problems it solves:**
  - Verify that the round-robin distributing work across peers
  - Detect stalled initial syncs (burst rate drops to 0 while active peers > 0)

#### Per-Peer Initial Sync Duration (Percentiles)
- **Metric:** `substrate_sync_initial_sync_duration_seconds` (histogram)
- **Buckets:** 10ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, 30s, 60s
- **What it measures:** Total wall-clock time from the first burst to completion of initial
  sync for each peer.
- **Why it matters:** Long sync durations mean new peers wait a long time before having
  a complete view of the statement store. Threshold: 5s (yellow), 30s (red).
- **How to read:** Three lines (green=p50, yellow=p90, red=p99).
- **Problems it solves:**
  - Detect slow initial syncs that delay new peer participation
  - Compare sync durations as statement store size grows
  - Identify network-level bottlenecks

---
