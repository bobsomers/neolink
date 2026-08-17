# Neolink - Claude Code Context

## Project Overview

Neolink is an RTSP bridge for Reolink IP cameras. It translates the proprietary Baichuan protocol (used by Reolink cameras on TCP port 9000) into standard RTSP, allowing any RTSP-compatible client to view camera streams without relying on Reolink's cloud services or proprietary apps.

This is a fork of [QuantumEntangledAndy/neolink](https://github.com/QuantumEntangledAndy/neolink), maintained by joshkautz with stability improvements for 24/7 unattended operation. It is the camera bridge used by the [joshkautz/alpr](https://github.com/joshkautz/alpr) license plate recognition project, deployed on NVIDIA Jetson hardware.

## Workspace Structure

```
neolink (root)          - Main binary crate (CLI, subcommands, RTSP/MQTT servers)
crates/core             - neolink_core: protocol library (~21k LOC), Baichuan protocol implementation
crates/decoder          - neolink_decoder: media decoding utilities
crates/pushnoti         - Push notification listener (FCM-based)
crates/mailnoti         - Email notification support
```

## Architecture

### Main Binary (src/)

- `main.rs` - Entry point, CLI dispatch via clap
- `cmdline.rs` - Clap command definitions for all subcommands
- `config.rs` - TOML configuration parsing
- `rtsp/` - GStreamer RTSP server, per-camera stream factories
- `mqtt/` - MQTT client with Home Assistant auto-discovery, per-camera topic handlers
- `common/` - NeoReactor (central coordinator), CamThread (per-camera connection management), instance management
- `battery/`, `pir/`, `ptz/`, `talk/`, `statusled/`, `reboot/`, `users/`, `services/`, `image/` - Subcommand implementations

### Core Library (crates/core / neolink_core)

- `bc_protocol/` - High-level camera API (BcCamera struct), connection management, authentication
- `bc/` - Low-level protocol structures, message types, XML message bodies, serialization
- `bcmedia/` - Media stream parsing (H264/H265 NAL units, AAC audio frames)
- `bcudp/` - UDP transport layer for P2P/relay connections

### Connection Lifecycle

RTSP client connects to Neolink -> Neolink initiates Baichuan TCP connection to camera on port 9000 -> negotiates AES-128 CFB encryption -> authenticates with camera credentials -> requests video stream -> repackages H264/H265 NAL units as RTSP stream -> serves to client.

## Subcommands

`rtsp`, `mqtt`, `mqtt-rtsp`, `image`, `battery`, `pir`, `ptz`, `talk`, `statusled`, `reboot`, `users`, `services`

## Baichuan Protocol

Reverse-engineered binary protocol used by Reolink cameras:

- TCP port 9000, with UDP variant for P2P/relay
- Message format: magic bytes + message ID + body length + XML body
- AES-128 CFB encryption after initial handshake
- Media packets contain H264/H265 NAL units and AAC audio
- Wireshark Lua dissector in `dissector/baichuan.lua` for debugging
- Protocol documentation in `dissector/protocol.md`, `messages.md`, `mediapacket.md`, `udp.md`

## Configuration

TOML-based configuration. See `sample_config.toml` for all options. Supports per-camera settings, MQTT integration, TLS, user management.

## Build Commands

```bash
# Standard build
cargo build
cargo build --release

# Cross-compile for ARM64 (Jetson)
cross build --target aarch64-unknown-linux-gnu --release

# Docker
docker build -t neolink .

# Checks
cargo fmt -- --check
cargo clippy
cargo test
```

## Dependencies

- **GStreamer** - RTSP server (gstreamer1.0-dev, libgstrtspserver-1.0-dev)
- **Tokio** - Async runtime
- **rumqttc** - MQTT client
- **quick-xml** + serde - XML serialization for protocol messages
- **AES** - Baichuan protocol encryption
- **clap** - CLI argument parsing
- **jemalloc** - Memory allocator (non-MSVC platforms)

## Feature Flags

- `gstreamer` (default) - RTSP server support
- `syphon` (default) - Publish decoded video as a macOS Syphon server. Its dependencies
  are macOS only and the module is additionally gated on `target_os = "macos"`, so
  non-macOS builds simply omit the subcommand rather than failing
- `ui` (default) - Loopback-only web UI for adjusting zoom and IR while streaming
- `pushnoti` (optional) - FCM push notification listener

## Cross-Compilation

`Cross.toml` configures cross-compilation for ARM64 targets. Used for Jetson deployment in the ALPR project.

## Docker

Multi-stage Dockerfile for containerized builds. Additional Docker configurations in `docker/`.

## Supply Chain Security

`deny.toml` configures cargo-deny for dependency auditing.

## Relationship to ALPR

This project provides the camera bridge for [joshkautz/alpr](https://github.com/joshkautz/alpr). The compiled binary is deployed to `/home/jetson/alpr/bin/neolink` on Jetson hardware, managed by a systemd service, reading configuration from `neolink.toml`.

## Debugging

- `RUST_LOG=debug` or `RUST_LOG=neolink=trace` for verbose logging
- `GST_DEBUG=3` or higher for GStreamer debug output
- Wireshark dissector at `dissector/baichuan.lua` for protocol-level debugging
