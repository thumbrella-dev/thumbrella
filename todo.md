# Thumbrella Project Plan

## Product Direction

Thumbrella should be designed as a clean-slate media intelligence and thumbnail platform with a narrow, opinionated product surface and a broad internal execution model.

The public contract stays narrow:

- File description and media inspection as a first-class result.
- One canonical thumbnail profile for now.
- Multi-item requests in one call.
- Streaming results as each item finishes.
- Per-item cache validation and up-to-date responses.
- Cancellable operations.
- Optional external cache backends.

The internal architecture stays broad:

- A shared inspection core that can answer what a file is even when no thumbnail is requested.
- A hardcoded thumbnail profile today, but represented internally as a structured generation spec.
- Tiered execution so light environments can handle easy cases and escalate harder cases upward.
- Fetch and inspection primitives designed for partial reads, range requests, and metadata-first decision making.

## Product Surfaces

The service should expose two related capabilities built on the same source analysis pipeline.

### File Intelligence

Return structured information about a file or object source, similar in spirit to `stat`, `file`, `ffprobe`, and object-store metadata APIs.

Typical output should include:

- Source metadata: URL, filename, extension, size, ETag, Last-Modified.
- Basic type identification: MIME, magic classification, coarse kind such as image, video, audio, document, archive, model, or unknown.
- Structural metadata when available: dimensions, duration, stream counts, codec and container info, page count, frame count, alpha presence, animation flag.
- Storage-style facts: content length, cache headers, content disposition, content encoding, range support.
- Analysis provenance: which inspectors ran and how confident the classification is.

### Canonical Thumbnail

Return the opinionated thumbnail artifact for sources that can be thumbnailed, using the same fetch and inspection pipeline.

The design rule is that thumbnailing depends on file intelligence, but file intelligence must not depend on thumbnail generation.

## SaaS Strategy

The minimal viable business strategy should assume the architecture lands before the polished commercial packaging does.

That suggests three parallel lanes rather than one monolithic launch:

- AI-platform distribution for immediate adoption.
- Self-hosted open distribution for developer trust and organic spread.
- Managed and enterprise offerings where the real monetization can emerge.

### Lane 1: AI-Platform Entry Point

The first easy sales pitch is not "buy a new SaaS," it is "you can start thumbnailing hard media types today on infrastructure you already know how to buy."

Examples:

- Replicate now.
- Later Fal, Vertex, and similar managed execution platforms.

Value proposition:

- Hard online thumbnailing becomes immediately accessible.
- Users do not need to assemble ffmpeg, renderers, fetch logic, and cache plumbing themselves.
- They can connect a cheap external cache such as Upstash Redis or Cloudflare KV, or even skip shared cache for light use cases.

Business reality:

- The platform operator captures most hosting revenue.
- Thumbrella still gains distribution, validation, examples, and low-friction adoption.
- This is a market-entry channel more than the final margin engine.

### Lane 2: Managed thumbrella.dev

The next commercial layer is a hosted service that competes on operational efficiency rather than convenience alone.

Why it can be cheaper:

- Tier 1 can run on lighter-weight infrastructure with better cold-start economics.
- A purpose-built multi-tier deployment can route expensive requests upward only when needed.
- Shared caching can reduce repeat work materially.

Core pitch:

- Easier than self-hosting.
- Potentially cheaper than generic AI platforms for steady workloads.
- Better tuned for thumbnailing and file inspection specifically.

Likely managed-service features:

- Hosted shared cache included by default.
- Predictable pricing by requests, compute class, or cached bytes.
- Tier-aware routing handled automatically.
- Streaming batch API and simple endpoint facades.
- Dashboard for request history, cache behavior, and failures.

### Lane 3: Self-Hosted and Enterprise

Self-hosting is not a threat to the business model; it is part of the funnel.

The free self-hosted offering builds:

- credibility
- developer adoption
- internal champions inside larger organizations
- a fallback option for teams with compliance concerns

The paid layer exists when buyers need support, operational polish, and internal-network features rather than just container images.

## Packaging Model

The cleanest packaging strategy is probably open-core rather than trying to artificially cripple the base engine.

