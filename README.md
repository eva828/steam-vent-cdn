# SteamVentCdn

Download Steam Workshop items and app depot contents via Steam CDN.

This project is a high-level application built on top of the
[steam-vent](https://codeberg.org/steam-vent/steam-vent) library.

## Installation

```bash
cargo install --git https://github.com/eva828/SteamVentCdn.git
```

## Usage

```bash
# Download a Workshop item
steam-vent-cdn item <APP_ID> <ITEM_ID>

# Download the depot contents of an app
steam-vent-cdn app <APP_ID>

# Download with authentication
# (password is prompted securely; avoid passing it on the command line)
steam-vent-cdn -u <user> item <APP_ID> <ITEM_ID>

# Credentials can also be provided via environment variables
# (CLI arguments take precedence; without a username the tool connects anonymously)
STEAM_USERNAME=<user> STEAM_PASSWORD=<pass> steam-vent-cdn app <APP_ID>

# Specify output directory
steam-vent-cdn -o ./output app <APP_ID>

# List available Workshop versions
steam-vent-cdn item <APP_ID> <ITEM_ID> --list-versions

# Download depots for a specific platform (defaults to the current OS)
steam-vent-cdn app <APP_ID> --os windows

# Pin a depot to a specific manifest (can be given multiple times)
steam-vent-cdn app <APP_ID> --manifest <DEPOT_ID>:<MANIFEST_ID>

# Increase parallel downloads
steam-vent-cdn --parallel-download 32 app <APP_ID>
```

## Output layout

The default `--layout depot` keeps one directory per depot:

- Workshop items: `output/<APP_ID>/<ITEM_ID>/<TIMESTAMP>/...`
- Apps: `output/<APP_ID>/<BUILD_ID>/<DEPOT_ID>/...`

`--layout steam` reproduces the Steam client layout and writes ACF files:

- Apps: `output/steamapps/common/<INSTALLDIR>/` with `output/steamapps/appmanifest_<APP_ID>.acf`
- Workshop: `output/steamapps/workshop/content/<APP_ID>/<ITEM_ID>/` with
  `output/steamapps/workshop/appworkshop_<APP_ID>.acf`

When several depots provide the same path they are merged in mount order, so the
later depot wins (as Steam does); a warning is logged.

### Shared depots

Some depots are provided by another app (for example the Steamworks Common
Redistributables). They are not downloaded by default; the command needed to
install them is printed instead. Pass `--shared-depots auto` to resolve and
download the owning app automatically (this can be significantly larger).

## Status

Implemented:

- Workshop item download (depot layout and Steam layout)
- App depot download with platform filtering (`--os`)
- Per-depot manifest override (`--manifest DEPOT_ID:MANIFEST_ID`)
- Anonymous and authenticated sessions, including `STEAM_USERNAME` / `STEAM_PASSWORD`
- Steam client compatible layout with generated ACF files (`--layout steam`)
- Shared depots: skipped with the command needed to install them, or fetched with
  `--shared-depots auto`

Not yet implemented:

- Beta branches and password protected manifests (`encryptedmanifests`)
- Resuming or verifying partially downloaded content
- Full ACF state coverage (generated ACF files are best effort)

## Limitations

- App downloads only support the `public` branch and the depots whose manifest is
  published for the requesting account. Owned apps require logging in; anonymous
  sessions can only download free apps.
- Password-protected branches (`encryptedmanifests`) are not supported.
- Generated ACF files are best effort. Steam may re-validate them and download
  missing content again.

## Notes

This project was developed with the help of AI.

## License

MIT
