//! niri-wallpaper — dual-layer wallpaper daemon.
//!
//! A single process that owns two layer-shell surfaces per output:
//! - namespace `wallpaper-bg` → shown as the regular background
//! - namespace `wallpaper`    → matched by niri's `place-within-backdrop` rule,
//!                              shown only as the overview backdrop (GPU-blurred)
//!
//! Watches a symlink (`--path`, defaults to ~/.config/niri-wallpaper/current).
//! On change, reloads the image, regenerates the blur on the GPU, and redraws
//! every output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::RgbaImage;

use calloop::timer::{TimeoutAction, Timer};
use calloop::{generic::Generic, EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use inotify::{Inotify, WatchDescriptor, WatchMask};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface::WlSurface},
    Connection, Proxy, QueueHandle,
};

mod animation;
mod blur;
mod config;
mod ipc;
mod wallpaper;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// IPC subcommand sent to a running daemon instance.
#[derive(Debug)]
enum IpcCommand {
    Next,
    Set(PathBuf),
    Current,
}

/// Parsed CLI: either run as the daemon or send an IPC command to a running
/// instance.
#[derive(Debug)]
enum CliMode {
    Daemon {
        /// CLI overrides applied on top of the TOML config.
        overrides: CliOverrides,
    },
    Ipc(IpcCommand),
    /// Pick a random wallpaper from the library, update the `current` symlink,
    /// print the path, and exit. No Wayland or daemon needed.
    Rotate,
}

/// Optional per-field CLI overrides. `None` means "use config/default".
#[derive(Debug, Default)]
struct CliOverrides {
    path: Option<PathBuf>,
    normal_namespace: Option<String>,
    backdrop_namespace: Option<String>,
    blur_downsample: Option<u32>,
    blur_radius: Option<u32>,
    blur_iterations: Option<u32>,
}

fn parse_cli() -> Result<CliMode> {
    let argv: Vec<String> = std::env::args().collect();

    // Check for IPC subcommands first.
    if argv.len() >= 2 {
        match argv[1].as_str() {
            "rotate" => return Ok(CliMode::Rotate),
            "msg" => {
                let sub = argv
                    .get(2)
                    .ok_or_else(|| anyhow!("usage: niri-wallpaper msg <next|set|current>"))?;
                match sub.as_str() {
                    "next" => return Ok(CliMode::Ipc(IpcCommand::Next)),
                    "current" => return Ok(CliMode::Ipc(IpcCommand::Current)),
                    "set" => {
                        let path = argv
                            .get(3)
                            .ok_or_else(|| anyhow!("usage: niri-wallpaper msg set <path>"))?;
                        return Ok(CliMode::Ipc(IpcCommand::Set(PathBuf::from(path))));
                    }
                    other => return Err(anyhow!("unknown msg command: {}", other)),
                }
            }
            _ => {}
        }
    }

    let mut overrides = CliOverrides::default();
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        let next = || {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| anyhow!("missing value for {}", arg))
        };
        match arg.as_str() {
            "-p" | "--path" => {
                overrides.path = Some(PathBuf::from(next()?));
                i += 2;
            }
            "--normal-namespace" => {
                overrides.normal_namespace = Some(next()?);
                i += 2;
            }
            "--backdrop-namespace" => {
                overrides.backdrop_namespace = Some(next()?);
                i += 2;
            }
            "--blur-downsample" => {
                overrides.blur_downsample =
                    Some(next()?.parse().context("--blur-downsample u32")?);
                i += 2;
            }
            "--blur-radius" => {
                overrides.blur_radius = Some(next()?.parse().context("--blur-radius u32")?);
                i += 2;
            }
            "--blur-iterations" => {
                overrides.blur_iterations =
                    Some(next()?.parse().context("--blur-iterations u32")?);
                i += 2;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {}", other)),
        }
    }
    Ok(CliMode::Daemon { overrides })
}

/// Apply CLI overrides on top of a loaded config.
fn apply_overrides(config: &mut config::Config, overrides: &CliOverrides) {
    if let Some(ref p) = overrides.path {
        config.path = Some(p.clone());
    }
    if let Some(ref ns) = overrides.normal_namespace {
        config.namespaces.normal = ns.clone();
    }
    if let Some(ref ns) = overrides.backdrop_namespace {
        config.namespaces.backdrop = ns.clone();
    }
    if let Some(v) = overrides.blur_downsample {
        config.blur.downsample = v;
    }
    if let Some(v) = overrides.blur_radius {
        config.blur.radius = v;
    }
    if let Some(v) = overrides.blur_iterations {
        config.blur.iterations = v;
    }
}