### Free and Open

Best suited for:

- local use
- hobby and indie projects
- proof-of-concept deployments
- AI-platform distribution
- basic self-hosting

Likely features:

- core describe and thumbnail APIs
- canonical thumbnail profile
- basic tier deployment
- simple cache adapters
- minimal observability
- docker image and reference configs

### Managed Paid Service

Best suited for:

- teams that want lower operational overhead
- workloads large enough for cache efficiency to matter
- customers who want price advantages over general AI platforms

Likely features:

- hosted cache space
- usage analytics
- service-level guarantees
- easier key management and team accounts
- request logs and debugging tools
- cost controls and quotas

### Enterprise and Supported Self-Host

Best suited for:

- companies with internal media or compliance requirements
- teams that need intranet deployment and internal integrations
- organizations that want procurement-friendly support contracts

Likely paid differentiators:

- more internal-network-oriented cache backends such as Redis, Memcached, and Postgres-backed metadata stores
- cache management tools such as retention policies, cleanup jobs, and LRU controls
- configuration of thumbnail profile variants such as size, quality, and output format
- better coordination and scaling for heavy rendering tiers
- on-prem support and deployment guidance
- internal-facing integrations such as SharePoint, Confluence, and similar file systems
- webhook and ingestion workflows for generate-on-upload pipelines
- pregeneration and backfill tools for expensive media libraries

## Freemium Model

If there is a freemium offering, it should be operationally bounded rather than feature-chaotic.

Better freemium levers:

- limited monthly request volume
- limited retained cache storage
- limited historical logs and analytics
- limited concurrent heavy-render jobs

Avoid weak freemium patterns such as:

- making the base API too crippled to evaluate properly
- hiding the core file-description capability behind paywalls
- splitting the product into confusing micro-plans too early

The free tier should be good enough for prototypes and light production use, while the paid tiers win on economics, scale, governance, and operational tooling.

## Why Customers Pay

The profit path is not just charging for thumbnail bytes. It is charging for operational leverage.

Likely paid value buckets:

- lower per-request cost than generic AI platforms once traffic is steady
- hosted shared cache that cuts repeated work
- better reliability and support for difficult file formats
- governance and enterprise deployment needs
- integrations and automation around existing media systems
- tooling for pre-generation, invalidation, retention, and debugging

The hosted product is valuable because thumbnailing and file inspection over arbitrary remote media is genuinely painful to build and operate well.

## Near-Term Go-To-Market Sequence

The most realistic commercial order probably looks like this:

1. Ship a useful open implementation and AI-platform package.
2. Prove demand through developers who just need thumbnailing to work.
3. Launch a managed thumbrella.dev service optimized around Tier 1 plus Tier 2 economics.
4. Add supported self-hosted and enterprise features once real customer requirements cluster around internal deployment, cache governance, and integrations.

That order matters because it lets the platform credibility and usage patterns shape the paid offering, instead of guessing the enterprise product in advance.

## Canonical Thumbnail Profile

Start with a single built-in profile and make everything else internal plumbing.

- Output format: JPEG only.
- Target box: 256x204 max frame (5:4).
- Quality target: low quality, likely JPEG quality 40-50 to start.
- Background handling: flatten transparency onto a fixed background color.
- Crop policy: deterministic cover/contain policy, not caller-defined.
- Metadata: strip everything not required.

Even though the public API is fixed, the implementation should use an internal generation bundle that can later support new formats and profiles without a rewrite.

Suggested internal shape:

- `ThumbnailProfile`: output format, bounds, crop mode, background, quality.
- `GenerationOptions`: tool-specific knobs derived from the profile.
- `RequestPolicy`: fetch limits, timeout, cache mode, escalation rules.

## Request Model

The next-generation request should be batch-oriented rather than single-asset oriented.

Suggested concepts:

- `ThumbnailBatchRequest`
- `ThumbnailItemRequest`
- `ThumbnailItemResult`
- `ThumbnailEvent`
- `DescribeBatchRequest`
- `DescribeItemResult`
- `DescribeEvent`

Each item request should include:

