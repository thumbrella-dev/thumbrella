# winget packaging

Manifests for distributing the Windows `thumbrella.exe` server through the
Windows Package Manager (winget).

The `manifests/` tree mirrors the layout used by the
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) community
repository so it can be copied straight into a fork for submission:

```
manifests / <letter> / <publisher> / <app> / <version> / <files>
```

For us that is `manifests/t/Thumbrella/Server/<version>/`.

## Decisions made here

- **PackageIdentifier:** `Thumbrella.Server`
- **Publisher:** `Thumbrella`
- **PackageName:** `Thumbrella Server`
- **InstallerType:** `zip` with `NestedInstallerType: portable`
- **InstallerUrl:** the `thumbrella-<tag>-windows-x86_64.zip` GitHub release
  asset. winget extracts the archive and creates a `thumbrella` command shim.
- **License:** `Apache-2.0`
- **ManifestVersion:** `1.12.0` (current schema)

## Per-release workflow

1. Run `release/winget/update-manifest.sh <tag>` (from the repo root this is
   `release/winget/update-manifest.sh v1.4.0`). The script:
   - downloads the Windows archive from the GitHub release,
   - verifies build provenance with `gh attestation verify`,
   - computes `InstallerSha256`,
   - writes the three manifest files for that version.
2. Validate and install-test on Windows (in a `winget-pkgs` sandbox or any
   Windows machine):

   ```
   winget validate release/winget/manifests/t/Thumbrella/Server/1.4.0
   winget install --manifest release/winget/manifests/t/Thumbrella/Server/1.4.0
   ```

3. Fork `microsoft/winget-pkgs`, copy the new version folder into
   `manifests/t/Thumbrella/Server/`, and open a pull request. The
   wingetbot validates the manifest, runs Defender + multi-AV scans and an
   install/uninstall test, then a moderator approves it and it is published.

There is **no ISV registration step** for the community repository: you author
the manifest and submit the PR. There is an optional, separate publisher
verification that adds a verified badge next to your publisher name, but it is
not required to ship.

## Naming

winget's `PackageIdentifier` is two segments: `Publisher.Application`. The first
segment is the publisher (also shown as the `Publisher` field) and the second is
the specific package. `Thumbrella.Server` therefore means publisher "Thumbrella",
package "Server", and leaves room for sibling packages such as
`Thumbrella.Client` or `Thumbrella.Desktop` under the same
`manifests/t/Thumbrella/` publisher folder. The human-readable `PackageName`
("Thumbrella Server") is independent of the identifier.

## Attestation

The release workflow (`thumbrella/.github/workflows/release.yml`) already
attests both the raw binary and the archive with
`actions/attest-build-provenance@v2` (Sigstore). Anyone can verify:

```
gh attestation verify thumbrella-v1.3.6-windows-x86_64.zip --repo thumbrella-dev/thumbrella
```

winget does **not** have a manifest field for Sigstore/SLSA provenance. What
winget pins is the `InstallerSha256` (integrity of the exact download) plus the
rule that `InstallerUrl` must come directly from the ISV's release location
(GitHub releases qualifies). The two mechanisms are complementary:

- the GitHub attestation proves the archive came from this repo's release
  workflow (build provenance),
- the `InstallerSha256` in the PR-reviewed manifest proves the winget client
  downloaded exactly that attested archive (integrity).

`SignatureSha256` exists in the schema but only applies to Authenticode-signed
MSIX/MSI installers, not `portable`.

The raw `thumbrella.exe` inside the zip is also attested (`release.yml` attests
`target/release/thumbrella.exe` and the archive as separate subjects). This
does not add anything winget can describe today, but it matters if we later
point `InstallerUrl` directly at the `.exe`: `update-manifest.sh` could then
verify provenance against the `.exe` rather than the zip. Either way the hash
winget pins is over whatever file the manifest references.

## What ships in the package

winget's user-facing metadata is the manifest: `winget show` renders
`License`, `LicenseUrl`, `ShortDescription`, `Description`, `ReleaseNotesUrl`,
`PublisherUrl`, and `PackageUrl`. Files placed inside a portable archive are
extracted to an obscure location
(`%LOCALAPPDATA%\Microsoft\WinGet\Packages\Thumbrella.Server_<hash>\`) and are
effectively invisible to users.

With that in mind:

- `README.md` and `thumbrella.png` add no value inside the winget package. The
  README content belongs in the manifest `Description`/`ReleaseNotesUrl`, and
  portable packages have no icon field.
- `LICENSE` is worth keeping: Apache-2.0 requires recipients to receive a copy
  of the license, and shipping it in the archive means it lands on disk beside
  the binary. `LicenseUrl` in the manifest is a weaker substitute.

The existing release zip (`thumbrella.exe` + `LICENSE` + `README.md` +
`thumbrella.png`) is used **as-is**: winget supports portable zip installers and
will extract the archive and shim `thumbrella.exe`. That is the chosen approach
here and requires no release workflow changes. Slimming the archive to just
`thumbrella.exe` + `LICENSE` is optional cosmetic cleanup (the extra files are
invisible to users anyway), and it would only affect winget; the npm staging
script extracts just `thumbrella.exe`, so it is unaffected.

## Improvements to consider

- **Publish the raw `.exe` as a release asset** so the package is a true
  "single file" download instead of a zip. In `release.yml`, add the binary to
  the `gh release create` asset list (next to the two archives) and point
  `InstallerUrl` at `thumbrella-<tag>-windows-x86_64.exe`. If we do this, keep
  `LICENSE` available out of band via the manifest `License`/`LicenseUrl`, or
  keep the zip as the license carrier.
- **Authenticode-sign `thumbrella.exe`** (e.g. Azure Trusted Signing). This does
  not change the manifest (portable installers have no `SignatureSha256`), but
  it improves SmartScreen/Defender reputation and lowers the chance of the
  winget validation pipeline flagging the binary.
