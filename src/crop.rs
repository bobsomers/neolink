//! Which part of the frame is worth publishing.
//!
//! Pointed at a roadway, most of a 4K frame is sky and buildings where no plate
//! will ever be, but everything downstream pays for those pixels anyway. Cropping
//! here, before the frame reaches Syphon, is the cheapest place to stop that:
//! publishing a strip of roadway rather than the whole frame roughly halves the
//! work the recogniser has to do.
//!
//! The crop is held as fractions of the frame rather than pixels, so it does not
//! have to be redone if the camera's resolution changes, and so the web UI can
//! use the same numbers directly as CSS percentages when drawing the guides.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The most that can be taken off any one side
///
/// Generous on purpose: a camera looking down a road from height may want most
/// of the sky gone, so the useful strip can easily be the bottom fifth of the
/// frame. What stops a crop being silly is the pixel minimum below, not this
const MAX_SIDE: f32 = 0.8;
/// The most that can be taken off a pair of opposite sides together
const MAX_PAIR: f32 = 0.9;
/// Smaller than this in either direction and there is nothing left to look at
const MIN_CROP_PIXELS: u32 = 128;

/// How much to take off each side, as a fraction of the whole frame
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Crop {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl Crop {
    /// Whether this crop leaves the frame alone
    pub(crate) fn is_none(&self) -> bool {
        *self == Crop::default()
    }

    /// Check the fractions make sense on their own
    ///
    /// The size they work out to is checked separately, since that needs a frame
    /// to measure against and this has to be usable before one has arrived.
    pub(crate) fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("left", self.left),
            ("right", self.right),
            ("top", self.top),
            ("bottom", self.bottom),
        ] {
            if !value.is_finite() || !(0.0..=MAX_SIDE).contains(&value) {
                return Err(anyhow!(
                    "Crop {name} is {value}, which is outside 0 to {MAX_SIDE}"
                ));
            }
        }
        if self.left + self.right > MAX_PAIR {
            return Err(anyhow!(
                "Crop left and right together take {:.0}%, more than the {:.0}% allowed",
                (self.left + self.right) * 100.0,
                MAX_PAIR * 100.0
            ));
        }
        if self.top + self.bottom > MAX_PAIR {
            return Err(anyhow!(
                "Crop top and bottom together take {:.0}%, more than the {:.0}% allowed",
                (self.top + self.bottom) * 100.0,
                MAX_PAIR * 100.0
            ));
        }
        Ok(())
    }

    /// The part of a frame of this size to keep, as (x, y, width, height)
    ///
    /// Returns an error rather than a tiny rectangle if the crop leaves too
    /// little of the frame to be worth publishing.
    pub(crate) fn region(&self, width: u32, height: u32) -> Result<(u32, u32, u32, u32)> {
        self.validate()?;

        let x = (width as f32 * self.left).round() as u32;
        let y = (height as f32 * self.top).round() as u32;
        let right = (width as f32 * self.right).round() as u32;
        let bottom = (height as f32 * self.bottom).round() as u32;

        // saturating_sub so a crop bigger than the frame reports the size
        // problem below rather than wrapping around
        let kept_width = width.saturating_sub(x + right);
        let kept_height = height.saturating_sub(y + bottom);

        if kept_width < MIN_CROP_PIXELS || kept_height < MIN_CROP_PIXELS {
            return Err(anyhow!(
                "Crop leaves {kept_width}x{kept_height} of {width}x{height}, \
                 smaller than the {MIN_CROP_PIXELS} pixel minimum"
            ));
        }
        Ok((x, y, kept_width, kept_height))
    }
}

/// Where a region's top left corner sits in a frame's bytes
///
/// Its own function because the units are easy to get wrong: `stride` is already
/// in bytes, while `x` is in pixels and has to be multiplied by the four bytes a
/// BGRA pixel takes. Getting that wrong shifts the picture rather than failing.
pub(crate) fn byte_offset(x: u32, y: u32, stride: usize) -> usize {
    y as usize * stride + x as usize * BYTES_PER_PIXEL
}