- Stable item id from caller.
- Source definition: upload, URL, or future storage reference.
- Optional caller metadata.
- Optional cache key override.

Batch-level options should include:

- Cancellation token / request-scoped cancellation context.
- Cache configuration.
- Read-only vs writable cache mode.
- Tier escalation policy.
- Overall timeout / limits policy.

The long-term clean model may be a single batch request where each item can ask for:

- file intelligence only
- thumbnail only
- both file intelligence and thumbnail

## Streaming Response Contract

The batch API should stream item-level events instead of waiting for the whole request to finish.

Likely transport choices:

- NDJSON over HTTP response body.
- Server-Sent Events if browser ergonomics matter early.

NDJSON is probably the simplest starting point because it is easy to produce from Python and easy to proxy.

Suggested event types:

- `item.accepted`
- `item.described`
- `item.cache_hit`
- `item.not_modified`
- `item.progress`
- `item.result`
- `item.error`
- `item.cancelled`
- `batch.complete`

Important behavior:

- Events are emitted independently per item.
- Output order should reflect availability, not input order.
- File intelligence may be emitted before thumbnail completion.
- Cache writes happen after the result has been streamed back.
- Failures for one item must not fail the whole batch unless the client disconnects or the batch is explicitly cancelled.

## File Intelligence Model

The file-information result should be a stable structured object, not a raw dump of subprocess output.

Suggested internal shape:

- `FileDescription`
- `SourceMetadata`
- `TypeClassification`
- `MediaProperties`
- `StorageProperties`
- `InspectionEvidence`

Suggested fields:

- source id or caller item id
- filename
- extension
- MIME from headers
- MIME from libmagic
- normalized kind
- byte size
- ETag
- Last-Modified
- Accept-Ranges support
- image dimensions
- video or audio duration
- codec and container summary
- page count or frame count where available
- embedded thumbnail presence
- parse warnings
- inspector provenance

Raw tool output such as ffprobe JSON may still be preserved internally or optionally exposed for debugging, but the primary contract should stay normalized.

## ETag / Freshness Semantics

There are two different freshness checks and they should stay separate.

### Source freshness

This answers: has the upstream file changed?

Data to capture when possible:

- Source URL.
- Upstream ETag.
- Last-Modified.
- Content-Length.
- Content-Type.
- Optional source fingerprint from partial bytes.

The same source freshness data should validate cached file descriptions, not only thumbnails.

### Thumbnail freshness

This answers: is the cached thumbnail valid for the current generation profile and source state?

Cache identity should include:

- Canonical source identity.
- Source validator state (ETag / Last-Modified / fingerprint).
- Thumbnail profile version.
- Tier / renderer capability version where relevant.

Per-item outcomes should clearly distinguish:

- `not_modified`: caller already has the current thumbnail.
- `cache_hit`: service has a current thumbnail and streams it.
- `generated`: thumbnail was produced now.
- `unsupported`: no capable tier or handler.

## Cache Architecture

Cache must be optional and backend-agnostic.

Initial backend interface:

- `CacheBackend.get(metadata_key)`
- `CacheBackend.get_blob(blob_key)`
- `CacheBackend.put(metadata_key, value)`
- `CacheBackend.put_blob(blob_key, bytes)`
- `CacheBackend.compare_source_validator(...)`

Cache entries should support both description records and thumbnail artifact records.

Supported modes:

- No cache.
- Read-only cache.
- Read-write cache.

Behavior rules:

- Reads may happen inline before generation.
- Writes should happen after the thumbnail result is streamed.
- Cache write failures should be logged and surfaced as internal telemetry, but should not invalidate an already streamed success.

Probable first backends:

- In-memory for local development/tests.
- Redis for server deployments.
- Cloudflare KV or similar for edge-oriented deployments.

## Cancellation Model

Cancellation needs to work at batch level and item level.

Minimum expectations:

- If the client disconnects, in-flight work is cancelled.
- Subprocesses are terminated on cancellation.
- Background cache writes are skipped if generation never completed.
- Batch orchestration stops scheduling new work once cancellation is requested.

Implementation note:

