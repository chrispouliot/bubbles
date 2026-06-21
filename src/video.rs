//! Video thumbnail decoding — extract a single frame from a video file and
//! return tightly-packed RGBA pixels, scaled so the longer edge ≤ `max_edge`.
//!
//! Uses GStreamer to decode the video, sample at ~1 second, and scale.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt;

use crate::image::{DecodedRgba, ImageLoadError};

/// Initialise gstreamer and register the static gtk4paintablesink plugin,
/// both exactly once per process.
#[allow(dead_code)]
pub(crate) fn ensure_gst_init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // If already initialised, init() returns an error — ignore it.
        let _ = gst::init();
        // Same for the static plugin — calling it twice emits the
        // GStreamer-CRITICAL warning about factory assertions.
        let _ = gstgtk4::plugin_register_static();
    });
}

/// Extract the EXIF-style image-orientation (1..=8) from a gstreamer `TagList`.
/// Returns `None` if the tag is absent, or the value is not in the valid 1..=8
/// range. The orientation spec is the same as JPEG EXIF: 1=identity,
/// 2=mirror-horiz, 3=180, 4=mirror-vert, 5..=8=combos with 90° rotations.
///
/// The gstreamer tag stores the orientation as a string from a fixed set:
/// `"rotate-0"` → 1, `"flip-rotate-0"` → 2, `"rotate-180"` → 3,
/// `"flip-rotate-180"` → 4, `"flip-rotate-270"` → 5, `"rotate-90"` → 6,
/// `"flip-rotate-90"` → 7, `"rotate-270"` → 8.
#[allow(dead_code)]
pub fn extract_orientation(tag_list: &gst::TagList) -> Option<u8> {
    use gst::tags::ImageOrientation;

    let tag_value = tag_list.get::<ImageOrientation>()?;
    let orientation_str = tag_value.get();

    match orientation_str {
        "rotate-0" => Some(1),
        "flip-rotate-0" => Some(2),
        "rotate-180" => Some(3),
        "flip-rotate-180" => Some(4),
        "flip-rotate-270" => Some(5),
        "rotate-90" => Some(6),
        "flip-rotate-90" => Some(7),
        "rotate-270" => Some(8),
        _ => None,
    }
}

