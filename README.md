# SteamVentCdn

Download Steam Workshop items via Steam CDN.

This project is a high-level application built on top of the
[steam-vent](https://codeberg.org/steam-vent/steam-vent) library.

## Installation

```bash
cargo install --git https://github.com/eva828/SteamVentCdn.git
```

## Usage

```bash
# Basic download
steam-vent-cdn <APP_ID> <ITEM_ID>

# Download with authentication
# (password is prompted securely; avoid passing it on the command line)
steam-vent-cdn --username <user> <APP_ID> <ITEM_ID>

# Credentials can also be provided via environment variables
# (CLI arguments take precedence; without a username the tool connects anonymously)
STEAM_USERNAME=<user> STEAM_PASSWORD=<pass> steam-vent-cdn <APP_ID> <ITEM_ID>

# Specify output directory
steam-vent-cdn -o ./output <APP_ID> <ITEM_ID>

# List available versions
steam-vent-cdn --list-versions <APP_ID> <ITEM_ID>

# Increase parallel downloads
steam-vent-cdn --parallel-download 32 <APP_ID> <ITEM_ID>
```

## Status

Implemented:

- Workshop item download
- Anonymous and authenticated sessions, including `STEAM_USERNAME` / `STEAM_PASSWORD`

In progress (branch `feat/app-download`):

- App depot download with platform filtering (`--os`)
- Per-depot manifest override (`--manifest DEPOT_ID:MANIFEST_ID`)
- Steam client compatible layout with generated ACF files (`--layout steam`)
- Shared depot handling (`--shared-depots skip|auto`)

## Notes

This project was developed with the help of AI.

## License

MIT
