i feel like we've pulled non-rust dependencies into tier 1. maybe just mozjpeg. this is going to prevent building tier1 to wasm (although I'm pretty sure there's wasm mozjpeg builds out there?). but for my sanity I think it would all be easier if all of tier1 was pure rust; part of this is the reason for having a tier1.





my plans for payment
--------------------

for the "no-profit-to-me" run engines like fal.ai or vertex, offer a paid
caching service that can easily be plugged into thsoe endpoints. the sales
pitch being something like

"oh, your enjoying thumbrella, here are some ways to slash your costs".
1. add a caching layer with an account (free for some usage, cheap for lots)
2. use our cloud service directly, we can run it far cheaper than fal.ai
3. self host with our free open source docker container
3b. or use our professional docker container with more controls and performance

note, signing up for any of the services (1,2,3b) should easily allow switching
or transitioning into the other layers. even "3" may want some optional signup
step where we also offer a limited free caching service also?

for projects that want embedded file pickers or browsers or asset libraries;

4. want to include thumbrella directly into your own product; its open source?
4b. it's open source, go for it
4c. or, lets chat at get it integrated expertly


other "pro level features" provided at that '3b' level
- webhooks, proactively create thumbnails
- redis/postgres caching 
  - this may be desirable for 3.(free self host) also
    - but maybe only simple TTL cache maintentance
- cache maintenance tools
- dashboard and analytics (like when you subscribe with 1. and 2.)

also note on 3. self hosted. I'm expecting this will be less efficient and
possibly slower for many cases, aside from completely internal network use
cases. it definitely won't have the "horizontal scale out" you get from the
cloud hosting. possibly comes "preconfigured" with conservative settings,
assuming it will be run on "potato level hardware".

the 3. and 3b. self hosted containers do not contain any of the code for
multiple tenants. so will need to find a way that can be built in modularly.
perhaps that is the "cloudflare workers" level code that enhances tier 1,
not part of the open source product. maybe we call that the "tier 0" layer
and it has the deeper connections to cloudflare also.
some of that does creep into the "3b. professional self hosting" when we
talk about the dashboards and tracking tools.

interesting note that tier1 and tier2 can be provided as standalone executables.
that may be a more desirable delivery mechanism for people who don't care about
documents/pdf/3d model thumbnails? maybe that is the free platform?
tier3 is going to need a carefully organized docker container with a lot of
complicated dependencies. Perhaps the standalone binary for tier3 is the 
"free to run for yourself" and it gracefully degrades when it can't run the
subtools in whatever environment it is running in...
this seems good. if someone wants some subset of filters on the free tool
they are welcome to put together their environment. even 1 or 2 specific cases
probably aren't too hard to get going. Although running some of these in a
headless environment will likely not be straightforward.

so the free and open source delivery is the tier3 executable.
it does not include the "cloudflare worker integration for tier 0" and
does not include the dashboard account management tools on the pro version.
the pro version also is a docker container.

i'm thinking we may want to build a unique docker container per licensed
user. a couple of things this does; bake the client name into the thumbnail jpgs,
wants some sort of provisioned license key (envvar, secret lookup?).
this is some hashed customer id so I can know who generated them but it 
doesn't leak company info.

another bonus of 2. and beyond is that the runtime can skip tier1 and beyond,
which ends up being less read requests, which may matter for s3 types of
storage that charge per request.

there may be a future where we want to host thumbrella ourselves in popular
cloud datacenters so customers can avoid outgress charges for their data
and only serve the samller thumbnails. (this may need to be a "use 3b for
yourself) type of situation.


buying 3b is something that gives you a license that allows running 
the pro/commercial docker image and any version updates for 1 year.
potentially with some mechanism for security updates for awhile beyond that.

there may also be a "key revoke list" we build into future containers that
deny certain use cases.

when free plans start requesting too many resources we just naturally
slow them down. unless that starts also backing up and then we do proper 429.
(this is only on our tier0 service)


pricing structure

free service
    $0, signup process (or github,google,other account)
    10mb cache (~1,000+ thumbnails, 1 week ttl)
    1k thumbnails per day (on our own provider)
    100 renders per day (tier 3, on our own provider)
    cache works from any provider (fal.ai, etc)
    simple dashboard

