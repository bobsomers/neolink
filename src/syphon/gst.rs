//! The decode half of the syphon subcommand.
//!
//! Takes the H264/H265 NAL units the camera sends and turns them into raw BGRA
//! frames using VideoToolbox hardware decoding, following the `appsrc` pattern
//! already used by `src/image/gst.rs`.
//!
//! The buffering here is deliberately minimal. The RTSP path this subcommand
//! exists to replace accumulates seconds of video and cycles it, which is the
//! source of the lag we are trying to remove, so nothing in this pipeline is
//! allowed to queue up frames.

use anyhow::{anyhow, Context, Result};
use gstreamer::{parse::launch_full, prelude::*, Caps, ParseFlags, Pipeline, State};
use gstreamer_app::{AppSink, AppSrc, AppStreamType};
use neolink_core::bcmedia::model::VideoType;

/// How much encoded video the source is allowed to hold.
///
/// Enough for a couple of key frames so a brief hiccup does not tear the
/// stream, but nowhere near the seconds of backlog the RTSP path keeps.
const SOURCE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A running decode pipeline
pub(super) struct DecodePipeline {
    pipeline: Pipeline,
    /// Encoded frames from the camera go in here
    pub(super) source: AppSrc,
    /// Decoded BGRA frames come out here
    pub(super) sink: AppSink,
}

impl DecodePipeline {
    /// Build a decode pipeline for the given codec and start it playing
    pub(super) fn new(format: VideoType) -> Result<Self> {
        gstreamer::init()
            .context("Unable to start gstreamer, ensure it and all plugins are installed")?;

        let parser = match format {
            VideoType::H264 => "h264parse",
            VideoType::H265 => "h265parse",
        };

        // vtdec_hw is VideoToolbox hardware decoding. decodebin is a portable
        // fallback in case it is unavailable
        let mut last_err = None;
        let mut built = None;
        for decoder in ["vtdec_hw", "decodebin"] {
            let launch_str = format!(
                "appsrc name=thesource \
                 ! {parser} \
                 ! {decoder} \
                 ! videoconvert \
                 ! appsink name=thesink"
            );
            log::debug!("Trying pipeline: {}", launch_str);
            match launch_full(&launch_str, None, ParseFlags::empty()) {
                Ok(element) => {
                    log::info!("Decoding {} with {}", format_name(format), decoder);
                    built = Some(element);
                    break;
                }
                Err(e) => {
                    log::debug!("Could not build pipeline with {decoder}: {e}");
                    last_err = Some(e);
                }
            }
        }

        let element = built.ok_or_else(|| {
            anyhow!(
                "Unable to build a decode pipeline, ensure gstreamer plugins are installed: {:?}",
                last_err
            )
        })?;

        let pipeline = element.dynamic_cast::<Pipeline>().map_err(|_| {
            anyhow!(
                "Unable to create gstreamer pipeline, ensure all gstreamer plugins are installed"
            )
        })?;

        let source = pipeline
            .by_name("thesource")
            .ok_or_else(|| anyhow!("Pipeline is missing its appsrc"))?
            .dynamic_cast::<AppSrc>()
            .map_err(|_| anyhow!("Cannot find appsrc, check your gstreamer plugins"))?;

        let sink = pipeline
            .by_name("thesink")
            .ok_or_else(|| anyhow!("Pipeline is missing its appsink"))?
            .dynamic_cast::<AppSink>()
            .map_err(|_| anyhow!("Cannot find appsink, check your gstreamer plugins"))?;

        source.set_is_live(false);
        source.set_block(false);
        source.set_max_bytes(SOURCE_MAX_BYTES);
        source.set_stream_type(AppStreamType::Stream);
        source.set_format(gstreamer::Format::Time);
        // Let the source stamp buffers as they are pushed. The camera's own
        // timestamps are not needed since we hand every frame straight on
        source.set_do_timestamp(true);

        // Syphon publishes BGRA, and so do the IOSurfaces we copy into, so ask
        // the pipeline for BGRA and avoid a second conversion of our own
        sink.set_caps(Some(
            &Caps::builder("video/x-raw").field("format", "BGRA").build(),
        ));
        // Only ever hold the newest frame. Stale realtime video is worthless,
        // so drop rather than queue
        sink.set_max_buffers(1);
        sink.set_drop(true);
        // Publish frames as soon as they decode rather than pacing to a clock
        sink.set_sync(false);

        pipeline
            .set_state(State::Playing)
            .context("Could not start the decode pipeline")?;

        Ok(Self {
            pipeline,
            source,
            sink,
        })
    }

    /// Stop the pipeline and release its resources
    pub(super) fn stop(&self) {
        if let Err(e) = self.pipeline.set_state(State::Null) {
            log::warn!("Error shutting down the decode pipeline: {e:?}");
        }
    }
}

impl Drop for DecodePipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Push one encoded frame into the pipeline
///
/// Returns `false` if the pipeline has finished and no more frames should be sent.
pub(super) fn push_frame(source: &AppSrc, data: &[u8]) -> Result<bool> {
    // If the source is already full then the decoder is not keeping up. Drop
    // the frame instead of stalling the camera reader behind it
    if source.current_level_bytes() >= source.max_bytes() {
        log::warn!("Decoder is behind, dropping a frame");
        return Ok(true);
    }

    let mut buf = gstreamer::Buffer::with_size(data.len())
        .map_err(|e| anyhow!("Could not allocate a gstreamer buffer: {e:?}"))?;
    {
        let buf_mut = buf
            .get_mut()
            .ok_or_else(|| anyhow!("Could not get a mutable buffer reference"))?;
        let mut buf_data = buf_mut
            .map_writable()
            .map_err(|e| anyhow!("Could not map buffer writable: {e:?}"))?;
        buf_data.copy_from_slice(data);
    }

    match source.push_buffer(buf) {
        Ok(_) => Ok(true),
        Err(gstreamer::FlowError::Flushing) | Err(gstreamer::FlowError::Eos) => {
            log::debug!("Decode pipeline has finished");
            Ok(false)
        }
        Err(e) => Err(anyhow!("Error pushing to the decoder: {e:?}")),
    }
}

/// Human readable codec name for logging
fn format_name(format: VideoType) -> &'static str {
    match format {
        VideoType::H264 => "H264",
        VideoType::H265 => "H265",
    }
}
