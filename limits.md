# Limits and Throttling

## API Keys and Account-Level Limits

- Every cloud-hosted request requires an API key.
- Accounts have per-key limits on concurrency, sustained load, and total quota.
- Limits vary by account tier (free vs paid).
- Approaching limits → notify user (and admins). Likely email. Consider both
  "you're close" and "you've been in backoff for too long without backing off."

## HTTP Status Codes

Top-level response codes:

- `429 Too Many Requests` — backoff and retry later
- `503 Service Unavailable` — capacity exhausted, retry later
- `402 Payment Required` — out of quota
- `200` with per-item status codes for partial success (see below)

## Per-Item Status Within a Batch

A batch response can be partial success. Each item in the response carries its
own status:

- Some items may succeed (thumbnail returned)
- Some may be deferred (retry this item individually)
- Some may fail permanently (bad URL, unsupported format)

Heavy items (large renders, tier 2/3 formats) can be retried individually even
if the batch partially succeeded.

## Batch Size Limits

- **Tier 1**: target ~10 items per request initially. Tier 1 is time-budget
  constrained (see below), which is the main driver of this limit.
- **Tier 2+**: target ~50 items per request. Less time-sensitive.
- Limits controlled per account tier.

If more items are submitted than the limit allows:
- Option A: process first N, return the rest with a "retry" status.
- Option B (preferred): time-budget based — process as many as possible within
  the per-request time limit, return remaining items with a retry status code.

## Per-Request Time Budget

Rather than a hard item count cutoff, Tier 1 processes items until a per-request
time budget is exhausted. Remaining items get a retry status. This is more
adaptive than a fixed count.

Notes on what counts against the Tier 1 time budget:
- Downloading the source does **not** count (I/O bound, not our work).
- Dispatching to Tier 2 or Tier 3 does **not** count (async handoff).
- Actual decode + encode work counts.

This means the Tier 1 item limit can be fairly accommodating in practice.

## Per-Tier Concurrency Limits

Each tier maintains its own limit on concurrent jobs:

- **Tier 1**: concurrency cap on in-flight decode/encode workers.
- **Tier 2**: concurrency cap on libav decode jobs.
- **Tier 3**: concurrency cap on total subprocess jobs, and likely also a
  separate cap per subprocess type (e.g. max N Ghostscript, max M LibreOffice).

When at capacity, incoming work joins a short-TTL work queue. If the queue
wait exceeds a threshold, respond with a retry status rather than holding the
connection open indefinitely.

## Connection Pool Limits (Outbound HTTP)

- Max 3 concurrent outbound connections per upstream host (enforced globally
  across all in-flight requests, not per-request).
- Prevents Thumbrella from being used as an amplification vector against
  upstream servers.
- Shared reqwest client with keep-alive for connection reuse.