fn print_help() {
    println!(
        "niri-wallpaper — dual-layer wallpaper daemon for niri

USAGE:
    niri-wallpaper [OPTIONS]       Start the daemon
    niri-wallpaper rotate            Pick a random wallpaper, update symlink, exit
    niri-wallpaper msg next          Force rotation on a running daemon
    niri-wallpaper msg set <PATH>    Set a specific wallpaper on a running daemon
    niri-wallpaper msg current       Print the current wallpaper path

OPTIONS:
    -p, --path <PATH>              Path to wallpaper (symlink OK). Watched for changes.
                                   Overrides config file. [default from config or
                                   ~/.config/niri-wallpaper/current]
    --normal-namespace <NAME>      Layer-shell namespace for the normal surface.
                                   [default: wallpaper-bg]
    --backdrop-namespace <NAME>    Layer-shell namespace for the backdrop surface.
                                   niri's layer-rule should match this to place it
                                   within the overview backdrop.
                                   [default: wallpaper]
    --blur-downsample <N>          Downsample factor before blurring. [default: 8]
    --blur-radius <N>              Box-blur radius (in downsampled pixels). [default: 6]
    --blur-iterations <N>          Box-blur passes (3 ≈ Gaussian). [default: 3]
    -h, --help                     Print this help.

CONFIGURATION:
    Config file: ~/.config/niri-wallpaper/config.toml
    See project documentation for all config options.
"
    );
}

pub(crate) fn dirs_config_home() -> Result<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(x));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config"))
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Cover-scale `src` to exactly `(dst_w, dst_h)`, center-cropping. Uses
/// `fast_image_resize` which is SIMD-vectorized and 3-5× faster than
/// `image::imageops::resize` for the filters we use here.
///
/// The source crop rectangle (same aspect ratio as the destination, centered)
/// is passed to the resizer via `ResizeOptions::crop`, so the resize and the
/// center-crop happen in a single pass — no separate `crop_imm` copy.
fn cover_scale(src: &RgbaImage, dst_w: u32, dst_h: u32, alg: ResizeAlg) -> RgbaImage {
    if dst_w == 0 || dst_h == 0 {
        return RgbaImage::new(1, 1);
    }
    let (sw, sh) = (src.width().max(1), src.height().max(1));

    // Aspect-matched source rectangle → the resizer stretches it to dst exactly.
    let aspect_dst = dst_w as f64 / dst_h as f64;
    let aspect_src = sw as f64 / sh as f64;
    let (crop_w, crop_h) = if aspect_src > aspect_dst {
        (sh as f64 * aspect_dst, sh as f64)
    } else {
        (sw as f64, sw as f64 / aspect_dst)
    };
    let crop_x = (sw as f64 - crop_w) / 2.0;
    let crop_y = (sh as f64 - crop_h) / 2.0;

    let src_view = ImageRef::new(sw, sh, src.as_raw(), PixelType::U8x4)
        .expect("source ImageRef");
    let mut dst_img = Image::new(dst_w, dst_h, PixelType::U8x4);

    // Opaque wallpapers don't need premultiplied-alpha math.
    let opts = ResizeOptions::new()
        .resize_alg(alg)
        .crop(crop_x, crop_y, crop_w, crop_h)
        .use_alpha(false);

    let mut resizer = Resizer::new();
    resizer
        .resize(&src_view, &mut dst_img, &opts)
        .expect("resize");

    RgbaImage::from_raw(dst_w, dst_h, dst_img.into_vec()).expect("RgbaImage from raw")
}

fn alg_for(role: SurfaceRole) -> ResizeAlg {
    match role {
        SurfaceRole::Normal => ResizeAlg::Convolution(FilterType::Bilinear),
        // Bilinear upscale smooths out the pixel boundaries of the downsampled
        // blur — nearest-neighbor leaves visible blocks at high downsample
        // factors. The cost is minor (runs once per wallpaper change).
        SurfaceRole::Backdrop => ResizeAlg::Convolution(FilterType::Bilinear),
    }
}

/// Return freed heap pages to the OS. glibc's allocator keeps freed memory
/// as an internal cache by default; after a big decode + resize cycle the
/// cache can be 20-50 MB. `malloc_trim(0)` hands it back.
#[cfg(target_env = "gnu")]
fn trim_heap() {
    // SAFETY: malloc_trim is thread-safe and callable at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(target_env = "gnu"))]
fn trim_heap() {}

// ---------------------------------------------------------------------------
// Per-surface state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceRole {
    Normal,
    Backdrop,
}

struct SurfaceSlot {
    layer: LayerSurface,
    /// Committed size (from the latest configure).
    size: Option<(u32, u32)>,
    /// Per-surface shm pool; recreated lazily as size changes.
    pool: Option<SlotPool>,
}

struct OutputSurfaces {
    output: wl_output::WlOutput,
    normal: SurfaceSlot,
    backdrop: SurfaceSlot,
}

impl OutputSurfaces {
    fn slot_mut(&mut self, role: SurfaceRole) -> &mut SurfaceSlot {
        match role {
            SurfaceRole::Normal => &mut self.normal,
            SurfaceRole::Backdrop => &mut self.backdrop,
        }
    }
}

