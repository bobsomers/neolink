use clap::Parser;

/// The ui command serves a small web page for adjusting the camera while it is
/// streaming, without needing an MQTT broker.
///
/// It listens on localhost only, so the controls are not exposed to the network.
#[derive(Parser, Debug)]
pub struct Opt {
    /// Port to serve the UI on
    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,
}
