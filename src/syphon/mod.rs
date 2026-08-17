///
/// # Neolink Syphon
///
/// This module decodes a camera's video stream and republishes it as a macOS
/// Syphon server, so Syphon-aware applications can read raw frames without
/// going through RTSP.
///
/// The camera half is the same as the `frame-stats` and `image` subcommands:
/// H264/H265 frames are pulled straight off the Baichuan connection. Those are
/// decoded with VideoToolbox and each decoded frame is copied into an
/// IOSurface-backed Metal texture and published.
///
/// Buffering is kept to an absolute minimum throughout. The RTSP path this
/// replaces accumulates and cycles seconds of video, which is where its latency
/// comes from, so nothing here is allowed to queue frames up.
///
/// # Usage
///
/// ```bash
/// neolink syphon --config=config.toml CameraName
/// neolink syphon --config=config.toml --stream sub --name "Front Door" CameraName
/// ```
///
use anyhow::{anyhow, Result};
use gstreamer_app::AppSink;
use gstreamer_video::{prelude::VideoFrameExt, VideoFrameRef, VideoInfo};
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{BcMedia, BcMediaIframe, BcMediaPframe},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::channel;

mod cmdline;
mod gst;
mod publisher;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;
use publisher::Publisher;

/// How often to log the publishing rate
const LOG_EVERY: Duration = Duration::from_secs(10);

/// Entry point for the syphon subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    // With --ui the control page runs beside the stream in the one process,
    // the same way the mqtt-rtsp subcommand runs two services together
    #[cfg(feature = "ui")]
    if opt.ui {
        let (ui_port, ui_reactor) = (opt.ui_port, reactor.clone());
        return tokio::select! {
            v = publish(opt, reactor) => v,
            v = crate::ui::serve(ui_port, ui_reactor) => v,
        };
    }

    publish(opt, reactor).await
}

/// Decode the camera stream and publish it over Syphon
async fn publish(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;
    let stream: StreamKind = opt.stream.into();
    let server_name = opt.name.clone().unwrap_or_else(|| opt.camera.clone());
    let buffer_size = opt.buffer;
    let name = opt.camera.clone();

    log::info!("{}: publishing {} over Syphon", name, stream);

    let (tx, mut rx) = channel(100);

    let thread_camera = camera.clone();
    tokio::task::spawn(async move {
        thread_camera
            .run_task(|cam| {
                let tx = tx.clone();
                Box::pin(async move {
                    let mut data = cam.start_video(stream, buffer_size, false).await?;
                    while let Ok(frame) = data.get_data().await {
                        match frame? {
                            BcMedia::Iframe(BcMediaIframe {
                                video_type, data, ..
                            })
                            | BcMedia::Pframe(BcMediaPframe {
                                video_type, data, ..
                            }) => {
                                if tx.send((video_type, data)).await.is_err() {
                                    // Publisher has finished
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Result::Ok(())
                })
            })
            .await
    });

    // The codec is only known once a frame turns up, and the pipeline needs it
    // to pick a parser, so wait for the first frame before building anything
    let (video_type, first_frame) = rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("Camera sent no video frames"))?;

    let pipeline = gst::DecodePipeline::new(video_type)?;

    // The Metal objects are not Send, so everything Syphon related is created
    // and used on this one thread. Same convention as the rtsp sender thread
    let sink = pipeline.sink.clone();
    let thread_name = format!("{name}::syphon");
    let publisher_thread = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || publish_loop(sink, server_name))?;

    gst::push_frame(&pipeline.source, &first_frame)?;
    while let Some((_, data)) = rx.recv().await {
        if !gst::push_frame(&pipeline.source, &data)? {
            break;
        }
        // This loop is driven by the runtime's block_on, so it runs on the
        // process main thread, which is the only place Syphon's discovery
        // notifications get delivered
        publisher::pump_run_loop();
    }

    log::info!("{}: camera stream ended", name);
    let _ = pipeline.source.end_of_stream();
    pipeline.stop();
    match publisher_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::error!("Syphon publisher failed: {e:?}"),
        Err(_) => log::error!("Syphon publisher thread panicked"),
    }

    Ok(())
}

/// Pull decoded frames and publish each one
///
/// Runs until the pipeline finishes.
fn publish_loop(sink: AppSink, server_name: String) -> Result<()> {
    let mut publisher: Option<Publisher> = None;
    let mut published: u64 = 0;
    let mut since_log: u64 = 0;
    let mut last_log = Instant::now();

    loop {
        let sample = match sink.pull_sample() {
            Ok(sample) => sample,
            Err(e) => {
                log::debug!("Decoded frames finished: {e:?}");
                break;
            }
        };

        match publish_sample(&sample, &mut publisher, &server_name) {
            Ok(()) => {
                published += 1;
                since_log += 1;
            }
            // A bad frame should not take the whole stream down
            Err(e) => log::warn!("Could not publish a frame: {e:?}"),
        }

        if last_log.elapsed() >= LOG_EVERY {
            let has_clients = publisher.as_ref().map(|p| p.has_clients()).unwrap_or(false);
            log::info!(
                "Published {} frames ({:.1} fps), clients connected: {}",
                published,
                since_log as f64 / last_log.elapsed().as_secs_f64(),
                has_clients,
            );
            since_log = 0;
            last_log = Instant::now();
        }
    }

    log::info!("Syphon publisher stopping after {} frames", published);
    Ok(())
}

/// Publish a single decoded sample, creating or resizing the server as needed
fn publish_sample(
    sample: &gstreamer::Sample,
    publisher: &mut Option<Publisher>,
    server_name: &str,
) -> Result<()> {
    let caps = sample
        .caps()
        .ok_or_else(|| anyhow!("Decoded sample has no caps"))?;
    let info = VideoInfo::from_caps(caps)?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| anyhow!("Decoded sample has no buffer"))?;
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|e| anyhow!("Could not map the decoded frame: {e:?}"))?;

    // Take the geometry from the frame rather than the caps, since the buffer
    // may carry a video meta that overrides what the caps advertise
    let (width, height) = (frame.width(), frame.height());

    // The server is sized to the video, which is only known once the first
    // frame has been decoded. Rebuild it if the camera changes resolution
    let needs_rebuild = match publisher.as_ref() {
        Some(p) => p.dimensions() != (width, height),
        None => true,
    };
    if needs_rebuild {
        if publisher.is_some() {
            log::info!(
                "Video size changed to {}x{}, restarting server",
                width,
                height
            );
        }
        // Drop the old one first so its server stops before the new one starts
        *publisher = None;
        *publisher = Some(Publisher::new(server_name, width, height)?);
    }

    let publisher = publisher
        .as_mut()
        .ok_or_else(|| anyhow!("Syphon publisher is missing"))?;

    let stride = frame.info().stride()[0] as usize;
    let data = frame
        .plane_data(0)
        .map_err(|e| anyhow!("Could not read the decoded frame: {e:?}"))?;

    publisher.publish(data, stride)
}
