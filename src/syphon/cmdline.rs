use clap::{Parser, ValueEnum};
use neolink_core::bc_protocol::StreamKind;

/// The syphon command decodes the camera stream and publishes it as a macOS
/// Syphon server, so that Syphon-aware applications can read the video frames
/// directly without going through RTSP.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera to publish. Must be a name in the config
    pub camera: String,
    /// Which stream to publish
    #[arg(short, long, value_enum, default_value_t = Stream::Main)]
    pub stream: Stream,
    /// Name to advertise the Syphon server under
    ///
    /// Defaults to the camera name
    #[arg(short, long)]
    pub name: Option<String>,
    /// Number of complete messages to buffer from the camera
    #[arg(long, default_value_t = 100)]
    pub buffer: usize,
    /// Also serve the camera control UI, so zoom and IR can be adjusted while
    /// the stream is running
    #[cfg(feature = "ui")]
    #[arg(long)]
    pub ui: bool,
    /// Port to serve the control UI on
    #[cfg(feature = "ui")]
    #[arg(long, default_value_t = 8080)]
    pub ui_port: u16,
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