fan service
    $0 but star on github (or social media post?) for double?
    20mb cache (~2,000+ thumbnails, 2 week ttl)
    2k thumbnails per day
    200 renders per day
    simple dashboard

easy service
    $4 per month
    200mb cache (6 month ttl)
    100k thumbnails per day (cached is free)
    10k renders per day
    simple dashboard

pro service
    $8 per month
    (triple the flat service amounts)
    scales up at $2 per
    +200mb of cache 
    +100k of thumbnails
    +10k of renders
    dashboards, cache maintenance tools
    cache maintenance and tracking tools
    customize resolutions and settings
    able to set maximum spent on limits
    reduce number of remote read requests (lower s3 reads)

self hosted
    free
    source and/or binary executable
        disk/volume based caching
    provide additional render processes as desired
    customize resolutions and settings

pro hosted
    $200 gets 1 year of upgrades (+1 year security updates)
    keyed docker images
    variety of caching hookups (redis/postgres/cloudflare/aws/disk_volume)
    dashboards, cache maintenance tools
    cache maintenance and tracking tools
    customize resolutions and settings



My thinking is there will be four+ repos where this is managed

- The regular project, written in rust, very easy to grab an executable and self
  host, or get the source and build for yourself (What license?)
- My recipe for building the "tier 3 runtime" docker image. This will probably
  be everything except thumbrella. xvfb environments, usd, and more. I'm
  thinking I'll have this all build on demand, it will be a big and beefy build
  step with a lot of annoying dependencies. The paid commercial service will be
  built on this image. This is also the base I'll use for running on platforms
  like fal.ai, cloudflare, and more
- The cloudflare worker build of tier 1. This adds the account management,
  throttling, platform control features to the tier 1 server. It will also be
  able to build to wasm (something the regular 1. repo probably won't have build
  instructions for?)
- simple dashboard functionality for the pro customers. also tools to chart and
  breakdown and manage the cache usage. also controls to configure the server
  (resolutions, jpeg settings, watermarks?, etc)
- open source client integrations. this may actually be a collections of
  separate repos, forks from existing "web file browsers" and "web media
  libraries" where we either integrate thumbrella, or make it usable as a simple
  embeddable plugin where that makes sense. owncloud, etc. there's also some
  standalone client web component that people can embed into anything that is
  dressed up and ready to thumbrella out of the box.







# Thumbrella Greenfield Architecture Notes

## Design Stance

Assume the existing code is disposable. The real task is to design stable interfaces and execution boundaries that can survive multiple implementations.

That means:

- File intelligence is a first-class product, not just support code for thumbnails.
- The batch streaming model is the primary architecture, not a later extension.
- The fixed thumbnail profile is a product choice, not a technical limitation.
- Tier escalation is part of the core design, not an optimization.
- Fetch, inspect, render, and cache are separate subsystems.

## Business Shape

The business should likely not start as "a single hosted API everyone must buy from directly."

More realistic opening shape:

- distribute on AI platforms to remove adoption friction
- allow free self-hosting to build trust and usage
- monetize later through managed hosting and enterprise support

That matches the product because the hard part is making remote media inspection and thumbnailing actually work reliably, not inventing artificial scarcity.

## Commercial Lanes

### AI-platform lane

Replicate and similar platforms are an adoption channel.

Pros:

- immediate reach
- no infra assembly required by users
- strong demo story
- easy way for developers to try hard media thumbnailing quickly

Cons:

- platform captures hosting margin
- less control over pricing model
- weaker direct customer relationship

Still worth it because it proves demand cheaply.

### Managed lane

thumbrella.dev can become the margin product once the tiered architecture is efficient enough.

The key idea is not just convenience, but better unit economics through:

- lightweight Tier 1 execution
- selective escalation to Tier 2 and Tier 3
- shared cache efficiency

### Enterprise lane

Enterprise value probably comes from operational and compliance features, not from the core engine alone.

Good candidates:

- supported self-host deployment
- internal cache backends
- cache lifecycle tooling
- intranet integrations
- generate-on-upload workflows
- pregeneration and library backfill tools

## Open-Core Bias

An open-core strategy seems more credible than trying to wall off the base product.

Open core should include:

