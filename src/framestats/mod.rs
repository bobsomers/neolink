///
/// # Neolink Frame Stats
///
/// This module measures the timing of video frames as they arrive from the
/// camera over the Baichuan protocol. It deliberately contains no RTSP server
/// and no GStreamer pipeline, so it observes the stream at the point *before*
/// any of the serving machinery in `rtsp/` touches it.
///
/// The intent is to bisect stream lag and stalls. Two clocks are compared:
///
/// - the wall clock at the moment each frame lands in neolink
/// - the camera's own timestamp carried in each frame
///
/// If the camera's timestamps are evenly spaced but arrivals are not, the
/// jitter is being introduced between the camera and here. If both are even,
/// the camera connection is healthy and anything the user sees must come from
/// the RTSP layer downstream.
///
/// # Usage
///
/// ```bash
/// neolink frame-stats --config=config.toml CameraName
/// neolink frame-stats --config=config.toml --stream sub --duration 60 CameraName
/// ```
///
use anyhow::Result;
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{BcMedia, BcMediaIframe, BcMediaPframe},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::channel;

mod cmdline;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;

/// A single video frame as it was received
struct Frame {
    /// When the frame landed in neolink
    at: Instant,
    /// The camera's own timestamp in microseconds. This is a u32 and wraps
    /// roughly every 71 minutes, so always compare with `wrapping_sub`
    cam_us: u32,
    /// Size of the encoded frame
    len: usize,
    /// Whether this was a key frame
    iframe: bool,
}

/// What the camera said about the stream, if it told us
#[derive(Default)]
struct StreamInfo {
    width: u32,
    height: u32,
    fps: u8,
}

/// Entry point for the frame-stats subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;
    let stream: StreamKind = opt.stream.into();
    let buffer_size = opt.buffer;
    let name = opt.camera.clone();

    log::info!(
        "{}: measuring {} for {}s (no rtsp, no gstreamer)",
        name,
        stream,
        opt.duration
    );

    // The camera task pushes frames over this channel. It is generously sized
    // so that this harness never becomes the bottleneck it is trying to measure
    let (tx, mut rx) = channel(2048);

    let thread_camera = camera.clone();
    tokio::task::spawn(async move {
        thread_camera
            .run_task(|cam| {
                let tx = tx.clone();
                Box::pin(async move {
                    let mut data = cam.start_video(stream, buffer_size, false).await?;
                    while let Ok(frame) = data.get_data().await {
                        // Timestamp as early as possible so the measurement
                        // reflects arrival, not our own processing
                        let at = Instant::now();
                        if tx.send((at, frame?)).await.is_err() {
                            // Collector has finished
                            break;
                        }
                    }
                    Result::Ok(())
                })
            })
            .await
    });

    let mut frames: Vec<Frame> = Vec::new();
    let mut audio_packets: usize = 0;
    let mut info: Option<StreamInfo> = None;

    let started = Instant::now();
    let deadline = tokio::time::Instant::from_std(started + Duration::from_secs(opt.duration));
    let mut next_report = started;
    let mut reported_frames = 0usize;

    loop {
        let received = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            v = rx.recv() => v,
        };

        let (at, media) = match received {
            Some(v) => v,
            None => {
                log::warn!("Camera stopped sending frames before the measurement finished");
                break;
            }
        };

        match media {
            BcMedia::Iframe(BcMediaIframe {
                microseconds, data, ..
            }) => frames.push(Frame {
                at,
                cam_us: microseconds,
                len: data.len(),
                iframe: true,
            }),
            BcMedia::Pframe(BcMediaPframe {
                microseconds, data, ..
            }) => frames.push(Frame {
                at,
                cam_us: microseconds,
                len: data.len(),
                iframe: false,
            }),
            BcMedia::Aac(_) | BcMedia::Adpcm(_) => audio_packets += 1,
            BcMedia::InfoV1(i) => {
                info = Some(StreamInfo {
                    width: i.video_width,
                    height: i.video_height,
                    fps: i.fps,
                })
            }
            BcMedia::InfoV2(i) => {
                info = Some(StreamInfo {
                    width: i.video_width,
                    height: i.video_height,
                    fps: i.fps,
                })
            }
        }

        // Periodic progress so a long run is not a silent wait
        if opt.interval > 0 && at.duration_since(next_report) >= Duration::from_secs(opt.interval) {
            let window = frames.len() - reported_frames;
            log::info!(
                "{}: +{:.0}s  {} frames  ({:.1} fps in the last {}s)",
                name,
                at.duration_since(started).as_secs_f64(),
                frames.len(),
                window as f64 / opt.interval as f64,
                opt.interval,
            );
            reported_frames = frames.len();
            next_report = at;
        }
    }

    report(&name, stream, &frames, audio_packets, info.as_ref(), &opt);

    Ok(())
}

