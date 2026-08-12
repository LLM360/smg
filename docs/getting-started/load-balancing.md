---
title: Load Balancing
---

# Load Balancing

SMG provides multiple load balancing policies to distribute requests across workers. Set the policy with `--policy`:

```bash
smg --worker-urls http://w1:8000 http://w2:8000 --policy cache_aware
```

<div class="prerequisites" markdown>

#### Before you begin

- Completed the [Getting Started](index.md) guide
- Two or more workers running

</div>

---

## Policy Comparison

| Policy | Load Aware | Cache Affinity | Session Affinity | Best For |
|--------|:----------:|:--------------:|:----------------:|----------|
| `cache_aware` | Yes | Yes | — | **Production LLM** |
| `bucket` | Yes | — | — | PD disaggregation |
| `least_load` | Yes | Optional | — | TokenSpeed K3 gRPC Chat |
| `power_of_two` | Yes | — | — | General load balancing |
| `consistent_hashing` | — | — | Yes | Session affinity |
| `prefix_hash` | Yes | Partial | — | Lightweight caching |
| `manual` | — | — | Yes | Stateful chat |
| `round_robin` | — | — | — | Even distribution |
| `random` | — | — | — | Testing |

---

## Cache-Aware (Recommended)

The production default. Maintains a radix tree mirroring backend KV cache state for optimal prefix routing with load balancing fallback. Maximizes KV cache hits (60-90% hit rate), reduces TTFT by 70-75%.

```bash
smg \
  --policy cache_aware \
  --worker-urls http://w1:8000 http://w2:8000 \
  --cache-threshold 0.3 \
  --balance-abs-threshold 64 \
  --balance-rel-threshold 1.5
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--cache-threshold` | `0.3` | Minimum prefix match ratio (0.0–1.0) to route to highest-match worker. At or below this threshold, routes to the least-loaded healthy worker |
| `--balance-abs-threshold` | `64` | Absolute load difference threshold — triggers load balancing when exceeded |
| `--balance-rel-threshold` | `1.5` | Relative load ratio threshold — triggers load balancing when max_load > min_load × ratio |
| `--eviction-interval` | `120` | Seconds between LRU eviction cycles for the radix trees |
| `--max-tree-size` | `67108864` | Maximum nodes per radix tree. Excess nodes are evicted during maintenance cycles |

Best for multi-turn conversations, RAG applications, and batch processing with shared templates.

---

## Power of Two Choices

Samples two random workers and routes to the one with lower load. Good load distribution with minimal overhead.

```bash
smg --policy power_of_two --worker-urls http://w1:8000 http://w2:8000
```

Best for heterogeneous workers with varying response times.

---

## Least Load Cache Credit (K3 gRPC Chat)

`least_load` normally routes by backend queue work plus work sent since the
last load poll. Its optional cache credit is a narrow rollout feature for a
single active gateway serving one K3 model through direct, scalar, streaming
gRPC Chat Completions:

```text
hybrid score = queued uncached prompt tokens / prefill throughput
             + (running + waiting) * mean remaining decode / generation throughput
             + hybrid work sent since the last load poll
             + KV-pressure barrier
             - certified cached prompt tokens / prefill throughput
```

Positive live generation throughput is used when available. Otherwise the
configured `--least-load-default-throughput` is used. The prefix term is a
lower bound built only from contiguous KV events with an exact rolling-prefix
match. A disconnected or unknown worker receives zero cache credit. Missing
load data, no fresh certified KV stream, unsupported topology, and every other
request path use the existing least-load selector unchanged.

The feature defaults to `off`. Start with `shadow` to observe decisions without
changing dispatch, then use `enforce` only after calibrating the fleet values:

```bash
smg \
  --policy least_load \
  --least-load-cache-mode shadow \
  --least-load-cache-prefill-throughput 8000 \
  --least-load-mean-remaining-decode-tokens 2048 \
  --least-load-default-throughput 200
```

The example values are the initial M2 calibration, not universal K3 constants.
Calibrate them from the deployment's generation and prefill measurements. In
particular, the legacy global generation fallback of `2000` tok/s is not a K3
recommendation. The first M2 shadow sweep should cover generation fallback
throughput `150/200/250`, cache prefill throughput `6000/8000/12000`, and mean
remaining decode `1024/2048/4096`. Certified KV events expire after 30 seconds
without a new batch because the live-only stream does not provide a heartbeat.

