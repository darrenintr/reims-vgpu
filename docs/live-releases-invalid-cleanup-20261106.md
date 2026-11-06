# Live GitHub Release selection and automatic invalid-version cleanup

The Ventura launcher uses **one live release menu**. On launch it fetches all paginated GitHub Releases, including historical and pre-release tags, and shows their local status:

- **[local - ready]** — a verified complete build already exists in the local QEMU bundle cache. Reims uses that build without downloading it again.
- **[download]** — a GitHub Release contains both the Linux archive and checksum; selecting it installs once, verifies the SHA256, and launches.
- **[source only]** — a published GitHub Release lacks a downloadable Linux build; it is visible but not runnable unless already installed.
- **[INVALID]** — explicitly marked by the owner in `scripts/invalid-releases.json` or locally with `--mark-invalid`; the launcher will not select or start it.

Launching the selector performs automatic invalid-tag synchronization and safe pruning first. This checks the `testing` branch's manifest on every invocation, caches the last validated manifest to survive offline/API failure, and never infers invalidity from age, source-only status, or a superseded version. It deletes only exact complete tagged local build directories that you explicitly mark invalid.

## Mark a version invalid

Tell the assistant which exact release tag has been tested and must no longer be used. The owner can update `scripts/invalid-releases.json` in the `testing` branch:

```json
{
  "schema_version": 1,
  "invalid_tags": ["testing-12-f36b720da5d1"]
}
```

After a push of that manifest, the next time you run `ventura-launch-remote` the tag is marked invalid. If no QEMU instance is running and the marked folder is not the selected installation, its local binaries are removed automatically. Historical GitHub Releases, Stable tags, VM disks and OpenCore images are not deleted.

For device-only invalid tags without updating GitHub:

```bash
~/.local/bin/reims-release --mark-invalid testing-12-f36b720da5d1
~/.local/bin/reims-release --prune-invalid --json
```

Remove a **local-only** mark with `--unmark-invalid TAG`. Remote manifest marks cannot be overridden by this command.

**Safety:** Automatic cleanup does not run while any QEMU VM process is detected. It also will not delete a currently selected installation: switch to a valid build and open the launcher again to clean it. Invalid versions are blocked from launch regardless. Unknown folders, symlinks, partial installs, VM disks and nonmarked versions remain untouched.

## Ubuntu/KDE launcher

```bash
install -Dm755 scripts/reims-release.py "$HOME/.local/bin/reims-release"
install -Dm755 scripts/ventura-launch-remote "$HOME/.local/bin/ventura-launch-remote"
ventura-launch-remote
```

The launcher needs an existing `reims-ctl` Ventura VM (default name `ventura`, override with `VENTURA_VM`). The legacy `~/OSX-KVM-Ventura` direct-QEMU script is not automatically modified.

Inspect all current GitHub Releases with `ventura-launch-remote --list-remote`, offline installed versions with `--list-local`, and pin an exact Release with `--tag TAG`. When the tag is already installed, `--tag` also reuses local bytes and does **not** redownload them.

The GitHub API uses your existing `gh auth` credentials if available, otherwise the anonymous API; rate limits/network failures fall back to locally installed versions. No polling daemon is created: *live* means refreshed at launch, not continuous background monitoring.
