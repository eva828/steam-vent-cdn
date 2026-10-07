# SteamVentCdn

[![Rust](https://img.shields.io/badge/rust-2024-orange.svg)](https://www.rust-lang.org)

Download Steam Workshop items via Steam CDN.

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

## License

MIT