---

## Consistent Hashing

Header-based routing with minimal redistribution on scaling. Routes based on `X-SMG-Routing-Key` header or implicit keys (`Authorization`, `X-Forwarded-For`, `Cookie`).

```bash
smg --policy consistent_hashing --worker-urls http://w1:8000 http://w2:8000
```

### Routing Headers

| Header | Description |
|--------|-------------|
| `X-SMG-Target-Worker` | Direct routing by worker index (0-based) |
| `X-SMG-Routing-Key` | Consistent hash routing for session affinity |

**Priority:** `X-SMG-Target-Worker` > `X-SMG-Routing-Key` > Implicit keys > Random fallback

Best for session affinity and user-to-worker pinning.

---

## Prefix Hash

A lightweight alternative to full cache-aware routing. Routes based on a hash of the first N tokens using consistent hashing with bounded load balancing.

```bash
smg \
  --policy prefix_hash \
  --worker-urls http://w1:8000 http://w2:8000 \
  --prefix-token-count 256 \
  --prefix-hash-load-factor 1.25
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--prefix-token-count` | `256` | Number of prefix tokens to hash. Longer = more precise routing, shorter = more requests grouped together |
| `--prefix-hash-load-factor` | `1.25` | Load threshold ratio — if a worker's load exceeds avg_load × factor, walk the hash ring to find a less loaded worker |

Lower memory than `cache_aware` with predictable O(log n) performance.

---

## Bucket

Routes requests based on text length with adaptive boundaries. Periodically adjusts boundaries based on observed load distribution.

```bash
smg \
  --policy bucket \
  --worker-urls http://w1:8000 http://w2:8000 http://w3:8000 \
  --balance-abs-threshold 64 \
  --balance-rel-threshold 1.5
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--balance-abs-threshold` | `64` | Absolute load difference threshold for load balancing |
| `--balance-rel-threshold` | `1.5` | Relative load ratio threshold for balancing decisions |

Best for PD disaggregation where prefill workers handle different request sizes.

---

## Manual

Sticky session routing with explicit routing key mapping. Sessions stay with their assigned worker even when new workers are added. Requires `X-SMG-Routing-Key` header.

```bash
smg \
  --policy manual \
  --worker-urls http://w1:8000 http://w2:8000 \
  --assignment-mode min_load \
  --max-idle-secs 14400 \
  --eviction-interval 120
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--assignment-mode` | `random` | Strategy for assigning new routing keys: `random`, `min_load` (fewest active requests), or `min_group` (fewest routing keys) |
| `--max-idle-secs` | `14400` | Maximum idle time (seconds) before a routing entry is evicted. Default is 4 hours |
| `--eviction-interval` | `120` | Seconds between TTL eviction cycles |

Best for stateful chat where context is stored on workers.

---

## Round Robin

Rotates through workers sequentially. Skips unhealthy workers automatically.

```bash
smg --policy round_robin --worker-urls http://w1:8000 http://w2:8000
```

---

## Random

Each healthy worker has equal probability of selection. Zero state overhead.

```bash
smg --policy random --worker-urls http://w1:8000 http://w2:8000
```

---

## Choosing a Policy

| Requirement | Recommended Policy |
|-------------|-------------------|
| Production LLM inference | `cache_aware` |
| Session affinity (sticky sessions) | `manual` or `consistent_hashing` |
| PD disaggregation | `bucket` |
| Load balancing without cache | `power_of_two` |
| Lightweight cache locality | `prefix_hash` |
| Even distribution | `round_robin` |
| Testing/development | `random` |

---

## Next Steps

- [Load Balancing Concepts](../concepts/routing/load-balancing.md) — Detailed policy architecture, advantages/limitations, scenario guides
- [Cache-Aware Routing Concepts](../concepts/routing/cache-aware.md) — Radix tree architecture and routing algorithm deep dive
- [Tokenizer Caching](tokenizer-caching.md) — Reduce tokenization overhead with two-level caching