- the core inspection and thumbnail engine
- a reasonable self-host path
- basic cache support

Paid layers should emphasize:

- managed operations
- support
- governance
- enterprise integrations
- operational tooling

## Pricing Intuition

The likely pricing axes are:

- request volume
- heavy-compute tier usage
- retained cache storage
- enterprise support scope

Free tier, if offered, should constrain usage levels rather than making the product feel fake.

## Core Subsystems

The system should be built around six independent but composable subsystems:

- API facade.
- Batch orchestrator.
- Fetch and inspection layer.
- File description and normalization layer.
- Handler registry and renderers.
- Tier client / escalation layer.
- Cache and freshness layer.

If those boundaries are correct, the implementation language or framework for a given tier can change without breaking the model.

## Canonical Internal Types

The first design step is to make the data model explicit.

Suggested types:

- `ThumbnailProfile`
- `ThumbnailBatchRequest`
- `ThumbnailItemRequest`
- `ThumbnailEvent`
- `FileDescription`
- `DescribeRequest`
- `DescribeEvent`
- `SourceRef`
- `SourceMetadata`
- `FetchSession`
- `InspectionReport`
- `InspectionEvidence`
- `HandlerCapability`
- `ThumbnailArtifact`
- `CacheEntry`
- `TierRoute`

These types matter more than the initial code structure.

## Batch First

Even if the public API keeps simple single-item endpoints, the engine should be batch-first.

Why:

- Streaming partial completion is natural.
- Cancellation has one clear owner.
- Cache operations can be tracked per item.
- Escalation decisions are made uniformly.

Single-item endpoints should just wrap a one-item batch.

Each item should be able to request either or both of:

- structured file description
- canonical thumbnail

## Streaming Contract

Prefer NDJSON for the first implementation.

Reasons:

- Easy to emit from almost any backend.
- Easy to proxy.
- Easy to consume from servers, CLIs, and browsers.
- Clean fit for per-item event streams.

Minimum event vocabulary:

- `item.accepted`
- `item.inspecting`
- `item.described`
- `item.cache_hit`
- `item.not_modified`
- `item.escalated`
- `item.result`
- `item.error`
- `item.cancelled`
- `batch.complete`

## Cache Model

The cache is two things, not one:

- Thumbnail artifact storage.
- Freshness metadata storage.

That separation is important because some backends may want cheap metadata reads and delayed blob reads.

Cache mode must be explicit on every request:

- Disabled.
- Read-only.
- Read-write.

Write timing:

- Never block result streaming on cache persistence.
- Do cache writes after the corresponding `item.result` event.

## Freshness Model

Need three distinct outcomes:

- The client already has the current thumbnail.
- The service has a current thumbnail cached.
- The thumbnail must be generated now.

So the system should distinguish:

- `not_modified`
- `cache_hit`
- `generated`

Cache identity should include both source state and renderer state:

- Source validator: ETag, Last-Modified, or fallback fingerprint.
- Description schema version.
- Canonical thumbnail profile version.
- Handler/render pipeline version.

Descriptions and thumbnails can be cached separately but should share source validation records.

## Fetch Layer Requirements

The fetch layer should not be designed as `download(url) -> bytes`.

It needs to support:

- Storage-style metadata discovery similar to S3 HEAD information.
- Metadata-only reads.
- Prefix reads.
- Tail reads.
- Sparse range reads.
- Spill-to-disk when needed.
- Shared read state across multiple inspectors.

The main abstraction should be a seek-oriented source object backed by progressive HTTP access.

## Inspection Strategy

Inspection should be cheap and incremental.

Typical sequence:

1. URL/header metadata.
2. Prefix bytes for magic sniffing.
3. Tail bytes when the format benefits from it.
4. Tool-specific lightweight probes.
5. Full materialization only if a chosen handler requires it.

The describe path should often stop before full materialization.

Normalized description should combine multiple evidence sources:

- transport metadata
- filename and extension hints
- libmagic classification
- ffprobe or media inspection
- format-specific structural reads

The service should preserve provenance so callers can understand whether a fact came from headers, bytes, or deeper inspection.

This is where the efficiency wins live, especially for Tier 2.

## Tier Architecture

### Tier 1

