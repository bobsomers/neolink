//! The Syphon half of the syphon subcommand.
//!
//! Owns the Metal device, the IOSurfaces the frames are copied into, and the
//! Syphon server itself. `MetalContext` is not `Send`, so a `Publisher` must be
//! created and used entirely on one thread.

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop};
use objc2_io_surface::IOSurfaceLockOptions;
use objc2_metal::{MTLCommandBuffer, MTLCommandQueue, MTLTexture};
use syphon_core::SyphonServer;
use syphon_metal::{IOSurface, IOSurfacePool, MetalContext};

/// Number of surfaces to cycle through.
///
/// Triple buffering means we are not writing into a surface that Syphon may
/// still be reading from for a previous frame.
const RING_SIZE: usize = 3;

/// How long to let the run loop run each time it is pumped
const RUN_LOOP_PUMP_SECONDS: f64 = 0.002;

/// Let the main thread's run loop process Syphon's discovery traffic.
///
/// Syphon servers announce themselves once at startup, but a client that starts
/// later asks for an announce and expects a reply. Those requests arrive as
/// distributed notifications, which are delivered on the **main** thread's run
/// loop, so this has to be called from there and not from the publisher thread.
///
/// Without it neolink is only discoverable by clients that were already running
/// when it started, which is the wrong way round for a long lived publisher.
pub(super) fn pump_run_loop() {
    // A zero timeout returns before the run loop can service an incoming mach
    // message, so give it a small real interval
    unsafe {
        CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, RUN_LOOP_PUMP_SECONDS, false);
    }
}

/// One reusable destination for a frame
struct Slot {
    surface: IOSurface,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
}

/// Publishes BGRA frames to a Syphon server
pub(super) struct Publisher {
    server: SyphonServer,
    ctx: MetalContext,
    ring: Vec<Slot>,
    next: usize,
    width: u32,
    height: u32,
}

impl Publisher {
    /// Create a Syphon server of the given name and size
    pub(super) fn new(name: &str, width: u32, height: u32) -> Result<Self> {
        let ctx = MetalContext::system_default()
            .ok_or_else(|| anyhow!("No Metal device is available for Syphon"))?;

        // The pool pre-allocates the surfaces. We hold on to all of them for
        // the lifetime of the publisher and cycle through them, so each surface
        // only ever needs its Metal texture created once
        let mut pool = IOSurfacePool::new(width, height, RING_SIZE);
        let mut ring = Vec::with_capacity(RING_SIZE);
        for _ in 0..RING_SIZE {
            let surface = pool
                .acquire()
                .ok_or_else(|| anyhow!("Could not allocate an IOSurface for Syphon"))?;
            let texture = ctx
                .create_texture_from_iosurface(&surface, width, height)
                .ok_or_else(|| anyhow!("Could not create a Metal texture from an IOSurface"))?;
            ring.push(Slot { surface, texture });
        }

        let server = SyphonServer::new(name, width, height)
            .map_err(|e| anyhow!("Could not create the Syphon server: {e:?}"))?;

        log::info!(
            "Publishing Syphon server '{}' at {}x{}",
            name,
            width,
            height
        );

        Ok(Self {
            server,
            ctx,
            ring,
            next: 0,
            width,
            height,
        })
    }

    /// The size this publisher was built for
    pub(super) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether anything is currently reading from this server
    pub(super) fn has_clients(&self) -> bool {
        self.server.has_clients()
    }

    /// Copy one BGRA frame into the next surface and publish it
    ///
    /// `src_stride` is the number of bytes per row in `src`, which is not
    /// necessarily `width * 4`.
    pub(super) fn publish(&mut self, src: &[u8], src_stride: usize) -> Result<()> {
        let height = self.height as usize;
        let row_bytes = self.width as usize * 4;

        if src_stride < row_bytes {
            return Err(anyhow!(
                "Source stride {} is too small for a {} pixel wide frame",
                src_stride,
                self.width
            ));
        }
        if src.len() < (height - 1) * src_stride + row_bytes {
            return Err(anyhow!(
                "Frame is {} bytes, too small for {}x{}",
                src.len(),
                self.width,
                self.height
            ));
        }

        let slot = &self.ring[self.next];
        self.next = (self.next + 1) % self.ring.len();

        // IOSurface rows are padded for alignment, so this is generally wider
        // than the frame itself and the copy has to go row by row
        let dst_stride = slot.surface.bytes_per_row();
        if dst_stride < row_bytes {
            return Err(anyhow!(
                "IOSurface stride {} is too small for a {} pixel wide frame",
                dst_stride,
                self.width
            ));
        }

        unsafe {
            let ret = slot
                .surface
                .lock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
            if ret != 0 {
                return Err(anyhow!("Could not lock the IOSurface: {ret}"));
            }

            let base = slot.surface.base_address().as_ptr() as *mut u8;
            for row in 0..height {
                // syphon-core publishes with `flipped: false`, but decoded video
                // is top down, so write the rows in reverse. This costs nothing
                // since the frame is being copied anyway, and it saves having to
                // flip on the GPU or patch the binding
                let src_row = src.as_ptr().add(row * src_stride);
                let dst_row = base.add((height - 1 - row) * dst_stride);
                std::ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
            }

            let ret = slot
                .surface
                .unlock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
            if ret != 0 {
                log::warn!("Could not unlock the IOSurface: {ret}");
            }
        }

        let command_buffer = self
            .ctx
            .queue()
            .commandBuffer()
            .ok_or_else(|| anyhow!("Could not create a Metal command buffer"))?;

        unsafe {
            let texture_ptr = Retained::as_ptr(&slot.texture) as *mut AnyObject;
            let command_buffer_ptr = Retained::as_ptr(&command_buffer) as *mut AnyObject;
            self.server
                .publish_metal_texture(texture_ptr, command_buffer_ptr);
        }

        // syphon-core only encodes into the command buffer, it does not submit
        // it, so nothing reaches a client until we commit here
        command_buffer.commit();

        Ok(())
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        log::debug!("Stopping the Syphon server");
        self.server.stop();
    }
}
