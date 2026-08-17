use clap::{Parser, ValueEnum};
use neolink_core::bc_protocol::StreamKind;

/// The frame-stats command measures frame arrival timing directly from the
/// camera, with no RTSP server and no GStreamer in the pipeline.
///
/// Use it to work out whether stream lag and stalls originate in the camera
/// connection itself or in the RTSP serving layer.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera to measure. Must be a name in the config
    pub camera: String,
    /// Which stream to measure
    #[arg(short, long, value_enum, default_value_t = Stream::Main)]
    pub stream: Stream,
    /// How long to measure for, in seconds
    #[arg(short, long, default_value_t = 30)]
    pub duration: u64,
    /// Seconds between progress reports. Set to 0 to disable them
    #[arg(short, long, default_value_t = 5)]
    pub interval: u64,
    /// A gap between frames longer than this many milliseconds counts as a stall
    #[arg(long, default_value_t = 250)]
    pub stall_ms: u64,
    /// Number of complete messages to buffer from the camera
    ///
    /// This is the same buffer the rtsp subcommand requests, so leaving it at
    /// the default measures what rtsp would see
    #[arg(long, default_value_t = 100)]
    pub buffer: usize,
}

/// Which camera stream to pull
#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Stream {
    /// The HD stream
    Main,
    /// The SD stream
    Sub,
    /// The balanced stream, if the camera supports it
    Extern,
}

impl From<Stream> for StreamKind {
    fn from(value: Stream) -> Self {
        match value {
            Stream::Main => StreamKind::Main,
            Stream::Sub => StreamKind::Sub,
            Stream::Extern => StreamKind::Extern,
        }
    }
}