Goal:

- Run in extremely constrained environments.

Role:

- Extract embedded thumbnails.
- Re-encode and crop.
- Produce placeholders.

Hard rule:

- If Tier 1 cannot satisfy the request cheaply, it should escalate instead of growing complex local dependencies.

Open research concern:

Cloudflare Workers is the natural home for Tier 1 but image processing inside Workers is constrained in ways that are not yet proven viable.

- Python via Pyodide is almost certainly too slow.
- Native binaries like `sharp` are not available.
- Cloudflare's own image resizing API costs $0.50 per 1000 images, which is too expensive.
- Pure JS or WASM image processing may be fast enough but must be validated with a real experiment.
- Rust compiled to WASM is likely a strong candidate because the heavy math can run outside JS while still fitting Workers constraints.

If Workers cannot do the image pipeline within its CPU time budget, Tier 1 falls back to a lightweight container on cheap hosting. Workers may still be useful for routing and cache lookups even in the fallback case.

This is one of the first science experiments that needs to run before the Tier 1 deployment model is committed.

### Tier 1: Icon Placeholder Strategy

Font rendering at request time is probably not viable in Workers.

Loading a full icon font per request is too expensive. Even a stripped subset font adds startup cost that competes directly with the CPU budget available for actual image work.

The more robust answer is a prerendered icon set:

- Generate placeholder JPEGs offline for each known MIME family, coarse kind, and file extension.
- Store them in R2 or KV as a static asset bundle.
- Serving a placeholder becomes a key lookup and a byte copy, not a render.
- Fallback for unknown types can be a single generic gray tile.

The icon set becomes a design artifact produced once, not a runtime concern. It can be versioned and updated independently of the Workers code.

The experiment is still worth running to confirm font rendering in WASM is slow enough that the prerendered path is clearly correct, not just probably correct.

### Tier 1: Workers Subrequest Architecture

Workers are billed and killed on CPU time, not wall-clock time. IO wait — fetches, KV reads, R2 reads, subrequests to other services — does not consume the CPU budget at all. A Worker doing a 30ms upstream fetch plus 2ms of actual JS is well within a 10ms CPU limit.

This changes the escalation story materially:

- A Tier 1 Worker can proxy a cache-miss to Tier 2 or Tier 3 and wait for the full response without any CPU cost for the wait.
- The only budget the Tier 1 Worker actually spends is its own JS and WASM execution: cache lookup, routing logic, response marshalling, and any image work on a cache hit.
- Escalation to higher tiers is essentially free from a billing perspective. The user absorbs the latency but the Worker is not penalized for it.

Practical design option:

- A thin routing Worker handles cache lookup and forwards misses directly to Tier 2 or Tier 3.
- On a cache hit, the Worker does a small amount of image work (or just returns the cached artifact) and never escalates.
- On a miss, it proxies and streams the Tier 2/3 response back, writing to cache asynchronously.

Unresolved: whether the image manipulation work on cache hits justifies WASM in the same Worker, or whether even that should be a separate Worker for isolation.

## Tier 1 Image Processing Candidates: Squoosh and Rust-WASM

Google's Squoosh library is a promising candidate for JPEG decode/encode/crop operations in Workers. It is designed for browser and edge environments, compiles to WASM, and is already used in production for image manipulation in the browser.

Rust-to-WASM is another strong candidate and may outperform JS-centric approaches for tight CPU budgets. A Rust path can use focused codec and resize crates, then expose a minimal interface to the Worker runtime.

The benchmark order should be:

- Squoosh first for fastest integration and baseline numbers.
- Rust-WASM second for an optimized path if Squoosh CPU or bundle size is too high.
- Pure JS fallback only if integration simplicity beats performance needs.

The Tier 1 decision should be based on measured CPU milliseconds and bundle size, not implementation preference.

### Tier 2

Goal:

- Be the main service for most real workloads.

Role:

- Handle common media using ffmpeg, ffprobe, Pillow, and lightweight document/image tooling.
- Exploit partial reads and range requests.
- Render the opinionated canonical thumbnail profile.

This is the tier to optimize first.

### Tier 2: Rust and libav