/// BGRA, which is what the decode pipeline is set to produce
const BYTES_PER_PIXEL: usize = 4;

/// What the web UI and the publisher thread need to agree on
///
/// The publisher runs on its own thread while the UI runs as a task on the
/// runtime, so this is the one thing they share. A plain mutex is enough: the
/// publisher takes it once per frame and nothing else touches it, so it is never
/// contended.
#[derive(Debug)]
pub(crate) struct CropShared {
    /// What to publish, as fractions of the frame
    pub crop: Crop,
    /// The size of the frame before cropping, once one has been seen. The page
    /// needs it to show what a crop works out to in pixels
    pub source: Option<(u32, u32)>,
    /// Which camera this is the crop for, since the page can be looking at
    /// another one
    pub camera: String,
    /// Where the crop is remembered between runs
    pub state_file: PathBuf,
}

/// A handle on the crop, shared between the web UI and the publisher
pub(crate) type CropHandle = Arc<Mutex<CropShared>>;

/// Where the crops for each camera are remembered between runs
///
/// Kept beside the config rather than inside it, so that applying a crop from
/// the web UI never has to rewrite a file the user maintains by hand.
pub(crate) fn state_path(config: &Path) -> PathBuf {
    config.with_extension("crop.toml")
}

/// Read the saved crop for one camera
///
/// A missing or unreadable file means no crop, not a failure: the stream is far
/// more important than the crop, and starting uncropped is always safe.
pub(crate) fn load(path: &Path, camera: &str) -> Crop {
    let saved: HashMap<String, Crop> = match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                log::warn!("Ignoring {}: {e}", path.display());
                return Crop::default();
            }
        },
        Err(_) => return Crop::default(),
    };

    let crop = saved.get(camera).copied().unwrap_or_default();
    if let Err(e) = crop.validate() {
        log::warn!("Ignoring the saved crop for {camera}: {e}");
        return Crop::default();
    }
    if !crop.is_none() {
        log::info!("Loaded a saved crop for {camera} from {}", path.display());
    }
    crop
}

