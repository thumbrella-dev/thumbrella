# Thumbrella 1.x changelog

## Development

- Ensure ThumbResult `source` uses `cache` for cached lookups  /tier1
- Empty ThumbResult `http_status` uses `null` instead of `0`  /tier1
- Full subprocess output shown when `$TBR_LOG=full`  /tier3
- Apply `$TBR_SCRATCH` more consistently  /tier3
- Remove unused code around Bubblewrap (`bwrap`) sandboxing  /tier3
- Dispatch `av1` and `exr` media to tier3, not tier2  /tier1
- Deployment tools tuned for "release" behaviors


## 1.0.0 - 2026/07/22

Initial end of prerelease, starting maintained and active releases.