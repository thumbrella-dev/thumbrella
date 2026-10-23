# Thumbrella Server 2.x changelog

## Development


## 2.1.1 - 2026/10/23

Cache updating and freshness handling

- Update cache expirations on 304 responses
- Default freshness ttl is 5 minutes, even with etag or last modified

## 2.1.0 - 2026/10/22

Cleanup reponse urls so clients can match results to requests

- result urls match the request urls, no longer sanitized
- html requests provide a proper document result with placeholder

## 2.0.1 - 2026/10/14

Fix 2.0 regressions around handoffs and fallback behavior

- Fix handoff logic regressions

## 2.0.0 - 2026/10/12

Support for pinned urls and major version for ffmpeg

- Update ffmpeg 9 and other Rust dependencies
- Add `pin` to results with url path for sharable image link
- Use $TBR_PIN to control pinning lifetime, or disable
- Config envvar paths support envvar and tilde expansion
- Placeholder images served as cachable static media
- Rename $TBR_ALLOW_LOCAL to $TBR_LOCAL
- Enable Tower server concurrency limits
- Packaging information for `cargo binstall`

## Summary of 1.x Changes  2026/07/22 - 2026/10/06

- Fix cache freshness and revalidation on updated responses
- Propagate entries across multiple cache backends
- Internal cache entries better track lifetimes
- Cleanup absolute file path logic for Windows
- Cloud cache fixes ttl values to agree with cloud service
- Update ffmpeg 8.1.3 (on linux), rustls, and other indirect dependencies
- Simplify cache configuration strings
- Add 'service' subcommand to generate service configurations
- External processes drop priveledges and use restrictions
- Compiler flags reduce build by 35% (about 9MB smaller on Linux)
- Support pdf, with or without embedded thumbnails
- Add optional 'pages' property for documents
- Add rendered border estimating page count for documents
- Server cli new 'thumb' mode writes thumbnail file
- Server cli new 'result' subcommand replaces the old 'thumb'
- Different view angle for 3d renders for better "front" representation
- Fix regression for files with wrong extension
- Cleanup the media properties for 3d geometries (now empty)
- Simplify User-Agent to 'thumbrella/major.minor'
- TBR_TIER2 and TBR_TIER3 can override server builtin handling
- TBR_CACHE can set `cloud:` with a connect string to cache to cloud service
- Batch size limited to 12 requests
- Streamed intermediates use proper placeholder
- Ensure ThumbResult `source` sets `cache` for cached lookups  /tier1
- Empty ThumbResult `http_status` uses `null` instead of `0`  /tier1
- Full subprocess output shown when `$TBR_LOG=full`  /tier3
