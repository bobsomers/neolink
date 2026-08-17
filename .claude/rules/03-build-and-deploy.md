# Build and Deploy

## Standard Build

```bash
cargo build              # Debug build
cargo build --release    # Release build (optimized)
```

## Cross-Compilation for Jetson (ARM64)

```bash
cross build --target aarch64-unknown-linux-gnu --release
```

`Cross.toml` in the project root configures the cross-compilation environment. This is the primary deployment target for the ALPR project (NVIDIA Jetson).

## Docker Build

```bash
docker build -t neolink .
```

The Dockerfile uses multi-stage builds. Additional Docker configurations are in the `docker/` directory.

## System Dependencies

GStreamer development libraries are required for the RTSP feature (enabled by default):

- `gstreamer1.0-dev`
- `libgstrtspserver-1.0-dev`
- `gstreamer1.0-plugins-base`
- `gstreamer1.0-plugins-good`
- `gstreamer1.0-plugins-bad`
- `gstreamer1.0-plugins-ugly`

## Feature Flags

- `gstreamer` (default) - Enables RTSP server via GStreamer
- `syphon` (default) - Publishes decoded video as a macOS Syphon server. Its
  dependencies are declared macOS-only and the module is also gated on
  `target_os = "macos"`, so Linux/Jetson builds resolve and compile normally,
  just without the subcommand
- `ui` (default) - Loopback-only web UI for zoom and IR control
- `pushnoti` (optional) - Enables FCM push notification listener

Build with specific features:
```bash
cargo build --features "gstreamer,pushnoti"
cargo build --no-default-features  # Minimal build without RTSP
```

## Checks and Linting

```bash
cargo fmt -- --check     # Format check
cargo clippy             # Lint
cargo test               # Run tests
```

## Supply Chain Security

```bash
cargo deny check         # Run cargo-deny with deny.toml configuration
```

## Release Artifacts

The build produces a single binary (`neolink` or `neolink.exe`). No additional runtime files are needed beyond the configuration TOML and system GStreamer libraries.

## Deployment to ALPR

For the joshkautz/alpr project:

1. Cross-compile for aarch64: `cross build --target aarch64-unknown-linux-gnu --release`
2. Deploy binary to `/home/jetson/alpr/bin/neolink` on the Jetson
3. Configuration file: `neolink.toml` (based on `sample_config.toml`)
4. Managed by systemd service: `neolink.service` (set up by ALPR's setup scripts)

## Configuration

See `sample_config.toml` for all available configuration options. The configuration is TOML-based and supports:

- Multiple cameras with per-camera settings (name, address, credentials, stream selection)
- MQTT integration (broker, topics, Home Assistant discovery)
- TLS configuration
- User/password management
- Bind address and port for RTSP server