const ROLES: [SurfaceRole; 2] = [SurfaceRole::Normal, SurfaceRole::Backdrop];

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    qh: QueueHandle<Self>,

    config: config::Config,

    /// Currently loaded image (full-res RGBA). Kept for re-renders on
    /// configure / hotplug; re-decoded from disk on each wallpaper change.
    image: Option<RgbaImage>,
    /// Blurred, downsampled version of `image`. Small (≈ 1 MB). Cover-scaled
    /// per output at draw time.
    blurred: Option<RgbaImage>,

    /// Path of the currently displayed wallpaper (for excluding from random
    /// selection and for the IPC `current` command).
    current_wallpaper: Option<PathBuf>,

    /// Active cross-fade animation (if any).
    fade: Option<animation::FadeState>,

    /// New full-res image waiting for fade to complete.
    pending_image: Option<RgbaImage>,

    /// Persistent wallpaper usage tracker for weighted random selection.
    usage: wallpaper::UsageTracker,

    /// Calloop handle for inserting/removing event sources (e.g., schedule timer).
    loop_handle: Option<LoopHandle<'static, App>>,

    /// Cron timer registration token (so we can cancel/re-register on config change).
    schedule_token: Option<RegistrationToken>,

    /// Set to `true` after we update the `current` symlink ourselves, to
    /// suppress the inotify event that the symlink update generates.
    suppress_next_wallpaper_event: bool,

    /// wl_surface id → OutputSurfaces index.
    surface_to_output: HashMap<u32, usize>,
    outputs: Vec<OutputSurfaces>,
}