- Use structured concurrency so fetch, decode, render, and cache tasks share a request-scoped cancellation context.
- Tier 3 subprocess wrappers need explicit kill/cleanup behavior.

## Tiered Server Model

Each higher tier must be a superset of the lower tiers.

### Tier 1: Embedded / Placeholder Service

Purpose:

- Extremely lightweight service for edge-style environments.

Capabilities:

- Cheap source description from headers, extensions, magic bytes, and embedded metadata.
- Extract embedded thumbnails from known file formats.
- Basic crop/resize/re-encode.
- Generate deterministic placeholder thumbnails from file type / extension / MIME mapping.

Constraints:

- Should be portable to non-Python environments later if needed.
- No dependence on heavyweight native toolchains.

Escalation triggers:

- No embedded thumbnail available.
- Source type requires real rendering.
- Requested inspection exceeds Tier 1 capability.

### Tier 1 Feasibility Concern: Cloudflare Workers

Cloudflare Workers is the natural target for Tier 1, but image processing inside Workers is a real constraint.

Python is not viable here. Running Python via Pyodide adds too much startup and runtime overhead to be practical within Workers CPU time limits.

Cloudflare's own image resizing API costs $0.50 per 1000 images with no pricing advantage for small outputs. That is too expensive for high-volume thumbnail use.

The remaining option for Workers is fast pure JavaScript image processing. The Workers environment does not support native binaries like `sharp`, so any image work must be done in WASM or pure JS.

Candidates worth evaluating:

- `@cf-wasm/photon` or similar WASM image libraries compiled for Workers.
- Pure JS JPEG decoders and encoders.
- Pixel manipulation using `ImageData`-style approaches without a canvas DOM.

The key open question for Tier 1 is whether a Workers script can, within the CPU time limits, do all of:

1. Read a specific byte range from a remote URL.
2. Locate and extract a JPEG region from within a container such as a zip or EXIF block.
3. Decode it.
4. Crop and resize to 256x204.
5. Re-encode as low-quality JPEG.
6. Return the result.

This needs to be treated as a science experiment before the Tier 1 architecture is committed to Workers. If it is not viable, the tier model needs adjustment.

Fallback if Workers is not viable:

- Tier 1 runs as a normal lightweight container on cheap hosting such as Fly or a single VPS.
- Lose the edge-location advantage but keep the lightweight execution and cold-start economics.
- Workers may still be useful for pure routing and cache lookup, even if image work is deferred to a container tier.

### Tier 2: Media Thumbnail Service

Purpose:

- Mainline renderer for images, video, PDFs, and common media formats.

Capabilities:

- Partial fetch and range-aware inspection.
- MIME detection and metadata inspection.
- ffprobe / ffmpeg / Pillow style processing.
- Progressive and truncated-read optimizations where viable.
- Rich file description for common media and documents.

Important design rule:

- The fetch layer should expose a seek-like buffered abstraction over HTTP, progressively filling data and optionally issuing range requests. Consumers should not need to know whether bytes came from a full download or sparse fetches.

Language note:

Rust with native libav bindings (`ffmpeg-next`) is a stronger implementation target for Tier 2 than Python. The custom `AVIOContext` story is especially relevant: Python's callbacks into a seekable HTTP source add overhead on every read, while Rust can implement the same interface with zero-overhead function pointers. The partial-read architecture is essentially free to implement in Rust and not free in Python. See notes.md for the full reasoning.

This tier is where to invest first.

### Tier 3: Advanced Render Service

Purpose:

- Complex rendering for formats that need large dependencies, shell tools, or GPU-ish environments.

Capabilities:

- 3D model rendering.
- Specialty document/rendering stacks.
- Sandboxed subprocess execution.
- Deep format-specific inspection when Tier 2 tooling is insufficient.

Constraints:

- Custom Docker image.
- More operationally expensive.
- Longer cold starts and tighter sandbox concerns.

Escalation triggers:

- Tier 2 cannot render or would require unsupported dependencies.
- Format-specific handlers declare Tier 3 required.

## Common Handler Interface

Every tier exposes the same handler interface. There is no "Tier 2 input format" versus a "Tier 3 input format" — only one request shape, one response shape.

