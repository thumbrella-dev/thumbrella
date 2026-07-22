# npmjs Release Workspace

This directory is for maintainers building and publishing Thumbrella npm
packages.

User-facing package documentation lives in `README.release.md` and is mirrored
into `packages/server/README.md` for npm.

## Package Structure

- `packages/server` - meta package (`@thumbrella/server`)
- `packages/server-linux-x64-gnu` - Linux x64 glibc binary package
- `packages/server-win32-x64-msvc` - Windows x64 binary package
- `scripts/` - staging and release helper scripts

## Scripts

From this directory (`release/npmjs`):

- `npm run readme:sync`
  - Copy `README.release.md` into `packages/server/README.md`.
- `npm run stage:from-local`
  - Stage binaries from local build outputs in `target/`.
- `npm run stage:from-release -- --tag v1.0.0`
  - Stage binaries from GitHub release assets.
- `npm run pack:all`
  - Run `npm pack --dry-run` for linux, windows, and meta packages.
- `npm run readme:sync`
  - Copy `README.release.md` into `packages/server/README.md`.

## Provenance-First Manual Flow

1. Stage from a published GitHub release:
   - `npm run stage:from-release -- --tag v0.5.1`
2. Sync package README from user-facing release README:
   - `npm run readme:sync`
3. Validate package contents:
   - `npm run pack:all`
4. Publish in order, `npm publish --access public:
   - `packages/server-linux-x64-gnu`
   - `packages/server-win32-x64-msvc`
   - `packages/server`

## Notes

- Keep versions aligned across all package.json files.
- First publish of scoped public packages requires `--access public`.
- If a binary is missing, target package dry-run pack may still succeed,
  but that package should not be published.
