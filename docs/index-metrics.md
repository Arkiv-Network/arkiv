# Ranking equality bitmap indices

This diagnostic change is based on the deployed `v0.1.0-prefill.1` release
(`57bde54`) and preserves its prefill/import support. It adds telemetry only:
bitmap bytes, account addresses, state roots, and index update behavior are
unchanged. It does not include the separate bitmap compression change.

## Build and endpoint

Build release `v0.1.0-prefill.2` (source branch `debug/index-metrics-prefill`):

```sh
cargo build --release --locked --bin arkiv-reth
```

For a container, use the existing `docker/arkiv-reth.Dockerfile`:

```sh
docker build -f docker/arkiv-reth.Dockerfile -t arkiv-reth:index-metrics-prefill .
```

The `v0.1.0-prefill.2` tag triggers the normal release workflow. Wait for its
container publication to complete before upgrading, then pin
`ghcr.io/arkiv-network/arkiv-reth:v0.1.0-prefill.2` by the published manifest digest.
Keep the existing chain configuration and data directory when upgrading.

Start the node with its existing arguments plus:

```text
--metrics=0.0.0.0:9001
```

The Prometheus endpoint is **`http://<node>:9001/metrics`**. No extra Arkiv flag
or RPC namespace is needed. The producer and watcher Helm templates already
enable this listener and name the container port `metrics`. The db-chain Alloy
discovery already scrapes that port and adds `network`, `component`,
`namespace`, `pod`, and `instance` labels.

For a direct check, replace the namespace and watcher pod placeholders:

```sh
kubectl -n <namespace> port-forward pod/<watcher-el-pod> 9001:9001
# In another terminal:
curl -fsS http://127.0.0.1:9001/metrics | grep '^reth_arkiv_index_equality_'
```

Metrics appear after the node executes index writes. An idle node, a bucket
never touched since restart, or a raw prefill import can leave no samples;
absence does not mean the index is empty.

## Ranked list: index name and rewrite impact

Use an **instant query / table** in Prometheus or Grafana. Select one watcher
pod in `$pod`; replace `$pod` with its literal name when using Prometheus
directly. After at least two scrapes, this returns the 20 equality bitmap
indices rewriting the most serialized bytes per second:

```promql
topk(20,
  sum by (attribute) (
    rate(reth_arkiv_index_equality_rewritten_bytes_total{
      network="sorbet", component="watcher-el", pod="$pod"
    }[5m])
  )
)
```

The result pairs names such as `$all`, `$owner`, `kind`, `ds`, or `chainid`
with bytes/second. Use `increase(...[1h])` instead of `rate(...[5m])` for
total rewritten bytes in an hour. This is the primary ranking for diagnosing
whole-bitmap rewrite amplification: it captures both frequently rewritten
small buckets and repeatedly rewritten large buckets.

Do not sum replicas to estimate chain-wide growth: each watcher executes the
same blocks. A producer can additionally rebuild candidate payloads, so its
workload is different. Without a pod filter, `max by (attribute)` across
watcher rates is useful as a busiest-watcher view, not an aggregate chain
measurement.

To normalize by actual membership changes, rank serialized bytes rewritten
per membership added or removed:

```promql
topk(20,
  sum by (attribute) (
    rate(reth_arkiv_index_equality_rewritten_bytes_total{
      network="sorbet", component="watcher-el", pod="$pod"
    }[5m])
  )
  /
  (
    sum by (attribute) (
      rate(reth_arkiv_index_equality_members_added_total{
        network="sorbet", component="watcher-el", pod="$pod"
      }[5m])
      +
      rate(reth_arkiv_index_equality_members_removed_total{
        network="sorbet", component="watcher-el", pod="$pod"
      }[5m])
    ) > 0
  )
)
```

The denominator filter omits indices with no membership changes in the
interval; the primary rewrite-rate query still includes their work. This
normalization measures execution serialization cost, not a physical disk
amplification ratio.