This keeps escalation simple: a tier that cannot handle a request forwards the identical request object to the next tier up.

Internal calls between tiers (e.g. Tier 1 routing to Tier 2) may use a reduced-overhead path that skips auth, validation, and SSRF checks that were already performed at the entry point. But the data shape is the same.

### Batch Input Shape

The batch endpoint takes a list of item descriptors. Each item is either:

- A bare URL string, or
- An object with `url` and optionally `etag` and `id`.

Suggested input shape:

```json
{
  "items": [
    "https://example.com/file.mp4",
    { "url": "https://example.com/photo.jpg", "etag": "\"abc123\"", "id": "item-42" }
  ],
  "cache": { "mode": "read-write", "backend": "upstash" },
  "ops": ["describe", "thumbnail"]
}
```

The `etag` field lets callers supply a previously seen validator so the service can short-circuit to `not_modified` without fetching at all. The `id` field is returned on every event for that item so callers can correlate results to their own records.

### Result Iterator Model

Internally the processing pipeline should produce results as an iterator or async stream, not a collected batch.

This shapes the architecture from the start so streaming endpoints are a natural consequence rather than a retrofit.

Initial deployment will just collect the iterator to produce a synchronous JSON response — the simplest possible consumer of that iterator.

Once that works, the same iterator drives a streaming endpoint by emitting each event as it becomes available, with no other changes to the pipeline.

Suggested progression:

1. Sync collect endpoint: `POST /batch` waits for all items and returns a JSON array.
2. Streaming endpoint: same `POST /batch` with `Accept: application/x-ndjson` or `text/event-stream` returns events as they arrive.
3. Long-running jobs for Tier 3: same event model, possibly over a webhook or polling pattern.

## Dispatch and Escalation

The dispatcher should become capability-driven rather than tool-driven.

Suggested flow:

1. Normalize request into internal source + profile + policy objects.
2. Attempt cache lookup and freshness validation.
3. Inspect enough source bytes/metadata to classify the asset.
4. Emit file intelligence as soon as it is stable enough to return.
5. Ask the local tier registry for the cheapest capable thumbnail handler if a thumbnail was requested.
6. If unsupported locally and escalation is allowed, forward to the next tier.
7. Stream thumbnail results immediately when ready.
8. Write cache asynchronously after result events.

This implies a few core abstractions:

- `SourceHandle`
- `FetchSession`
- `Inspector`
- `FileDescriber`
- `ThumbnailHandler`
- `TierClient`
- `CacheBackend`
- `BatchOrchestrator`

## Fetch Layer Plan

The fetch subsystem should be designed from scratch around metadata-first behavior rather than treated as a thin download helper.

Phase 1 fetch goals:

- HEAD/initial metadata fetch.
- Enforced max size limits.
- SSRF protections.
- Access to response headers including ETag and Last-Modified.
- Storage-style metadata normalization similar to S3 HEAD responses.

Phase 2 fetch goals:

- Prefix reads for file type sniffing.
- Range requests for tail reads.
- Buffered seekable reader interface.
- Shared download state across multiple inspectors.

Phase 3 fetch goals:

- Tool adapters that can consume the buffered reader or a spill-to-disk representation.
- Better heuristics for progressive JPEG, MP4 metadata atoms, archive manifests, and similar structures.

## Handler Strategy

Prefer many narrow handlers over one giant dispatcher.

Initial handler families:

- Header and magic describer.
- ffprobe-based media describer.
- Document describer.
- Embedded thumbnail extractor.
- Placeholder generator.
- Still image renderer.
- Video frame extractor.
- PDF/document renderer.
- Advanced 3D renderer.

Each handler should declare:

- Supported MIME/extensions.
- Minimum required tier.
- Whether it can operate from partial data.
- Whether it requires local file materialization.

Describers and thumbnail handlers should be separate concepts even if some implementations share tooling.

## API Plan

Target API direction:

- Keep simple endpoints for convenience.
- Keep description-only endpoints for cheap inspection use cases.
- Add a batch endpoint for orchestrated behavior.
- Prefer streamed event responses for the batch path.

