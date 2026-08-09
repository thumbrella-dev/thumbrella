# Thumbrella Server 1.x changelog

## Development

- Small refinement on tier 3 handoff failure handling

## 1.3.0 - 2026/08/06

Hybrid self hosted servers extended by Thumbrella Cloud

- TBR_TIER2 and TBR_TIER3 can override server builtin handling
- TBR_CACHE can set `cloud:` with a connect string to cache to cloud service
- Caches only store the 'media' instead of full 'result'
- Eliminate duplicate cache reads

## 1.2.0 - 2026/08/01

Small consistency improvements for demo projects and clients.

- Batch size limited to 12 requests
- Streamed intermediates use proper placeholder

## 1.1.0 - 2026/07/26

The major note is that this fixes a bug on the standalone server with the ndjson streaming data. This now matches what was already served (properly) from Thumbrella Cloud. Also continue cleanup of the Result fields.

- Streamed results are not encapsulated in extra object  /tier1  /break
- Ensure ThumbResult `source` sets `cache` for cached lookups  /tier1
- Empty ThumbResult `http_status` uses `null` instead of `0`  /tier1
- Full subprocess output shown when `$TBR_LOG=full`  /tier3
- Apply `$TBR_SCRATCH` more consistently  /tier3
- Remove unused code around Bubblewrap (`bwrap`) sandboxing  /tier3
- Dispatch `av1` and `exr` media to tier3, not tier2  /tier1

## 1.0.0 - 2026/07/22

Initial end of prerelease, starting maintained and active releases.