/// Remember the crop for one camera, leaving any others in the file alone
pub(crate) fn save(path: &Path, camera: &str, crop: Crop) -> Result<()> {
    let mut saved: HashMap<String, Crop> = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();

    saved.insert(camera.to_string(), crop);
    let text = toml::to_string_pretty(&saved).context("Could not encode the crop")?;
    std::fs::write(path, text).with_context(|| format!("Could not write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: (u32, u32) = (3840, 2160);

    fn crop(left: f32, right: f32, top: f32, bottom: f32) -> Crop {
        Crop {
            left,
            right,
            top,
            bottom,
        }
    }

    #[test]
    fn no_crop_keeps_the_whole_frame() {
        let region = Crop::default().region(FRAME.0, FRAME.1).unwrap();
        assert_eq!(region, (0, 0, 3840, 2160));
    }

    #[test]
    fn fractions_become_pixels() {
        // A tenth off the left, a twentieth off the right, and most of the sky
        let region = crop(0.1, 0.05, 0.3, 0.2).region(FRAME.0, FRAME.1).unwrap();
        assert_eq!(region, (384, 648, 3264, 1080));
    }

    #[test]
    fn a_strip_of_roadway_is_the_point() {
        let (x, y, width, height) = crop(0.0, 0.0, 0.4, 0.28).region(FRAME.0, FRAME.1).unwrap();
        assert_eq!((x, y, width), (0, 864, 3840));
        assert!((690..=700).contains(&height), "height was {height}");
    }

    #[test]
    fn most_of_the_sky_can_be_cropped_away() {
        // The case the generous per-side limit exists for
        let (_, y, width, height) = crop(0.0, 0.0, 0.75, 0.05).region(FRAME.0, FRAME.1).unwrap();
        assert_eq!((y, width), (1620, 3840));
        assert_eq!(height, 432);
    }

    #[test]
    fn a_side_beyond_the_limit_is_refused() {
        assert!(crop(0.9, 0.0, 0.0, 0.0).validate().is_err());
        assert!(crop(0.0, 0.0, 0.0, -0.1).validate().is_err());
        assert!(crop(f32::NAN, 0.0, 0.0, 0.0).validate().is_err());
    }

    #[test]
    fn opposite_sides_cannot_swallow_the_frame() {
        // Each side is within its own limit, but together they leave too little
        let greedy = crop(0.8, 0.8, 0.0, 0.0);
        assert!(greedy.left <= MAX_SIDE && greedy.right <= MAX_SIDE);
        assert!(greedy.validate().is_err());

        // Exactly at the pair limit is allowed, and 4K has plenty left over
        assert!(crop(0.45, 0.45, 0.45, 0.45)
            .region(FRAME.0, FRAME.1)
            .is_ok());
    }

    #[test]
    fn a_crop_that_leaves_almost_nothing_is_refused() {
        // Legal fractions, but a 200x120 frame has under the pixel minimum left
        let error = crop(0.4, 0.4, 0.4, 0.4).region(400, 300).unwrap_err();
        assert!(
            error.to_string().contains("minimum"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_offset_counts_rows_in_bytes_and_columns_in_pixels() {
        // A 4K frame with no padding: 3840 pixels at four bytes each
        let stride = 3840 * 4;
        assert_eq!(byte_offset(0, 0, stride), 0);
        assert_eq!(byte_offset(10, 0, stride), 40);
        assert_eq!(byte_offset(0, 1, stride), stride);
        assert_eq!(byte_offset(384, 648, stride), 648 * stride + 1536);
    }

    #[test]
    fn the_offset_follows_a_padded_stride() {
        // IOSurface and some decoders pad rows, so the stride is wider than the
        // frame and a row is not width * 4
        let padded = 3840 * 4 + 256;
        assert_eq!(byte_offset(0, 2, padded), 2 * padded);
        assert_eq!(byte_offset(5, 2, padded), 2 * padded + 20);
    }

    #[test]
    fn a_crop_region_and_its_offset_agree() {
        let stride = FRAME.0 as usize * 4;
        let (x, y, _, _) = crop(0.1, 0.05, 0.3, 0.2).region(FRAME.0, FRAME.1).unwrap();
        assert_eq!(byte_offset(x, y, stride), 648 * stride + 384 * 4);
    }

    #[test]
    fn the_state_file_sits_beside_the_config() {
        assert_eq!(
            state_path(Path::new("/etc/neolink.toml")),
            PathBuf::from("/etc/neolink.crop.toml")
        );
    }

    #[test]
    fn saving_then_loading_gives_the_same_crop() {
        let dir = std::env::temp_dir().join(format!("neolink-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("neolink.crop.toml");

        let wanted = crop(0.1, 0.05, 0.3, 0.2);
        save(&path, "FlockOff", wanted).unwrap();
        assert_eq!(load(&path, "FlockOff"), wanted);

        // Another camera in the same file is untouched by the first
        let other = crop(0.0, 0.0, 0.25, 0.25);
        save(&path, "Driveway", other).unwrap();
        assert_eq!(load(&path, "FlockOff"), wanted);
        assert_eq!(load(&path, "Driveway"), other);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_just_means_no_crop() {
        let path = std::env::temp_dir().join("neolink-crop-does-not-exist.toml");
        std::fs::remove_file(&path).ok();
        assert!(load(&path, "FlockOff").is_none());
    }

    #[test]
    fn a_camera_with_nothing_saved_gets_no_crop() {
        let dir = std::env::temp_dir().join(format!("neolink-crop-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("neolink.crop.toml");
        save(&path, "FlockOff", crop(0.1, 0.0, 0.0, 0.0)).unwrap();

        assert!(load(&path, "SomeOtherCamera").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_ignored_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("neolink-crop-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("neolink.crop.toml");
        std::fs::write(&path, "this is not toml {{{").unwrap();

        assert!(load(&path, "FlockOff").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_saved_crop_that_is_out_of_range_is_ignored() {
        let dir = std::env::temp_dir().join(format!("neolink-crop-wild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("neolink.crop.toml");
        std::fs::write(&path, "[FlockOff]\nleft = 0.99\n").unwrap();

        assert!(load(&path, "FlockOff").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