Likely endpoint shape:

- `/describe` exists as the simple synchronous file-information facade.
- `/describe/url` exists as a convenience facade.
- `/thumbnail` exists as the simple synchronous facade.
- `/thumbnail/url` exists as a convenience facade.
- `/batch` becomes the real orchestration endpoint for mixed describe and thumbnail workloads.

The simple endpoints can internally call the same batch orchestrator with one item.

## Operational Concerns

Need explicit limits from the start:

- Max source size.
- Max batch size.
- Max concurrent items per batch.
- Per-tier timeout.
- Subprocess timeout.
- Memory spill-to-disk thresholds.

Also worth planning early:

- Abuse limiting / throttling.
- Metrics for cache hit rate, bytes fetched, tier escalation rate, render latency.
- Structured logging around item lifecycle events.
- Packaging and billing boundaries between free, managed, and enterprise tiers.

## Language and Implementation Target

The primary implementation language is Rust throughout the stack. This is a deliberate architectural choice, not a preference:

- Tier 1 WASM: pure Rust image crates compiled to WASM for Cloudflare Workers.
- Tier 2 native: Rust with `ffmpeg-next` / `ffmpeg-sys-next` bindings to native libav.
- Tier 3 native: Rust server that shells out to heavyweight external renderers.

The Cog / Replicate entry point still requires a Python wrapper. When that surface is needed, it will be exposed as a Python extension wrapping the compiled Rust binary, not a reimplementation in Python.

The existing Python source code in this repo is being removed. The Python scaffolding was a useful starting sketch but the architecture has moved past it.

## Build Order

Start with **Tier 2**. It is where the bulk of real media-handling code lives, it can run on any machine with ffmpeg installed, and it does not need the WASM constraints of Tier 1 or the heavy environment of Tier 3.

Once Tier 2 is solid:

- Extract the lightweight subset of Tier 2 down into Tier 1 WASM targets.
- Add the Tier 3 subprocess-heavy renderers on top of the Tier 2 base.

**Tier 3 scope note**: Tier 3 is the "big environment" tier. It handles geometry rendering, calling executables like `usdrender`, `xvfb`, `f3d`, and other tools that need a specific, heavy Docker environment. It is a real compute product in its own right, not just a superset package list.

## Implementation Roadmap

### Milestone 1: Tier 2 Rust Server — Common Media Cases

Goal: a working Rust HTTP server that correctly thumbnails the most common media types end-to-end, using the core architectural interfaces.

In scope:

- Rust workspace and HTTP server skeleton (likely `axum` or `actix-web`).
- Batch request handler accepting a list of URL items with optional etag and id per item.
- Fetch session: HEAD metadata, full download, SSRF protection, size limits.
- `SourceMetadata` type: url, MIME, size, etag, last-modified.
- MIME detection using `infer` or equivalent.
- Result iterator model: internal stream of `ItemEvent` values collected into a sync response.
- Handlers for the first media families:
  - JPEG, PNG, WebP, GIF — still image crop/resize/encode via `image` + `fast_image_resize` + `mozjpeg`.
  - MP4, WebM, MOV — video frame extraction via `ffmpeg-next` with a representative frame selector.
  - PDF — first-page render via `pdfium-render` or `poppler` binding.
- Thumbnail output: JPEG at the canonical profile (256x204, quality ~40, alpha flattened, metadata stripped).
- `/batch` endpoint: collect all item results, return as JSON array.
- `/thumbnail/url` single-item facade: wraps a one-item batch.
- `/health` endpoint.
- Basic integration test: send a list of URLs, verify JPEG thumbnails come back.

Out of scope for Milestone 1:

- Streaming / NDJSON / SSE responses.
- Cache backends.
- ETag freshness / not-modified paths.
- Tier 1 WASM target.
- Tier 3 subprocess renderers.
- Auth, rate limiting, metrics.
- Cog / Replicate Python wrapper.

### Stage 1: Streaming responses

- Add NDJSON streaming to the batch endpoint.
- Emit `item.accepted`, `item.result`, `item.error`, `batch.complete` events.
- Support `Accept` header to switch between collect and stream modes.