/// Decode a single frame from the video at `path`, scale it so the longer side
/// is ≤ `max_edge`, and return tightly-packed RGBA pixels.
///
/// The frame is sampled at approximately 1 second into the video.
///
/// # Errors
///
/// Returns [`ImageLoadError::FileNotFound`] if the path does not exist.
/// Returns [`ImageLoadError::DecodeFailed`] if the file cannot be decoded.
#[allow(dead_code)]
pub fn decode_video_thumbnail_rgba(
    path: &Path,
    max_edge: u32,
) -> Result<DecodedRgba, ImageLoadError> {
    // Must check file existence before touching gstreamer so that a missing
    // file returns ImageLoadError::FileNotFound, not a DecodeFailed wrapping
    // a gstreamer I/O error.
    std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ImageLoadError::FileNotFound
        } else {
            ImageLoadError::DecodeFailed(e.to_string())
        }
    })?;

    ensure_gst_init();

    // Build pipeline elements.
    let src = gst::ElementFactory::make("filesrc")
        .name("src")
        .build()
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot create filesrc: {e}")))?;
    let decodebin = gst::ElementFactory::make("decodebin")
        .name("decodebin")
        .build()
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot create decodebin: {e}")))?;
    let videoconvert = gst::ElementFactory::make("videoconvert")
        .name("videoconvert")
        .build()
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot create videoconvert: {e}")))?;
    // NOTE: The `videoorientation` gstreamer element is not available in all
    // build environments (it's an interface, not a real element factory).
    // Instead, the Rust-side fix at the bottom of this function reads the
    // `image-orientation` tag from the gstreamer stream metadata (captured
    // during the bus message loop below) and applies the existing
    // `apply_orientation` helper from `src/image.rs` to the decoded RGBA
    // pixels. This approach is portable, testable, and does not depend on
    // any gstreamer plugin being present.
    let videoscale = gst::ElementFactory::make("videoscale")
        .name("videoscale")
        .build()
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot create videoscale: {e}")))?;
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name("capsfilter")
        .build()
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot create capsfilter: {e}")))?;
    let appsink_elem = gst::ElementFactory::make("appsink")
        .name("sink")
        .build()
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot create appsink: {e}")))?;
    appsink_elem.set_property("sync", false);
    let appsink = appsink_elem
        .clone()
        .dynamic_cast::<gst_app::AppSink>()
        .map_err(|_| ImageLoadError::DecodeFailed("cannot cast appsink to AppSink".into()))?;

    // Configure properties.
    src.set_property("location", path.to_string_lossy().as_ref());

    let caps = gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(max_edge as i32)
        .height(max_edge as i32)
        .build();
    capsfilter.set_property("caps", &caps);

    // Build pipeline.
    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&src, &decodebin, &videoconvert, &videoscale, &capsfilter, &appsink_elem])
        .map_err(|_| ImageLoadError::DecodeFailed("cannot add elements to pipeline".into()))?;

    // Static link: filesrc ! decodebin
    src.link(&decodebin)
        .map_err(|_| ImageLoadError::DecodeFailed("cannot link src -> decodebin".into()))?;

    // Static link: videoconvert ! videoscale ! capsfilter ! appsink
    gst::Element::link_many([&videoconvert, &videoscale, &capsfilter, &appsink_elem])
        .map_err(|_| ImageLoadError::DecodeFailed("cannot link video post-process chain".into()))?;

    // Dynamic link: decodebin -> videoconvert (decodebin's src pads appear
    // at runtime when it discovers the stream type).
    let vc = videoconvert.clone();
    decodebin.connect_pad_added(move |_db, pad| {
        let sink_pad = vc.static_pad("sink").expect("videoconvert has sink pad");
        if !sink_pad.is_linked() {
            let _ = pad.link(&sink_pad);
        }
    });

    // Transition to PAUSED and wait for pre-roll (this lets us seek before
    // going to PLAYING).
    pipeline
        .set_state(gst::State::Paused)
        .map_err(|_| ImageLoadError::DecodeFailed("set_state(Paused) failed".into()))?;

    let bus = pipeline.bus().unwrap();
    // Capture the image-orientation tag (if any) from tag messages posted on
    // the bus during pre-roll.  This tag carries the EXIF-style rotation
    // (1..=8) that we apply in Rust below — see [`extract_orientation`].
    let mut orientation: Option<u8> = None;

    // Wait for pre-roll (PAUSED) or error/EOS.
    loop {
        let msg = bus
            .timed_pop(gst::ClockTime::from_seconds(10))
            .ok_or_else(|| ImageLoadError::DecodeFailed("timeout waiting for pre-roll".into()))?;
        match msg.view() {
            gst::MessageView::Tag(tag_msg) => {
                if orientation.is_none() {
                    let tags = tag_msg.tags();
                    orientation = extract_orientation(&tags);
                }
            }
            gst::MessageView::Error(err) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(ImageLoadError::DecodeFailed(format!(
                    "gstreamer error: {}",
                    err.error()
                )));
            }
            gst::MessageView::StateChanged(sc) => {
                if let Some(src_el) = sc.src() {
                    if src_el == &pipeline && sc.current() == gst::State::Paused {
                        break;
                    }
                }
            }
            gst::MessageView::Eos(_) => {
                // Stream ended during pre-roll; try pulling sample anyway.
                break;
            }
            _ => {}
        }
    }

    // Seek to ~1 second to get a representative frame (better than t=0 for
    // live photos / short clips).  Ignore seek errors — some formats don't
    // support it and we fall back to whatever frame we got.
    let _ = pipeline.seek_simple(
        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
        gst::ClockTime::from_seconds(1),
    );

    // Transition to PLAYING and pull one sample.
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| ImageLoadError::DecodeFailed("set_state(Playing) failed".into()))?;

    let sample = appsink
        .try_pull_sample(gst::ClockTime::from_seconds(5))
        .ok_or_else(|| ImageLoadError::DecodeFailed("no sample from appsink within 5 s".into()))?;

    // Shut down the pipeline.
    pipeline
        .set_state(gst::State::Null)
        .map_err(|_| ImageLoadError::DecodeFailed("set_state(Null) failed".into()))?;

    // Extract RGBA from the sample.
    let buffer = sample
        .buffer()
        .ok_or_else(|| ImageLoadError::DecodeFailed("sample has no buffer".into()))?;

    let sample_caps = sample
        .caps()
        .ok_or_else(|| ImageLoadError::DecodeFailed("sample has no caps".into()))?;

    let video_info = gst_video::VideoInfo::from_caps(sample_caps)
        .map_err(|_| ImageLoadError::DecodeFailed("cannot parse video info from caps".into()))?;

    let width = video_info.width();
    let height = video_info.height();

    let frame = gst_video::VideoFrame::from_buffer_readable(buffer.to_owned(), &video_info)
        .map_err(|_| ImageLoadError::DecodeFailed("cannot create video frame".into()))?;

    let plane_data = frame
        .plane_data(0)
        .map_err(|e| ImageLoadError::DecodeFailed(format!("cannot get plane data: {e}")))?;
    let plane_stride = frame.plane_stride()[0] as usize;

    // Copy to tightly-packed RGBA (strip stride padding → width × height × 4).
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row_start = y * plane_stride;
        pixels.extend_from_slice(&plane_data[row_start..row_start + width as usize * 4]);
    }

    // If we captured a non-identity orientation tag from the bus, apply the
    // EXIF rotation to the decoded pixels.  `apply_orientation` handles
    // the full 1..=8 mapping and may swap width/height for 90°/270° variants.
    if let Some(o) = orientation {
        if o != 1 {
            let decoded = DecodedRgba { width, height, pixels };
            let rotated = crate::image::apply_orientation(decoded, o);
            return Ok(rotated);
        }
    }

    Ok(DecodedRgba {
        width,
        height,
        pixels,
    })
}

