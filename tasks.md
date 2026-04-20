switch to real http sources and start a closer look at measuring data use
leading into some optimization and efficiency development (partial png, comparing libav settings and flags)
and/or leading to focus on etag and cache handling
specify the input and output data structures, including the "media metadata"
strategy for building the tier 3 docker image with high level media tools
    cog endpoint (python harness)
interconnect between tiers of services
designing tier 0 (actually, just part of tier 1) the admin-related work unrelated to thumbnails
    caching backends
    usage throttling and status codes
    accounts and quotas
    usage monitoring and tracking
batch run review client, allow running on collection of files and review the thumbnails and stats generated
    starts to feel like a lightweight end-to-end test framework, good
try building with optimization flags and see where we're feeling performance-wise
real tier1 wasm build for tier1 and cloudflare wrangler



