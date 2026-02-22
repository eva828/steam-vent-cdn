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
steam-vent-cdn --username <user> --password <pass> <APP_ID> <ITEM_ID>

# Specify output directory
steam-vent-cdn -o ./output <APP_ID> <ITEM_ID>

# List files only (no download)
steam-vent-cdn --list <APP_ID> <ITEM_ID>

# Increase parallel downloads
steam-vent-cdn --parallel-download 32 <APP_ID> <ITEM_ID>
```

## License

MIT