/// Dispatch video-thumbnail decode for each path via `spawn_blocking` (up to
/// [`MAX_CONCURRENT_DECODES`] in parallel) and invoke `on_each` on the GTK main
/// thread with each result as it arrives.
///
/// Mirrors [`crate::image::schedule_image_loads`]; reused for the inline
/// video-bubble thumbnail in the chat view.
///
/// The concurrency cap is 3, matching the constant in
/// [`crate::image::schedule_parallel`].
pub fn schedule_video_thumbnails<F>(
    items: Vec<PathBuf>,
    max_edge: u32,
    on_each: F,
)
where
    F: Fn(Result<DecodedRgba, ImageLoadError>) + Clone + 'static,
{
    // Channel ferries results from tokio workers → GTK main thread.
    let (tx, rx) = async_channel::unbounded::<Result<DecodedRgba, ImageLoadError>>();

    crate::image::schedule_parallel(
        items,
        move |path: &Path| decode_video_thumbnail_rgba(path, max_edge),
        move |result| {
            // send on an unbounded channel is non-blocking; send_blocking
            // on an unbounded channel never blocks.
            let _ = tx.send_blocking(result);
        },
    );

    // Drain on the GTK main thread and invoke the user's !Send callback.
    glib::spawn_future_local(async move {
        while let Ok(result) = rx.recv().await {
            on_each(result);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use gstreamer as gst;

    use crate::image::{DecodedRgba, ImageLoadError};

    use super::{decode_video_thumbnail_rgba, extract_orientation};

    // -----------------------------------------------------------------------
    // Test 1 – non-existent path → FileNotFound
    // -----------------------------------------------------------------------

    #[test]
    fn returns_file_not_found_for_missing_path() {
        let result = decode_video_thumbnail_rgba(
            Path::new("/tmp/does-not-exist-openbubbles-42.mp4"),
            320,
        );
        assert_eq!(result, Err(ImageLoadError::FileNotFound));
    }

    // -----------------------------------------------------------------------
    // Test 2 – invalid content → DecodeFailed
    // -----------------------------------------------------------------------

    #[test]
    fn returns_decode_failed_for_invalid_file() {
        let temp = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(temp.path(), b"this is not a video file")
            .expect("write garbage bytes");

        let result = decode_video_thumbnail_rgba(temp.path(), 320);
        assert!(result.is_err(), "expected Err for non-video content");
        match result {
            Err(ImageLoadError::DecodeFailed(_)) => { /* expected */ }
            other => panic!("expected DecodeFailed(_), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test 3 – real small MP4 → Ok(DecodedRgba) with correct shape & data
    //
    // This test requires `ffmpeg` on PATH to generate the fixture at runtime.
    // It is marked #[ignore] so `cargo test` passes without ffmpeg; run it
    // explicitly with:
    //
    //   cargo test video -- --ignored
    //
    // Fixture generation command (documented for reproducibility):
    //   ffmpeg -f lavfi -i testsrc=size=64x48:rate=10:duration=1 \
    //          -pix_fmt yuv420p -v quiet -y tests/fixtures/sample.mp4
    // -----------------------------------------------------------------------

    #[test]
    #[ignore]
    fn decodes_real_mp4_fixture() {
        // Check that ffmpeg is available before attempting generation.
        let has_ffmpeg = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map_or(false, |o| o.status.success());

        if !has_ffmpeg {
            eprintln!("ffmpeg not found on PATH — skipping real MP4 test");
            return;
        }

        let dir = tempfile::tempdir().expect("create temp dir for fixture");
        let mp4_path = dir.path().join("test.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-f", "lavfi",
                "-i", "testsrc=size=64x48:rate=10:duration=1",
                "-pix_fmt", "yuv420p",
                "-v", "quiet",
                "-y",
                mp4_path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg subprocess failed to start");
        assert!(
            status.success(),
            "ffmpeg exited with non-zero status: {status:?}",
        );

        let max_edge: u32 = 32;
        let result = decode_video_thumbnail_rgba(&mp4_path, max_edge);

        assert!(
            result.is_ok(),
            "expected Ok(DecodedRgba), got Err: {:?}",
            result,
        );
        let decoded: DecodedRgba = result.unwrap();

        // Tightly-packed RGBA: width × height × 4 == pixels.len()
        assert_eq!(
            decoded.pixels.len() as u32,
            decoded.width * decoded.height * 4,
            "pixel buffer must be tightly-packed RGBA (width×height×4)",
        );

        // Scaled so the longer edge ≤ max_edge.
        assert!(
            decoded.width <= max_edge && decoded.height <= max_edge,
            "scaled dimensions ({0}×{1}) must not exceed max_edge ({max_edge})",
            decoded.width, decoded.height,
        );

        // And the longer edge must equal max_edge (proportional scaling).
        assert!(
            decoded.width == max_edge || decoded.height == max_edge,
            "one dimension must equal max_edge ({max_edge}), got {0}×{1}",
            decoded.width, decoded.height,
        );

        // At least some pixel components must be non-zero — real frame data,
        // not a zero-filled placeholder.
        let nonzero_count = decoded.pixels.iter().filter(|&&b| b != 0).count();
        assert!(
            nonzero_count > 0,
            "decoded pixel data is all zeros — no real frame data",
        );
    }

    // -----------------------------------------------------------------------
    // extract_orientation tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_orientation_returns_none_for_empty_tag_list() {
        // Gstreamer must be initialized to construct a TagList.
        let _ = gst::init();

        let tags = gst::TagList::new();
        assert_eq!(extract_orientation(&tags), None);
    }

    #[test]
    fn extract_orientation_returns_some_for_tagged_list() {
        let _ = gst::init();

        let mut tags = gst::TagList::new();
        tags.get_mut()
            .unwrap()
            .add::<gst::tags::ImageOrientation>(&"rotate-90", gst::TagMergeMode::Replace);
        assert_eq!(extract_orientation(&tags), Some(6));
    }

    #[test]
    fn extract_orientation_returns_none_for_out_of_range_value() {
        let _ = gst::init();

        let mut tags = gst::TagList::new();
        // An unrecognized string value.
        tags.get_mut()
            .unwrap()
            .add::<gst::tags::ImageOrientation>(&"bogus", gst::TagMergeMode::Replace);
        assert_eq!(extract_orientation(&tags), None);
    }
}