/// Print the measurement summary
fn report(
    name: &str,
    stream: StreamKind,
    frames: &[Frame],
    audio_packets: usize,
    info: Option<&StreamInfo>,
    opt: &Opt,
) {
    println!();
    println!("=== Frame timing: {} / {} ===", name, stream);

    if frames.len() < 2 {
        println!(
            "Only {} video frames arrived, which is not enough to measure.",
            frames.len()
        );
        println!("Check the camera name, credentials and that the stream exists.");
        return;
    }

    let first = &frames[0];
    let last = &frames[frames.len() - 1];
    let wall_elapsed = last.at.duration_since(first.at).as_secs_f64();
    // wrapping_sub keeps this correct across the u32 microsecond rollover
    let cam_elapsed = last.cam_us.wrapping_sub(first.cam_us) as f64 / 1_000_000.0;

    let iframes = frames.iter().filter(|f| f.iframe).count();
    let total_bytes: usize = frames.iter().map(|f| f.len).sum();

    println!(
        "Measured {:.1}s, {} video frames ({} I-frames), {} audio packets",
        wall_elapsed,
        frames.len(),
        iframes,
        audio_packets
    );
    if let Some(i) = info {
        println!("Camera reports: {}x{} @ {} fps", i.width, i.height, i.fps);
    }
    println!(
        "Measured rate: {:.2} fps",
        (frames.len() - 1) as f64 / wall_elapsed
    );

    // Arrival intervals: the jitter as neolink actually experiences it
    let mut arrival: Vec<f64> = Vec::with_capacity(frames.len());
    // Camera intervals: the pacing the camera intended
    let mut camera: Vec<f64> = Vec::with_capacity(frames.len());
    for pair in frames.windows(2) {
        arrival.push(pair[1].at.duration_since(pair[0].at).as_secs_f64() * 1000.0);
        camera.push(pair[1].cam_us.wrapping_sub(pair[0].cam_us) as f64 / 1000.0);
    }

    print_intervals("Arrival interval (wall clock, ms)", &arrival);
    print_intervals("Camera timestamp interval (ms)", &camera);

    // Stalls
    let stall_threshold = opt.stall_ms as f64;
    let stalls: Vec<(f64, f64)> = arrival
        .iter()
        .enumerate()
        .filter(|(_, gap)| **gap > stall_threshold)
        .map(|(i, gap)| {
            (
                frames[i + 1].at.duration_since(first.at).as_secs_f64(),
                *gap,
            )
        })
        .collect();

    println!();
    println!("Stalls (gap > {}ms): {}", opt.stall_ms, stalls.len());
    for (at, gap) in stalls.iter().take(20) {
        println!("  at +{:>6.1}s   {:.0}ms", at, gap);
    }
    if stalls.len() > 20 {
        println!("  ... and {} more", stalls.len() - 20);
    }

    // Drift is the headline number. If neolink keeps pace with the camera this
    // stays near zero. If it grows, frames are backing up before RTSP is
    // ever involved
    println!();
    println!(
        "Drift (wall elapsed - camera elapsed): {:+.2}s over {:.1}s",
        wall_elapsed - cam_elapsed,
        wall_elapsed
    );
    println!(
        "Throughput: {:.2} Mbit/s, mean frame {:.1} KB",
        (total_bytes as f64 * 8.0) / wall_elapsed / 1_000_000.0,
        total_bytes as f64 / frames.len() as f64 / 1024.0
    );

    println!();
    println!("How to read this:");
    println!("  Camera intervals even + arrivals even + drift ~0");
    println!("    -> the camera connection is healthy, look downstream in rtsp/");
    println!("  Camera intervals even + arrivals spiky");
    println!("    -> jitter is being added between the camera and here");
    println!("  Drift growing steadily");
    println!("    -> neolink is not keeping up with the camera, independent of rtsp");
}

/// Print mean/min/max and percentiles for a set of intervals
fn print_intervals(title: &str, values: &[f64]) {
    if values.is_empty() {
        return;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mean = values.iter().sum::<f64>() / values.len() as f64;

    println!();
    println!("{}:", title);
    println!(
        "  mean {:>7.1}   min {:>7.1}   max {:>7.1}",
        mean,
        sorted[0],
        sorted[sorted.len() - 1]
    );
    println!(
        "  p50  {:>7.1}   p95 {:>7.1}   p99 {:>7.1}",
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99)
    );
}

/// Nearest-rank percentile of an already sorted slice
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}
