# App depot download

Design notes for `steam-vent-cdn app` and `--layout steam`, kept in sync with the
implementation. Historical planning notes live in the commit history.

## CLI

```
steam-vent-cdn [common] item <APP_ID> <ITEM_ID> [...]
steam-vent-cdn [common] app  <APP_ID> [--os <OS>] [--language <LANG>] [--manifest <DEPOT>:<MANIFEST>]... [--shared-depots <skip|auto>]
```

Common: `-o/--output`, `-u/--username`, `-p/--password`, `--parallel-download`,
`--server-mode`, `--layout <depot|steam>`, `-v/--verbose`.

`--os` defaults to the host OS and fails with an explicit error when the host is
not one of `windows`, `macos` or `linux`; `--os all` disables the filter.
`--language` defaults to `english`; `all` disables the filter.

## Data sources

- `CMsgClientPICSProductInfoRequest` with `single_response`, read through
  `job_multi` (the reply is only complete once `response_pending` is cleared;
  `job` would stop at the first, possibly empty, message). An empty buffer can
  precede the real one, so the last non-empty buffer wins.
- Transient network errors are retried with the configured `RetryPolicy`.
- Relevant appinfo keys: `common.name`, `config.installdir`,
  `depots.branches.public.buildid` / `timeupdated`,
  `depots.<id>.manifests.public.gid` / `size` (both the nested object and the
  legacy scalar form), `depots.<id>.config.{oslist,language,lowviolence}`,
  `depots.<id>.dlcappid`, `depots.<id>.depotfromapp`.

## Depot selection

1. `lowviolence` depots are always skipped: they are regional alternatives to
   the regular content.
2. A depot with a `language` list is kept only when it contains the selected
   language; depots without `language` hold shared content and are kept.
3. `oslist` is matched case-insensitively; a missing `oslist` means every
   platform.
4. `--manifest` overrides force a depot in even when the filters dropped it.
   Unknown depot ids and duplicate overrides are errors.
5. The surviving order is the appinfo order. Depots are downloaded and written
   in that order, so a later depot overwrites files of an earlier one; the
   number of such overlapping files is logged once per app. Steam's own mount
   order is not modelled.

## Layouts

`--layout depot`: `output/<APP_ID>/<BUILD_ID>/<DEPOT_ID>/...`.

`--layout steam`:

- `steamapps/common/<INSTALLDIR>/` plus `steamapps/appmanifest_<APP_ID>.acf`
- `steamapps/workshop/content/<APP_ID>/<ITEM_ID>/` plus
  `steamapps/workshop/appworkshop_<APP_ID>.acf`

Shop items delivered as a web file (`file_url`) are not supported in the Steam
layout, because Steam expects them as workshop content.

## Shared depots

A depot whose appinfo entry has `depotfromapp` is provided by another app and is
not downloaded as part of the consuming app. `--shared-depots skip` (default)
prints the command that installs the owning app; `auto` walks the owners.

The walk is breadth first, one layer at a time, so the depots several apps link
to can be merged into one request for the owner before it is downloaded:

- root app: every selected depot,
- owner app: only the depot ids the consuming apps linked (regardless of the
  owner's own `oslist`/`language`), merged per layer,
- a depot that the owner does not list is an error,
- the walk is bounded by `MAX_SHARED_DEPOT_DEPTH` as a cycle guard,
- already installed owners are not downloaded twice (within one run),
- app manifests are written only after the whole walk succeeded, so a failed
  owner download cannot leave a "fully installed" consumer behind,
- an owner installs only the linked depots, so its manifest is written with
  `StateFlags` 2 (update required): the app itself is not fully installed.

`SharedDepots` is written for skipped depots too (Steam's own manifests do the
same); the consuming app's `StateFlags` is only `4` (fully installed) when no
shared depot is missing, otherwise `2` (update required).

## ACF generation

Existing files are read, merged key by key and written back, so `UserConfig`,
`MountedConfig` and unknown keys survive. `InstalledDepots` accumulates across
runs; depots left over from a previous run are reported once.

- `SizeOnDisk`: sum of the merged `InstalledDepots` sizes (uncompressed).
- `BytesToDownload` / `BytesDownloaded`: compressed size of the manifests that
  were downloaded in this run (`chunks[].compressed_size`).
- `StateFlags`: `4` fully installed, `2` when shared depots are missing.
- `LastOwner` / `subscribedby`: 0 for anonymous sessions.

The files are best effort: Steam may re-validate them and download missing
content again.

## VDF

`src/downloader/vdf.rs` reads both binary KeyValues and the text format used by
ACF/appinfo. Text input is detected from the first non-whitespace byte; unknown
escapes keep their backslash so merged files round-trip, and nesting is capped at
`MAX_DEPTH`.

## Path safety

`src/path_safety.rs` rejects absolute paths, drive letters, `.`/`..` components
and NUL characters for every name that is joined onto the output directory:
manifest entries (during decode), the PICS install directory, workshop file
names and, as a last line of defence, the file writer.

## Out of scope

- beta branches and `encryptedmanifests`
- resuming or verifying partially downloaded content
- manifest signature verification
- full ACF state coverage