Rust is a strong candidate for the Tier 2 server-side implementation, not just the Tier 1 WASM layer. The libav story in Rust is better than Python in several ways that matter directly to this architecture.

**Bindings**: `ffmpeg-next` and `ffmpeg-sys-next` provide access to the same `libavcodec`, `libavformat`, and `libavutil` stack that Python's PyAV wraps. The difference is that Python adds GIL lock/release cycles and Python object allocation on every frame, packet, and codec call. Rust calls the C API at zero extra cost.

**Custom IO is the real win**: libav's IO layer is pluggable via `AVIOContext`. This is exactly what the partial-read and range-request architecture needs — a seekable HTTP source where libav can call back for specific byte ranges. In Python, implementing a custom `AVIOContext` requires Python callbacks, which add overhead on every read. In Rust, you implement `Read + Seek` and pass a function pointer — zero overhead per read call. The entire buffered-HTTP-as-a-file abstraction becomes essentially free.

**Performance ceiling rises**: things that Python would struggle with — keeping libav's frame pipeline full, doing pixel math between decode and encode steps, managing buffer lifetimes without copies — are non-issues in Rust. Operations that would need numpy tricks or careful PyAV buffer juggling just become normal code.

**Concurrency**: Rust's async can drive multiple libav demux/decode streams without the GIL being a concern at all. Parallel batch items stay independent without fighting for interpreter time.

**WASM split**: for Tier 1 Workers, full FFmpeg compiled to WASM would be several MB and too slow to instantiate. Pure Rust image crates are the right choice there — `zune-jpeg`, `fast_image_resize`, `mozjpeg`. For Tier 2 on a real server, native Rust with dynamically linked libav is the right choice — same library, full capability, no WASM constraints.

This means Rust as a single language can plausibly cover both the Tier 1 WASM pipeline and the Tier 2 media server, with different crate sets and compilation targets rather than different languages.

### Tier 3

Goal:

- Handle expensive or exotic renderers.

Role:

- 3D rendering.
- Heavy document pipelines.
- Anything needing complex containers, GPU-ish stacks, or shell-heavy workflows.

Hard rule:

- Tier 3 should feel like a separate compute product with strict sandboxing, not just “Tier 2 but with more packages.”

## Handler Model

Handlers should be narrow and self-describing.

There are really two handler families:

- describers
- thumbnail renderers

Each handler should declare:

- Supported MIME and extension families.
- Required tier.
- Required source access pattern.
- Whether partial reads are sufficient.
- Whether it emits a final artifact directly or a frame/image to post-process.

Describers should be able to return partial facts without blocking on full parse success.

That keeps dispatch capability-driven.

## Opinionated Output Profile

The public product should stay rigid for now:

- JPEG only.
- 256x204 bounds.
- Low quality.
- Deterministic crop behavior.
- Alpha flattened.
- Metadata stripped.

Internally, still treat that as versioned profile data rather than ad hoc constants.

## Description Contract

The description result should feel like a normalized fusion of `stat`, `file`, and `ffprobe`, plus object-storage style source metadata.

Useful top-level sections:

- `source`
- `classification`
- `storage`
- `media`
- `warnings`
- `evidence`

Potential fields:

- filename
- extension
- byte size
- MIME from transport
- MIME from magic
- normalized kind
- container
- codecs
- width and height
- duration
- frame rate
- page count
- stream summary
- embedded thumbnail presence
- ETag
- Last-Modified
- cache control
- content disposition
- accept-ranges

The contract should prefer normalized values over raw tool dumps, with optional raw evidence preserved for debugging.

## Tooling Notes Worth Testing Later

- ffprobe plus magic for early classification.
- ffmpeg representative frame extraction versus fixed timestamp.
- Whether MP4 tail reads materially reduce transfer for frame extraction.
- SVG and PDF rendering tool choices.
- EXR colorspace handling.
- JPEG quality experiments at 30, 40, and 50.

## Near-Term Planning Order

The design work should probably proceed in this order:

1. Freeze the event model and freshness semantics.
2. Freeze the file description schema.
3. Freeze the canonical thumbnail profile.
4. Define fetch session and source access abstractions.
5. Define describer and renderer contracts plus capability negotiation.
6. Define tier escalation protocol.
7. Only then pick the concrete implementation stack for the first server.