### Stage 2: Describe endpoint and file intelligence

- Add `/describe` and `/describe/url`.
- Emit `FileDescription` with source, classification, media, and storage sections.
- Extend batch items to request `describe` only, `thumbnail` only, or both.

### Stage 3: ETag and freshness

- Accept `etag` per item.
- Emit `item.not_modified` when the upstream validator matches.
- Add in-memory cache backend and `item.cache_hit` path.

### Stage 4: More handlers

- SVG rasterization.
- Audio waveform thumbnail.
- ZIP / archive directory placeholder.
- Office document (DOCX, PPTX) via `libreoffice` subprocess.
- EXR and HDR image support.

### Stage 5: Range-aware fetch

- Implement partial-read fetch session (prefix + tail ranges).
- Wire into ffmpeg `AVIOContext` custom IO.
- Measure transfer savings on MP4 and other format families.

### Stage 6: Tier 1 WASM

- Compile still-image pipeline (JPEG, PNG, WebP, embedded thumbnail extraction) to WASM.
- Validate in Cloudflare Workers CPU budget.
- Build routing Worker that serves cache hits and escalates misses to Tier 2.

### Stage 7: Tier 3 heavy renderers

- Add Tier 3 subprocess harness and sandbox model.
- First renderer: GLB/glTF via `f3d` or similar.
- Second renderer: USD via `usdrender` + `xvfb`.
- Docker image with full Tier 3 environment.
- Cog entry point as Python wrapper over compiled binary.

### Stage 8: Commercial packaging

- Redis / Cloudflare KV cache backends.
- Hosted managed service.
- Enterprise cache lifecycle tooling.
- Replicate / Fal / Vertex platform packages.

## Immediate Next Work

The next concrete implementation tasks should be:

- Lock the canonical `ThumbnailProfile` and cache/result semantics.
- Lock the normalized `FileDescription` schema.
- Design the streamed batch event schema before writing handlers.
- Define the fetch session, source metadata, and range-read interfaces.
- Define the inspection pipeline and provenance model.
- Define handler contracts and tier capability negotiation.
- Add a minimal in-memory cache backend to exercise freshness flows.

The next concrete product-strategy tasks should be:

- Decide whether the first commercial motion is AI-platform distribution only or AI-platform plus hosted beta.
- Define which cache backends belong in open core versus enterprise support.
- Decide whether customization of thumbnail profiles is a paid feature, a self-hosted feature, or both.
- Sketch pricing dimensions around requests, compute tier, cache storage, and heavy-render jobs.

## Open Questions

These should be resolved early because they shape the interfaces:

- Is the canonical thumbnail crop mode `cover` or `contain`?
- What exact background color should be used when flattening alpha?
- Is JPEG quality fixed globally, or can handlers lower it further for extremely large inputs?
- Should `not_modified` return no image bytes, or a small event that references the caller's current asset?
- For tier escalation, is the upper tier called synchronously as an HTTP subrequest, or asynchronously as a job?
- Is Tier 1 a conceptual portability target for now, or do we want a real non-Python implementation boundary from the start?
- Can Cloudflare Workers perform a full extract/decode/resize/re-encode JPEG pipeline within CPU time limits using WASM or pure JS? This gates whether the edge deployment model for Tier 1 is viable at all.

## Notes for Media Experiments

Useful experiments still worth running later:

- Compare JPEG output sizes and visual quality at 30, 40, and 50.
- Measure ffmpeg representative frame extraction versus fixed timestamp extraction.
- Verify how far partial reads can go for MP4, progressive JPEG, PDF, and EXR before full download is needed.
- Evaluate whether ffprobe plus libmagic gives enough early classification for dispatch.
- Revisit sandboxing boundaries before Tier 3 lands.

## Tier 1 Science Experiment: Workers JS Image Pipeline

This experiment must happen before finalizing the Tier 1 deployment architecture.

Hypothesis: A Cloudflare Workers script can perform a complete JPEG extract, crop, resize, and re-encode cycle fast enough to be practical.

Test conditions:

- Target: extract a thumbnail-sized JPEG from a zip archive entry or EXIF block via a ranged HTTP fetch.
- Resize and crop to 256x204.
- Re-encode at low JPEG quality.
- Must stay within Workers CPU time limit (typically 10-50ms depending on plan).
- No native binaries. WASM is acceptable.

Things to try:

- `Squoosh` (Google's image codec library) for WASM-backed JPEG decode/encode in Workers. It is designed for browser use and likely has good performance characteristics.
- Rust-to-WASM image pipeline for Workers, using Rust crates for decode, resize, and encode compiled into a small Worker-compatible module.
- `@cf-wasm/photon` for WASM-backed pixel operations in Workers.
- Lightweight pure-JS JPEG decoders such as `jpeg-js`.
- Manual JPEG MCU extraction to avoid full decode when only a region is needed.
- Measuring whether the JS startup cost alone is prohibitive at scale.

Rust-to-WASM is likely the best pure-performance path for Tier 1 in Workers because it keeps the CPU-heavy image code out of JS while still fitting the Workers execution model.

Success criteria:

- End-to-end time under 20ms CPU for a simple JPEG crop and resize in a minimal Worker.
- Output JPEG visually acceptable at 256x204.
- Workers bundled script size reasonable.

Fail criteria and fallback:

- If WASM or JS image processing is too slow, Tier 1 becomes a lightweight containerized Python or Bun service on cheap hosting.
- Workers role may be reduced to routing and cache reads only.
- This does not invalidate the tier model, but it changes the hosting economics.

## Tier 1 Science Experiment: Icon Font Rendering in Workers

Placeholder thumbnails for unsupported file types currently assume a font-based icon can be rendered at request time. Worth validating whether that is practical in a Workers budget.

Hypothesis: rendering a single glyph from an icon font to a JPEG at 256x204 in pure JS or WASM is fast enough per request.

Risk: font loading alone is expensive if the full font file is loaded every request. Even a subset font may be large enough to blow the CPU budget.

Likelier answer: ship a bundle of prerendered icon JPEGs indexed by MIME type, coarse kind, and extension. This turns the placeholder path into a cache lookup and a byte copy rather than a render.

Prerendered icon strategy:

- Generate a static set of placeholder thumbnails offline, not at request time.
- Index them by MIME family, coarse kind, and file extension.
- Store the set in a dedicated asset cache, likely R2 or KV for a Workers deployment.
- On a placeholder request, look up the best matching icon, return it directly.
- Fall through to a generated gray placeholder only if no matching icon exists.

This is almost certainly the right design. The remaining question for the experiment is just confirming that font rendering is not worth attempting at all, so the prerendered path can be committed to without hedging.

The icon set itself is a design artifact that should be produced intentionally rather than assembled ad hoc.

## Cloudflare Workers Subrequest Architecture

Workers can make subrequests to other Workers via Service Bindings. This affects how the Tier 1 pipeline could be decomposed.

Key facts to plan around:

- A Worker can call another Worker synchronously via a Service Binding without going over the public internet.
- Workers are billed and killed on CPU time only, not wall-clock time. A Worker that does a 30ms fetch plus 2ms of actual JS execution only consumes 2ms against its CPU limit.
- IO wait — including fetch, KV reads, R2 reads, and subrequests to other services — does not count against the CPU budget at all.
- CPU time limits apply per Worker invocation. Subrequests to other Workers are billed as separate invocations.

Implication for Tier 1 design:

- A Tier 1 Worker can freely proxy cache-miss requests to Tier 2 or Tier 3 and wait for the response without being billed or killed for that wait time.
- The only CPU budget concern is the JS and WASM execution the Worker itself does: cache lookup, response marshalling, and any local image work on a cache hit.
- Escalation to higher tiers is essentially free from a Workers budget perspective. The user pays latency but not compute.
- This makes the routing Worker design compelling: stay cheap on the fast path, escalate freely on the slow path.

Open question:

- Is it worth splitting the icon lookup, cache check, image processing, and response into separate Worker services for isolation and billing clarity, or does a monolithic Worker with careful code splitting serve Tier 1 better at this stage?
