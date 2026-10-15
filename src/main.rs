use std::env;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{PathBuf, absolute};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use image::RgbaImage;
use latchshot::output::{Target, notify, write_to_targets};
use latchshot::overlay::select;
use latchshot::{
    Compositor, FrameCapture, Rect, Scene, Selection, SelectionResult, WaylandCapture, Window,
    WindowCapture,
};
use log::{info, warn};

#[derive(Parser)]
#[command(version, about)]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    /// Write the screenshot to PATH
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Write PNG data to stdout
    #[arg(long)]
    stdout: bool,

    /// Copy the PNG to the Wayland clipboard (the default)
    #[arg(short, long)]
    clipboard: bool,

    /// Print the scene as JSON
    #[arg(long, conflicts_with_all = ["output", "stdout", "clipboard"])]
    windows: bool,

    /// Disable snap animation
    #[arg(long)]
    no_animation: bool,

    /// Disable desktop notifications
    #[arg(long)]
    no_notify: bool,

    /// Crop clicked windows from the frozen desktop instead of capturing the
    /// toplevel
    #[arg(long)]
    prefer_crop: bool,

    /// Capture the focused window without the overlay (not supported by the
    /// generic backend)
    #[arg(long, conflicts_with_all = ["windows", "all_outputs"])]
    active_window: bool,

    /// Capture every output as one image without the overlay; gaps between
    /// outputs stay transparent
    #[arg(long, conflicts_with = "windows")]
    all_outputs: bool,

    /// Compositor backend (auto-detected, or generic when unknown)
    #[arg(long)]
    compositor: Option<Compositor>,
}

