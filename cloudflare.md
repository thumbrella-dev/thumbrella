Taking advantage of Cloudflare's workers is the key to reducing cost to less
than what other providers can charge. But the trick is that this can only run
a subset of the simple requests.

We'll also take advantage of other Cloudflare features to make this the single
entry point and do billing, quotas, and management. (KV or other features)


- Tier 1 runs as regular workers. These will be wasm code built from rust.
- Tier 2 runs as container workers (docker). These run as native code and will
contain the libav dependencies.
- Tier 3 will be a heavyweight docker image that likely runs on other platforms
like Replicate. 


Cloudflare features that should be high value

- Tier 2 container workers can be totally private, not on the public internet
but still have a route to the general workers. With a route setup cloudflare
runs the workers on the same host (no latency overhead)
- Workers have a cache that is per-data-region. It is free to access and store
information there. Shared between workers (both simple and containered).
Eviction and order of operations are not reliable.
- Worker fetch requests (and subworker requests) are not billed against the
workers usage. This means subbing out to things like Replicate are free for the worker.
- Regular workers run in an isolate that shared 128mb, although if workers fill
that isolate a new one is spun up for non-aggressive overflows.
- KV and D1 can be accessed from external compute
- KV ttl deletes are free, making a good source for resource usage limiting
- Pages serves pregenerated placeholders for cdn fast access
- For throttling/usage limitations use durable objects for syncronized real time

Other notes

Worker containers on the "lite" instance get 6000 minutes of runtime per month.
The ram/disk for keeping the server active costs around $1.73 per month. Then the
cpu usage on top of that would be $3.24 for 100% cpu the entire month. Cloudflare
can spin up additional instances of the container when overloaded. Containers
can also sleep which is free and has faster spinup time than a cold start; about 
1 second, versus 1-3 seconds for a cold startup.