impl App {
    fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Self>,
        config: config::Config,
    ) -> Result<Self> {
        let registry_state = RegistryState::new(globals);
        let output_state = OutputState::new(globals, qh);
        let compositor_state =
            CompositorState::bind(globals, qh).context("wl_compositor not available")?;
        let layer_shell =
            LayerShell::bind(globals, qh).context("zwlr_layer_shell_v1 not available")?;
        let shm = Shm::bind(globals, qh).context("wl_shm not available")?;

        let usage = wallpaper::UsageTracker::load();

        Ok(Self {
            registry_state,
            output_state,
            compositor_state,
            layer_shell,
            shm,
            qh: qh.clone(),
            config,
            image: None,
            blurred: None,
            current_wallpaper: None,
            fade: None,
            pending_image: None,
            usage,
            loop_handle: None,
            schedule_token: None,
            suppress_next_wallpaper_event: false,
            surface_to_output: HashMap::new(),
            outputs: Vec::new(),
        })
    }

    /// Decode an image from disk and generate its blurred preview.
    /// Returns `(image, blurred, canonical_path)`.
    fn decode_image(&self, path: &Path) -> Result<(RgbaImage, RgbaImage, PathBuf)> {
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("resolve {}", path.display()))?;
        log::info!("loading wallpaper: {}", canonical.display());

        let t0 = std::time::Instant::now();
        let img = image::open(&canonical)
            .with_context(|| format!("decode {}", canonical.display()))?
            .to_rgba8();
        log::debug!(
            "decoded {}x{} in {:?}",
            img.width(),
            img.height(),
            t0.elapsed()
        );

        let t0 = std::time::Instant::now();
        let blurred = blur::blur(
            &img,
            self.config.blur.downsample,
            self.config.blur.radius,
            self.config.blur.iterations,
        );
        log::info!(
            "blurred {}x{} → {}x{} in {:?}",
            img.width(),
            img.height(),
            blurred.width(),
            blurred.height(),
            t0.elapsed()
        );

        Ok((img, blurred, canonical))
    }

    /// Load a new wallpaper and either instant-swap or start a cross-fade.
    /// If `path` is `None`, resolves the wallpaper path from the config.
    fn change_wallpaper(&mut self, path: Option<&Path>, qh: &QueueHandle<Self>) -> Result<()> {
        let resolve;
        let path = match path {
            Some(p) => p,
            None => {
                resolve = config::resolve_wallpaper_path(&self.config)?;
                &resolve
            }
        };

        let (new_img, new_blurred, canonical) = self.decode_image(path)?;

        let duration_ms = self.config.animation.duration_ms;
        let has_old_image = self.image.is_some();

        if duration_ms == 0 || !has_old_image {
            // Instant swap (no animation, or first image load).
            self.image = Some(new_img);
            self.blurred = Some(new_blurred);
            self.current_wallpaper = Some(canonical.clone());
            self.redraw_all();
        } else {
            // If a fade is already in progress, finalize it first.
            if self.fade.is_some() {
                self.finalize_fade();
            }

            // Pre-scale old and new normal images for each output.
            let old_image = self.image.as_ref().unwrap();
            let output_frames: Vec<Option<animation::OutputFadeFrames>> = self
                .outputs
                .iter()
                .map(|out| {
                    let (w, h) = out.normal.size?;
                    if w == 0 || h == 0 {
                        return None;
                    }
                    let old_normal = cover_scale(old_image, w, h, alg_for(SurfaceRole::Normal));
                    let new_normal = cover_scale(&new_img, w, h, alg_for(SurfaceRole::Normal));
                    Some(animation::OutputFadeFrames {
                        old_normal,
                        new_normal,
                    })
                })
                .collect();

            // Instant-swap all backdrop surfaces to the new blurred image.
            self.blurred = Some(new_blurred);

            // Collect backdrop jobs first (avoid borrow conflict with upload_slot).
            let backdrop_jobs: Vec<(usize, u32, u32, RgbaImage)> = self
                .outputs
                .iter()
                .enumerate()
                .filter_map(|(idx, out)| {
                    let (w, h) = out.backdrop.size?;
                    if w == 0 || h == 0 {
                        return None;
                    }
                    let blurred = self.blurred.as_ref()?;
                    let scaled = cover_scale(blurred, w, h, alg_for(SurfaceRole::Backdrop));
                    Some((idx, w, h, scaled))
                })
                .collect();
            for (idx, w, h, scaled) in backdrop_jobs {
                self.upload_slot(idx, SurfaceRole::Backdrop, w, h, scaled);
            }

            let duration = Duration::from_millis(duration_ms);
            self.fade = Some(animation::FadeState {
                start: Instant::now(),
                duration,
                easing: self.config.animation.easing,
                output_frames,
            });

            // Store the new image — it will replace self.image when fade completes.
            // We keep a reference to it in the fade state via output_frames.
            // Store canonicalized path now.
            self.current_wallpaper = Some(canonical.clone());

            // Render first frame (fully old image) and request frame callbacks.
            self.render_fade_frame(qh);

            // Store the new full-res image for finalization.
            // We stash it temporarily — need a field for this.
            self.pending_image = Some(new_img);
        }

        trim_heap();
        Ok(())
    }

    /// Render one frame of the cross-fade animation and request the next
    /// frame callback on each animated surface.
    fn render_fade_frame(&mut self, qh: &QueueHandle<Self>) {
        let fade = match self.fade.as_ref() {
            Some(f) => f,
            None => return,
        };

        let alpha = fade.progress();
        let is_done = fade.is_done();

        // Collect blended frames first to avoid borrow conflicts.
        let frames: Vec<(usize, RgbaImage)> = fade
            .output_frames
            .iter()
            .enumerate()
            .filter_map(|(idx, frames_opt)| {
                let f = frames_opt.as_ref()?;
                let blended = if is_done {
                    f.new_normal.clone()
                } else {
                    animation::blend(&f.old_normal, &f.new_normal, alpha)
                };
                Some((idx, blended))
            })
            .collect();

        for (idx, blended) in frames {
            let (w, h) = blended.dimensions();
            self.upload_slot(idx, SurfaceRole::Normal, w, h, blended);

            if !is_done {
                // Request next frame callback.
                let wl_surface = self.outputs[idx].normal.layer.wl_surface();
                wl_surface.frame(qh, wl_surface.clone());
                self.outputs[idx].normal.layer.commit();
            }
        }

        if is_done {
            self.finalize_fade();
        }
    }

    /// Finalize the cross-fade: promote new images to current, drop fade state.
    fn finalize_fade(&mut self) {
        if let Some(new_img) = self.pending_image.take() {
            self.image = Some(new_img);
        }
        self.fade = None;
        trim_heap();
        log::debug!("cross-fade completed");
    }

    /// Handle a config file change: re-parse, diff, and apply.
    fn handle_config_reload(&mut self, cli_overrides: &CliOverrides) {
        let mut new_cfg = match config::load_config() {
            Ok(c) => c,
            Err(e) => {
                log::error!("config reload failed: {:#}", e);
                return;
            }
        };
        apply_overrides(&mut new_cfg, cli_overrides);

        let old = &self.config;

        if old.namespaces != new_cfg.namespaces {
            log::warn!(
                "namespace config changed — restart required for this to take effect"
            );
        }

        let blur_changed = old.blur != new_cfg.blur;
        let schedule_changed = old.schedule != new_cfg.schedule;

        self.config = new_cfg;

        if blur_changed {
            log::info!("blur config changed, regenerating");
            if let Some(ref img) = self.image {
                let blurred = blur::blur(
                    img,
                    self.config.blur.downsample,
                    self.config.blur.radius,
                    self.config.blur.iterations,
                );
                self.blurred = Some(blurred);
                self.redraw_all();
                trim_heap();
            }
        }

        if schedule_changed {
            log::info!("schedule config changed");
            if let Some(ref lh) = self.loop_handle {
                if let Some(ref schedule) = self.config.schedule {
                    setup_schedule_timer(lh, &mut self.schedule_token, schedule);
                } else {
                    // Schedule removed — cancel the timer.
                    if let Some(token) = self.schedule_token.take() {
                        lh.remove(token);
                        log::info!("schedule timer removed");
                    }
                }
            }
        }

        log::info!("config reloaded successfully");
    }

    /// Pick the next wallpaper from the library using weighted random selection,
    /// load it, update the current symlink, and redraw all outputs.
    fn rotate_wallpaper(&mut self) {
        let library_dir = match config::resolve_library_path(&self.config) {
            Ok(d) => d,
            Err(e) => {
                log::error!("cannot resolve library path: {:#}", e);
                return;
            }
        };

        let candidates = wallpaper::scan_library(&library_dir);
        if candidates.is_empty() {
            log::warn!("no wallpapers found in {}", library_dir.display());
            return;
        }

        let chosen = match self.usage.pick(&candidates, self.current_wallpaper.as_deref()) {
            Some(p) => p,
            None => {
                log::warn!("could not pick a wallpaper");
                return;
            }
        };

        log::info!("rotating to wallpaper: {}", chosen.display());
        let qh = self.qh.clone();
        if let Err(e) = self.change_wallpaper(Some(&chosen), &qh) {
            log::error!("failed to load wallpaper {}: {:#}", chosen.display(), e);
        }
        // Update the current symlink so other apps can discover the wallpaper.
        self.update_symlink();
    }

    /// Update the `current` symlink and suppress the resulting inotify event.
    fn update_symlink(&mut self) {
        if let Some(ref cur) = self.current_wallpaper {
            self.suppress_next_wallpaper_event = true;
            if let Err(e) = wallpaper::update_current_symlink(cur) {
                log::error!("update current symlink: {:#}", e);
                self.suppress_next_wallpaper_event = false;
            }
        }
    }

    /// Handle an IPC request and write the response.
    fn handle_ipc(&mut self, mut stream: std::os::unix::net::UnixStream) {
        let request = match ipc::read_request(&stream) {
            Ok(r) => r,
            Err(e) => {
                ipc::write_error(&mut stream, &format!("{:#}", e));
                return;
            }
        };

        match request {
            ipc::IpcRequest::Next => {
                self.rotate_wallpaper();
                match &self.current_wallpaper {
                    Some(p) => ipc::write_response(&mut stream, &format!("ok {}", p.display())),
                    None => ipc::write_error(&mut stream, "no wallpaper selected"),
                }
            }
            ipc::IpcRequest::Set(path) => {
                let qh = self.qh.clone();
                match self.change_wallpaper(Some(&path), &qh) {
                    Ok(()) => {
                        self.update_symlink();
                        ipc::write_response(&mut stream, "ok");
                    }
                    Err(e) => ipc::write_error(&mut stream, &format!("{:#}", e)),
                }
            }
            ipc::IpcRequest::Current => {
                match &self.current_wallpaper {
                    Some(p) => ipc::write_response(&mut stream, &p.to_string_lossy()),
                    None => ipc::write_response(&mut stream, "none"),
                }
            }
        }
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let normal_ns = self.config.namespaces.normal.clone();
        let backdrop_ns = self.config.namespaces.backdrop.clone();
        let normal_layer = self.create_layer(qh, &output, &normal_ns, Layer::Bottom);
        let backdrop_layer = self.create_layer(qh, &output, &backdrop_ns, Layer::Background);

        let idx = self.outputs.len();
        self.surface_to_output
            .insert(normal_layer.wl_surface().id().protocol_id(), idx);
        self.surface_to_output
            .insert(backdrop_layer.wl_surface().id().protocol_id(), idx);

        self.outputs.push(OutputSurfaces {
            output,
            normal: SurfaceSlot { layer: normal_layer, size: None, pool: None },
            backdrop: SurfaceSlot { layer: backdrop_layer, size: None, pool: None },
        });
    }

    fn create_layer(
        &self,
        qh: &QueueHandle<Self>,
        output: &wl_output::WlOutput,
        namespace: &str,
        layerbo: Layer,
    ) -> LayerSurface {
        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            layerbo,
            Some(namespace.to_string()),
            Some(output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        layer
    }

    fn remove_output(&mut self, output: &wl_output::WlOutput) {
        let Some(pos) = self.outputs.iter().position(|o| &o.output == output) else {
            return;
        };
        let removed = self.outputs.swap_remove(pos);
        for role in ROLES {
            let id = match role {
                SurfaceRole::Normal => removed.normal.layer.wl_surface().id().protocol_id(),
                SurfaceRole::Backdrop => removed.backdrop.layer.wl_surface().id().protocol_id(),
            };
            self.surface_to_output.remove(&id);
        }
        // If we swap_removed a non-last element, the last one moved into `pos` —
        // re-point its surface-ids to the new index.
        if pos < self.outputs.len() {
            for role in ROLES {
                let id = self.outputs[pos].slot_mut(role).layer.wl_surface().id().protocol_id();
                self.surface_to_output.insert(id, pos);
            }
        }
    }

    fn redraw_all(&mut self) {
        // Gather the jobs that actually need rendering, borrowing the source
        // images immutably so `cover_scale` can run on worker threads.
        let image = self.image.as_ref();
        let blurred = self.blurred.as_ref();

        let jobs: Vec<(usize, SurfaceRole, u32, u32, &RgbaImage)> = self
            .outputs
            .iter()
            .enumerate()
            .flat_map(|(idx, out)| {
                ROLES.iter().copied().filter_map(move |role| {
                    let slot = match role {
                        SurfaceRole::Normal => &out.normal,
                        SurfaceRole::Backdrop => &out.backdrop,
                    };
                    let (w, h) = slot.size?;
                    if w == 0 || h == 0 {
                        return None;
                    }
                    let src = match role {
                        SurfaceRole::Normal => image?,
                        SurfaceRole::Backdrop => blurred?,
                    };
                    Some((idx, role, w, h, src))
                })
            })
            .collect();

        if jobs.is_empty() {
            return;
        }

        // Phase 1: cover_scale in parallel. Each slot's resize is independent
        // and CPU-bound; on multi-output setups this is the dominant cost.
        let t0 = std::time::Instant::now();
        let scaled: Vec<(usize, SurfaceRole, u32, u32, RgbaImage)> =
            std::thread::scope(|s| {
                let handles: Vec<_> = jobs
                    .iter()
                    .map(|&(idx, role, w, h, src)| {
                        s.spawn(move || (idx, role, w, h, cover_scale(src, w, h, alg_for(role))))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("cover_scale thread"))
                    .collect()
            });
        log::debug!("parallel cover_scale of {} slot(s) in {:?}", jobs.len(), t0.elapsed());

        // Phase 2: upload + commit sequentially. The Wayland buffer / SlotPool
        // machinery lives on the main thread.
        for (idx, role, w, h, img) in scaled {
            self.upload_slot(idx, role, w, h, img);
        }

        // The 4× parallel resizes allocate ~dst_w*dst_h*4 each. Reclaim now.
        trim_heap();
    }

    fn redraw_slot(&mut self, idx: usize, role: SurfaceRole) {
        let src_image = match role {
            SurfaceRole::Normal => self.image.as_ref(),
            SurfaceRole::Backdrop => self.blurred.as_ref(),
        };
        let Some(src_image) = src_image else { return };

        let (w, h) = match self.outputs[idx].slot_mut(role).size {
            Some((w, h)) if w > 0 && h > 0 => (w, h),
            _ => return,
        };

        let scaled = cover_scale(src_image, w, h, alg_for(role));
        self.upload_slot(idx, role, w, h, scaled);
        trim_heap();
    }

    fn upload_slot(&mut self, idx: usize, role: SurfaceRole, w: u32, h: u32, scaled: RgbaImage) {
        let slot = self.outputs[idx].slot_mut(role);

        let stride = (w * 4) as i32;
        let pool_len_needed = (stride as usize) * (h as usize);

        if slot.pool.is_none() {
            match SlotPool::new(pool_len_needed.max(4096), &self.shm) {
                Ok(p) => slot.pool = Some(p),
                Err(e) => {
                    log::error!("create slot pool: {}", e);
                    return;
                }
            }
        }
        let pool = slot.pool.as_mut().expect("pool created above");

        // `Abgr8888` on LE = bytes [R, G, B, A] — the native layout of
        // `image::RgbaImage`, so the pixel copy below is a straight memcpy
        // instead of a per-pixel swizzle.
        let (buffer, canvas) = match pool.create_buffer(
            w as i32,
            h as i32,
            stride,
            wl_shm::Format::Abgr8888,
        ) {
            Ok(v) => v,
            Err(e) => {
                log::error!("create_buffer failed: {}", e);
                return;
            }
        };

        canvas.copy_from_slice(scaled.as_raw());

        let wl_surface = slot.layer.wl_surface();
        wl_surface.damage_buffer(0, 0, w as i32, h as i32);
        wl_surface.set_buffer_scale(1);
        if let Err(e) = buffer.attach_to(wl_surface) {
            log::error!("attach buffer failed: {}", e);
            return;
        }
        slot.layer.commit();
    }
}

// ---------------------------------------------------------------------------
// Wayland trait impls
// ---------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
        if self.fade.is_some() {
            self.render_fade_frame(qh);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        log::info!("new output");
        self.add_output(qh, output);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        log::info!("output destroyed");
        self.remove_output(&output);
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl LayerShellHandler for App {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
    ) {
        log::info!("layer surface closed");
        let sid = layer.wl_surface().id().protocol_id();
        self.surface_to_output.remove(&sid);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        cfg: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let sid = layer.wl_surface().id().protocol_id();
        let Some(&idx) = self.surface_to_output.get(&sid) else {
            return;
        };

        let role = if self.outputs[idx].normal.layer.wl_surface().id().protocol_id() == sid {
            SurfaceRole::Normal
        } else {
            SurfaceRole::Backdrop
        };

        let slot = self.outputs[idx].slot_mut(role);
        let (w, h) = cfg.new_size;
        // `(0, 0)` means "pick your own" — clamp to 1 until we get real dims.
        let new_size = (w.max(1), h.max(1));
        if slot.size == Some(new_size) {
            return;
        }
        slot.size = Some(new_size);

        self.redraw_slot(idx, role);
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

// ---------------------------------------------------------------------------
// Inotify → calloop source
// ---------------------------------------------------------------------------

/// Tracks inotify watch descriptors so the callback can distinguish which
/// file triggered an event.
struct InotifyWatches {
    /// Watch descriptor for the wallpaper path's parent directory.
    wallpaper_wd: WatchDescriptor,
    wallpaper_name: std::ffi::OsString,
    /// Watch descriptor for the config file's parent directory (if different
    /// from the wallpaper parent, or same directory with a different name).
    config_wd: Option<WatchDescriptor>,
    config_name: std::ffi::OsString,
}

const WATCH_MASK: WatchMask = WatchMask::MOVED_TO
    .union(WatchMask::CREATE)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::ATTRIB);

/// Set up inotify watches for both the wallpaper path and the config file.
fn setup_inotify(
    wallpaper_path: &Path,
    config_path: &Path,
) -> Result<(Inotify, InotifyWatches)> {
    let inotify = Inotify::init().context("inotify init")?;

    // Watch wallpaper parent directory.
    let wp_parent = wallpaper_path
        .parent()
        .ok_or_else(|| anyhow!("wallpaper path has no parent: {}", wallpaper_path.display()))?;
    let wp_name = wallpaper_path
        .file_name()
        .ok_or_else(|| anyhow!("wallpaper path has no file name: {}", wallpaper_path.display()))?
        .to_os_string();

    std::fs::create_dir_all(wp_parent)
        .with_context(|| format!("create {}", wp_parent.display()))?;

    let wallpaper_wd = inotify
        .watches()
        .add(wp_parent, WATCH_MASK)
        .with_context(|| format!("inotify watch {}", wp_parent.display()))?;

    // Watch config parent directory. If the config lives in the same directory
    // as the wallpaper, inotify de-duplicates the watch (returns the same wd).
    let cfg_parent = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", config_path.display()))?;
    let cfg_name = config_path
        .file_name()
        .ok_or_else(|| anyhow!("config path has no file name: {}", config_path.display()))?
        .to_os_string();

    std::fs::create_dir_all(cfg_parent)
        .with_context(|| format!("create {}", cfg_parent.display()))?;

    let config_wd = inotify
        .watches()
        .add(cfg_parent, WATCH_MASK)
        .with_context(|| format!("inotify watch {}", cfg_parent.display()))?;

    // If config_wd == wallpaper_wd, it means they share a directory — we still
    // differentiate by file name in the callback.
    let config_wd = if config_wd == wallpaper_wd {
        None // same watch, differentiate by name only
    } else {
        Some(config_wd)
    };

    Ok((
        inotify,
        InotifyWatches {
            wallpaper_wd,
            wallpaper_name: wp_name,
            config_wd,
            config_name: cfg_name,
        },
    ))
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// Compute the `Duration` from now until the next occurrence of the given cron
/// expression. Returns `None` if the expression is invalid or no next time exists.
fn next_cron_duration(schedule: &str) -> Option<Duration> {
    let cron = croner::Cron::from_str(schedule)
        .map_err(|e| log::error!("invalid cron expression {:?}: {}", schedule, e))
        .ok()?;
    let now = chrono::Local::now();
    let next = cron
        .find_next_occurrence(&now, false)
        .map_err(|e| log::error!("cron next occurrence error: {}", e))
        .ok()?;
    let delta = next.signed_duration_since(now);
    // Convert chrono::Duration to std::time::Duration.
    delta.to_std().ok().or_else(|| {
        // If negative (somehow in the past), fire immediately.
        Some(Duration::from_millis(0))
    })
}

/// Register (or re-register) the cron timer on the calloop event loop.
fn setup_schedule_timer(
    loop_handle: &LoopHandle<'static, App>,
    app_schedule_token: &mut Option<RegistrationToken>,
    schedule: &str,
) {
    // Remove existing timer if any.
    if let Some(token) = app_schedule_token.take() {
        loop_handle.remove(token);
    }

    let duration = match next_cron_duration(schedule) {
        Some(d) => d,
        None => {
            log::warn!("could not compute next cron occurrence for {:?}", schedule);
            return;
        }
    };

    log::info!(
        "next wallpaper rotation in {:.0?} (schedule: {})",
        duration,
        schedule,
    );

    let schedule_owned = schedule.to_string();
    let timer = Timer::from_duration(duration);
    match loop_handle.insert_source(timer, move |_deadline, _metadata, app: &mut App| {
        app.rotate_wallpaper();
        // Reschedule for the next occurrence.
        match next_cron_duration(&schedule_owned) {
            Some(d) => {
                log::info!("next wallpaper rotation in {:.0?}", d);
                TimeoutAction::ToDuration(d)
            }
            None => {
                log::warn!("no further cron occurrence, stopping rotation");
                TimeoutAction::Drop
            }
        }
    }) {
        Ok(token) => *app_schedule_token = Some(token),
        Err(e) => log::error!("failed to insert schedule timer: {}", e),
    }
}

/// Standalone rotate: pick a random wallpaper from the library, update the
/// `current` symlink, print the chosen path, and exit. No Wayland needed.
fn run_rotate() -> Result<()> {
    let cfg = config::load_config()?;
    let library_dir = config::resolve_library_path(&cfg)?;

    let candidates = wallpaper::scan_library(&library_dir);
    if candidates.is_empty() {
        anyhow::bail!("no wallpapers found in {}", library_dir.display());
    }

    // Read the current symlink target to exclude it from selection.
    let current_symlink = config::resolve_wallpaper_path(&cfg).ok();
    let current_canonical = current_symlink
        .as_ref()
        .and_then(|p| std::fs::canonicalize(p).ok());

    let mut usage = wallpaper::UsageTracker::load();
    let chosen = usage
        .pick(&candidates, current_canonical.as_deref())
        .ok_or_else(|| anyhow!("could not pick a wallpaper"))?;

    let canonical = std::fs::canonicalize(&chosen)
        .unwrap_or_else(|_| chosen.clone());

    wallpaper::update_current_symlink(&canonical)?;
    println!("{}", canonical.display());
    Ok(())
}

fn main() -> Result<()> {
    // Default to info-level logs unless RUST_LOG says otherwise.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = parse_cli()?;

    // Dispatch non-daemon modes.
    let overrides = match cli {
        CliMode::Ipc(cmd) => {
            return ipc::run_client(cmd);
        }
        CliMode::Rotate => {
            return run_rotate();
        }
        CliMode::Daemon { overrides } => overrides,
    };

    // Load config from file and apply CLI overrides.
    let mut cfg = config::load_config()?;
    let cli_overrides = overrides;
    apply_overrides(&mut cfg, &cli_overrides);

    let wallpaper_path = config::resolve_wallpaper_path(&cfg)?;
    let cfg_path = config::config_path()?;
    log::info!(
        "starting niri-wallpaper (path={}, normal_ns={}, backdrop_ns={})",
        wallpaper_path.display(),
        cfg.namespaces.normal,
        cfg.namespaces.backdrop,
    );

    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, event_queue) = registry_queue_init(&conn).context("registry_queue_init")?;
    let qh = event_queue.handle();

    // Set up inotify for both wallpaper path and config file watching.
    let (inotify, watches) = setup_inotify(&wallpaper_path, &cfg_path)?;

    let mut app = App::new(&globals, &qh, cfg)?;

    // Load the wallpaper from the configured path / symlink.
    // Use `niri-wallpaper rotate` before starting the daemon to pick a fresh
    // wallpaper (e.g., in the niri spawn-at-startup or a systemd service).
    {
        let qh_clone = qh.clone();
        if let Err(e) = app.change_wallpaper(None, &qh_clone) {
            log::warn!("initial image load failed: {:#}", e);
        }
    }

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("calloop event loop")?;
    let loop_handle: LoopHandle<App> = event_loop.handle();
    app.loop_handle = Some(loop_handle.clone());

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow!("insert wayland source: {}", e.error))?;

    let watches = std::sync::Arc::new(watches);
    let inotify_src = Generic::new(inotify, Interest::READ, Mode::Level);
    loop_handle
        .insert_source(inotify_src, {
            let watches = watches.clone();
            move |_readiness, inotify, app: &mut App| {
                // SAFETY: we only read events from the inotify instance and don't drop it.
                let inotify = unsafe { inotify.get_mut() };
                let mut buf = [0u8; 4096];
                let mut wallpaper_changed = false;
                let mut config_changed = false;
                loop {
                    match inotify.read_events(&mut buf) {
                        Ok(events) => {
                            let mut any = false;
                            for ev in events {
                                any = true;
                                let name = ev.name;
                                // Check if this event matches the wallpaper file.
                                let is_wp_dir = ev.wd == watches.wallpaper_wd
                                    || watches.config_wd.is_none();
                                if is_wp_dir
                                    && name == Some(watches.wallpaper_name.as_os_str())
                                {
                                    wallpaper_changed = true;
                                }
                                // Check if this event matches the config file.
                                let is_cfg_dir = watches
                                    .config_wd
                                    .as_ref()
                                    .map_or(true, |cwd| ev.wd == *cwd);
                                if is_cfg_dir
                                    && name == Some(watches.config_name.as_os_str())
                                {
                                    config_changed = true;
                                }
                            }
                            if !any {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            log::error!("inotify read: {}", e);
                            break;
                        }
                    }
                }

                if config_changed {
                    log::info!("config file changed, reloading");
                    std::thread::sleep(Duration::from_millis(50));
                    app.handle_config_reload(&cli_overrides);
                }

                if wallpaper_changed {
                    if app.suppress_next_wallpaper_event {
                        // This event was triggered by our own symlink update.
                        app.suppress_next_wallpaper_event = false;
                        log::debug!("suppressing self-triggered wallpaper event");
                    } else {
                        log::info!("wallpaper file changed, reloading");
                        // Small grace period: `ln -sfn` and editors may emit several events.
                        std::thread::sleep(Duration::from_millis(50));

                        // Avoid reloading the same wallpaper we already have
                        // (can happen if the symlink target didn't actually change).
                        let wp_path = config::resolve_wallpaper_path(&app.config).ok();
                        let canonical = wp_path.as_ref().and_then(|p| std::fs::canonicalize(p).ok());
                        let already_loaded = match (&canonical, &app.current_wallpaper) {
                            (Some(a), Some(b)) => a == b,
                            _ => false,
                        };

                        if already_loaded {
                            log::debug!("wallpaper unchanged, skipping reload");
                        } else {
                            let qh = app.qh.clone();
                            if let Err(e) = app.change_wallpaper(None, &qh) {
                                log::error!("reload failed: {:#}", e);
                            }
                        }
                    }
                }
                Ok(PostAction::Continue)
            }
        })
        .map_err(|e| anyhow!("insert inotify source: {}", e))?;

    // Set up IPC socket listener.
    let ipc_listener = ipc::create_listener()?;
    let ipc_src = Generic::new(ipc_listener, Interest::READ, Mode::Level);
    loop_handle
        .insert_source(ipc_src, |_readiness, listener, app: &mut App| {
            let listener = unsafe { listener.get_mut() };
            // Accept all pending connections.
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        app.handle_ipc(stream);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log::error!("IPC accept: {}", e);
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| anyhow!("insert IPC source: {}", e))?;

    // Set up cron schedule timer if configured.
    if let Some(ref schedule) = app.config.schedule.clone() {
        setup_schedule_timer(&loop_handle, &mut app.schedule_token, schedule);
    }

    log::info!("event loop ready");
    event_loop
        .run(None, &mut app, |_| {})
        .context("event loop")?;

    // Best-effort cleanup.
    ipc::cleanup_socket();
    Ok(())
}