impl Args {
    fn targets(self) -> Result<Vec<Target>> {
        let mut targets = Vec::with_capacity(3);

        if let Some(path) = self.output {
            let path = absolute(&path)
                .with_context(|| format!("failed to resolve output path {}", path.display()))?;
            targets.push(Target::File(path));
        }
        if self.stdout {
            targets.push(Target::Stdout);
        }
        if self.clipboard {
            targets.push(Target::Clipboard);
        }
        if targets.is_empty() {
            targets.push(Target::Clipboard);
        }

        Ok(targets)
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let Some(capture_lock) = try_acquire_capture_lock()? else {
        info!("another latchshot capture is already in progress; exiting");

        return Ok(());
    };

    let backend = args
        .compositor
        .or_else(Compositor::detect)
        .unwrap_or(Compositor::Generic);
    info!("compositor backend: {backend}");
    let mut compositor_capture = backend.window_capturer();
    let mut compositor = backend.connect()?;
    let mut scene = compositor.scene()?;
    if scene.outputs.is_empty() {
        bail!("the compositor reported no active outputs");
    }

    let mut capture = WaylandCapture::connect()?;
    let mut prefer_crop = args.prefer_crop;
    // A focused window that is already known can be captured natively without
    // freezing the whole desktop first.
    if args.active_window
        && !prefer_crop
        && let Some(window) = scene.windows.iter().find(|window| window.focused)
    {
        if let Some(image) = capture_window(&mut capture, &mut compositor_capture, window) {
            drop(capture_lock);

            return deliver(&image, args);
        }
        // Crop the frozen frame below instead of retrying the failed capture.
        prefer_crop = true;
    }
    let frame = capture.capture(&scene)?;
    compositor.refine_scene(&mut scene, &frame)?;
    info!(
        "scene: {} outputs, {} windows",
        scene.outputs.len(),
        scene.windows.len()
    );

    if args.windows {
        println!("{}", serde_json::to_string_pretty(&scene).unwrap());

        return Ok(());
    }

    let (selection, frame) = if args.active_window {
        let window = scene
            .windows
            .iter()
            .find(|window| window.focused)
            .cloned()
            .with_context(|| format!("the {backend} backend reported no focused window"))?;
        ensure!(
            frame.outputs.iter().any(|output| output
                .logical_geometry
                .intersection(window.geometry)
                .is_some()),
            "the focused window lies outside every output"
        );
        (Selection::Window(window), frame)
    } else if args.all_outputs {
        (Selection::Region(desktop_bounds(&scene)), frame)
    } else {
        match select(scene, frame, !args.no_animation)? {
            (SelectionResult::Selected(selection), frame) => (selection, frame),
            (SelectionResult::Cancelled, _) => return Ok(()),
        }
    };
    let image = match selection {
        Selection::Window(window) => {
            if prefer_crop {
                info!(
                    "native window capture disabled or failed; cropping the frozen desktop frame"
                );

                frame.crop(window.geometry)
            } else {
                capture_window(&mut capture, &mut compositor_capture, &window).unwrap_or_else(
                    || {
                        info!(
                            "native window capture unavailable; cropping the frozen desktop frame"
                        );
                        frame.crop(window.geometry)
                    },
                )
            }
        }
        Selection::Region(geometry) => frame.crop(geometry),
    };
    drop(capture_lock);

    deliver(&image, args)
}

fn deliver(image: &RgbaImage, args: Args) -> Result<()> {
    let notifications_enabled = !args.no_notify;
    let targets = args.targets()?;
    write_to_targets(image, &targets)?;

    if notifications_enabled {
        let body = targets
            .iter()
            .filter_map(|target| match target {
                Target::File(path) => Some(format!("Screenshot saved to {}", path.display())),
                Target::Clipboard => Some("Screenshot copied to clipboard".to_owned()),
                Target::Stdout => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if !body.is_empty() {
            notify(&body);
        }
    }

    Ok(())
}

fn desktop_bounds(scene: &Scene) -> Rect {
    let (mut left, mut top) = (f64::INFINITY, f64::INFINITY);
    let (mut right, mut bottom) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for output in &scene.outputs {
        let geometry = output.logical_geometry;
        left = left.min(geometry.left());
        top = top.min(geometry.top());
        right = right.max(geometry.right());
        bottom = bottom.max(geometry.bottom());
    }

    Rect::new(left, top, right - left, bottom - top)
}

fn capture_window(
    capture: &mut dyn WindowCapture,
    compositor: &mut Option<Box<dyn WindowCapture>>,
    window: &Window,
) -> Option<RgbaImage> {
    match capture.capture_window(window) {
        Ok(Some(image)) => return Some(image),
        Ok(None) => {}
        Err(error) => warn!("standard Wayland window capture failed: {error:#}"),
    }

    let compositor = compositor.as_deref_mut()?;
    match compositor.capture_window(window) {
        Ok(Some(image)) => Some(image),
        Ok(None) => None,
        Err(error) => {
            warn!("compositor-specific window capture failed: {error:#}");
            None
        }
    }
}

fn try_acquire_capture_lock() -> Result<Option<File>> {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let path = PathBuf::from(runtime_dir).join("latchshot.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open capture lock {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("failed to acquire capture lock {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use latchshot::{Output, OutputId, OutputTransform, Rect, Scene, Size};

    use super::desktop_bounds;

    fn output(name: &str, geometry: Rect) -> Output {
        Output {
            id: OutputId::new(name),
            logical_geometry: geometry,
            pixel_size: Size::new(geometry.width(), geometry.height()),
            scale: 1.0,
            transform: OutputTransform::Normal,
        }
    }

    #[test]
    fn desktop_bounds_span_offset_outputs_and_gaps() {
        let scene = Scene {
            outputs: vec![
                output("left", Rect::new(-1920.0, 155.0, 1920.0, 1080.0)),
                output("right", Rect::new(100.0, 0.0, 2560.0, 1440.0)),
            ],
            windows: Vec::new(),
        };

        assert_eq!(
            desktop_bounds(&scene),
            Rect::new(-1920.0, 0.0, 4580.0, 1440.0)
        );
    }
}