For the largest **single bucket observed since process start**, in bytes:

```promql
topk(20,
  max by (attribute) (
    reth_arkiv_index_equality_largest_observed_bucket_bytes{
      network="sorbet", component="watcher-el", pod="$pod"
    }
  )
)
```

For net serialized growth per second across executed rewrites:

```promql
topk(20,
  sum by (attribute) (
    rate(reth_arkiv_index_equality_growth_bytes_total{
      network="sorbet", component="watcher-el", pod="$pod"
    }[5m])
    -
    rate(reth_arkiv_index_equality_shrink_bytes_total{
      network="sorbet", component="watcher-el", pod="$pod"
    }[5m])
  )
)
```

The last query can be negative during removals. It is observed execution
growth, not an estimate of the database's physical growth.

## What is measured

All metrics have one instrumentation label, `attribute`. All types and values
of an attribute are aggregated under that name. reth adds the `reth_` prefix.
Every metric below starts with **`reth_arkiv_index_equality_`**:

| Suffix | Type | Meaning |
| --- | --- | --- |
| `rewrites_total` | Counter | Successful bitmap `set_code` calls, including unchanged rewrites. |
| `rewritten_bytes_total` | Counter | Full serialized bytes passed to those calls. |
| `loaded_bytes_total` | Counter | Original serialized lengths loaded for those successful rewrites. |
| `growth_bytes_total` | Counter | Sum of positive `after_bytes - before_bytes`. |
| `shrink_bytes_total` | Counter | Sum of positive `before_bytes - after_bytes`. |
| `members_added_total` | Counter | Actual entity memberships added; duplicates add zero. |
| `members_removed_total` | Counter | Actual entity memberships removed; missing members add zero. |
| `largest_observed_bucket_bytes` | Gauge | Maximum single bucket length, before or after a successful rewrite, since startup. |
| `largest_observed_bucket_members` | Gauge | Maximum single bucket membership count observed in the same way. |

The growth calculation uses the original stored bytes on the first touch of
an existing bucket. An absent account has zero bytes; an empty but serialized
bitmap retains its serialization header. Instrumentation adds no extra
backend reads or bitmap serialization.

These are **equality bitmap indices only**. Range-index B-trees, string
cascades, entity payloads, id maps, trie nodes, and MDBX pages are not measured.
The high-water gauges are maxima across touched buckets, not the sum of all
buckets, a current snapshot, or an inventory of prefilled state. They do not
decrease after a bucket shrinks or disappears, and the two maxima can come
from different buckets.

The counters record successful writes to the execution backend, usually a
transaction overlay. They include speculative payload builds, simulation,
replay, and writes from executions later discarded or reverted. Failed
`set_code` calls and removals from already-empty buckets contribute nothing.
Repeated writes of identical bytes still count rewrite work even though the
content hash is unchanged. Query reads do not contribute. These measurements
are **not committed bytes, unique bytecode growth, physical disk writes, or a
disk write-amplification ratio**. Counters and high-water gauges reset when
the process restarts.

## Cardinality

The seven built-in index names are always retained: `$all`, `$creator`,
`$createdAtBlock`, `$owner`, `$key`, `$expiration`, and `$contentType`.
The first **64 distinct user attribute names**, each valid UTF-8 and at most
128 bytes, also retain their names for the life of the process. They are not
evicted. Values, account addresses, and entity ids never appear as labels.

Other names share **`attribute="$other"`**, including names beyond the cap,
overlong/non-UTF-8 names, and unrecognized reserved names. The cap bounds this
instrumentation to 72 attribute labels and 648 time series per process.
The normal Sourcify names (`ds`, `kind`, `chainid`, `compiler`, etc.) are
eligible for individual attribution.

Include `$other` in rankings: if it accounts for material rewrite volume,
the named rows are an incomplete attribution. `$other`'s largest-bucket gauge
is the largest bucket among the aggregated names, not their combined size.
The first 64 names can differ between processes or restarts.
