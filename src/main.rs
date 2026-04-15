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
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::RgbaImage;

use calloop::{generic::Generic, EventLoop, Interest, LoopHandle, Mode, PostAction};
use calloop_wayland_source::WaylandSource;
use inotify::{Inotify, WatchMask};

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

mod blur;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Args {
    /// Symlink (or regular file) to the wallpaper. Watched for changes.
    path: PathBuf,
    normal_namespace: String,
    backdrop_namespace: String,
    /// Downsample factor applied before blurring (higher = softer + faster).
    blur_downsample: u32,
    /// Box-blur radius in downsampled pixels.
    blur_radius: u32,
    /// Number of box-blur passes (3 ≈ Gaussian).
    blur_iterations: u32,
}

impl Args {
    fn parse() -> Result<Self> {
        let config_dir = dirs_config_home()?;
        let default_path = config_dir.join("niri-wallpaper").join("current");

        let mut out = Self {
            path: default_path,
            normal_namespace: "wallpaper-bg".to_string(),
            backdrop_namespace: "wallpaper".to_string(),
            blur_downsample: 8,
            blur_radius: 6,
            blur_iterations: 3,
        };

        let argv: Vec<String> = std::env::args().collect();
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
                    out.path = PathBuf::from(next()?);
                    i += 2;
                }
                "--normal-namespace" => {
                    out.normal_namespace = next()?;
                    i += 2;
                }
                "--backdrop-namespace" => {
                    out.backdrop_namespace = next()?;
                    i += 2;
                }
                "--blur-downsample" => {
                    out.blur_downsample = next()?.parse().context("--blur-downsample u32")?;
                    i += 2;
                }
                "--blur-radius" => {
                    out.blur_radius = next()?.parse().context("--blur-radius u32")?;
                    i += 2;
                }
                "--blur-iterations" => {
                    out.blur_iterations = next()?.parse().context("--blur-iterations u32")?;
                    i += 2;
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument: {}", other)),
            }
        }
        Ok(out)
    }
}

fn print_help() {
    println!(
        "niri-wallpaper — dual-layer wallpaper daemon for niri

USAGE:
    niri-wallpaper [OPTIONS]

OPTIONS:
    -p, --path <PATH>              Path to wallpaper (symlink OK). Watched for changes.
                                   [default: ~/.config/niri-wallpaper/current]
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
"
    );
}

fn dirs_config_home() -> Result<PathBuf> {
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

/// The Backdrop surface is already heavily blurred, so any interpolation on
/// upscale produces the same visual result — use the cheapest algorithm.
fn alg_for(role: SurfaceRole) -> ResizeAlg {
    match role {
        SurfaceRole::Normal => ResizeAlg::Convolution(FilterType::Bilinear),
        SurfaceRole::Backdrop => ResizeAlg::Nearest,
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

    args: Args,

    /// Currently loaded image (full-res RGBA). Kept for re-renders on
    /// configure / hotplug; re-decoded from disk on each wallpaper change.
    image: Option<RgbaImage>,
    /// Blurred, downsampled version of `image`. Small (≈ 1 MB). Cover-scaled
    /// per output at draw time.
    blurred: Option<RgbaImage>,

    /// wl_surface id → OutputSurfaces index.
    surface_to_output: HashMap<u32, usize>,
    outputs: Vec<OutputSurfaces>,
}

impl App {
    fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Self>,
        args: Args,
    ) -> Result<Self> {
        let registry_state = RegistryState::new(globals);
        let output_state = OutputState::new(globals, qh);
        let compositor_state =
            CompositorState::bind(globals, qh).context("wl_compositor not available")?;
        let layer_shell =
            LayerShell::bind(globals, qh).context("zwlr_layer_shell_v1 not available")?;
        let shm = Shm::bind(globals, qh).context("wl_shm not available")?;

        Ok(Self {
            registry_state,
            output_state,
            compositor_state,
            layer_shell,
            shm,
            args,
            image: None,
            blurred: None,
            surface_to_output: HashMap::new(),
            outputs: Vec::new(),
        })
    }

    /// (Re)load the image from disk and regenerate the blurred preview.
    fn reload_image(&mut self) -> Result<()> {
        let canonical = std::fs::canonicalize(&self.args.path)
            .with_context(|| format!("resolve {}", self.args.path.display()))?;
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
            self.args.blur_downsample,
            self.args.blur_radius,
            self.args.blur_iterations,
        );
        log::info!(
            "blurred {}x{} → {}x{} in {:?}",
            img.width(),
            img.height(),
            blurred.width(),
            blurred.height(),
            t0.elapsed()
        );

        self.image = Some(img);
        self.blurred = Some(blurred);

        // Reclaim the decode/blur heap cache back to the OS.
        trim_heap();
        Ok(())
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let normal_layer = self.create_layer(qh, &output, &self.args.normal_namespace);
        let backdrop_layer = self.create_layer(qh, &output, &self.args.backdrop_namespace);

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
    ) -> LayerSurface {
        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
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
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
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

/// We watch the *parent directory* of the target path for `MOVED_TO | CREATE`
/// events matching the target's basename. This works with `ln -sfn` (which uses
/// rename(2)) and also with plain file replacements.
fn setup_inotify(path: &Path) -> Result<(Inotify, PathBuf, std::ffi::OsString)> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("wallpaper path has no parent: {}", path.display()))?
        .to_path_buf();
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("wallpaper path has no file name: {}", path.display()))?
        .to_os_string();

    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create {}", parent.display()))?;

    let inotify = Inotify::init().context("inotify init")?;
    inotify
        .watches()
        .add(
            &parent,
            WatchMask::MOVED_TO
                | WatchMask::CREATE
                | WatchMask::CLOSE_WRITE
                | WatchMask::ATTRIB,
        )
        .with_context(|| format!("inotify watch {}", parent.display()))?;

    Ok((inotify, parent, name))
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Default to info-level logs unless RUST_LOG says otherwise.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse()?;
    log::info!(
        "starting niri-wallpaper (path={}, normal_ns={}, backdrop_ns={})",
        args.path.display(),
        args.normal_namespace,
        args.backdrop_namespace,
    );

    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, event_queue) = registry_queue_init(&conn).context("registry_queue_init")?;
    let qh = event_queue.handle();

    // Set up inotify before moving `args` into `App`.
    let (inotify, _parent, watch_name) = setup_inotify(&args.path)?;

    let mut app = App::new(&globals, &qh, args)?;

    // Try to load the image now. It's OK if the file doesn't exist yet — the
    // inotify watcher will pick it up when the picker script creates it.
    if let Err(e) = app.reload_image() {
        log::warn!("initial image load failed: {:#}", e);
    }

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("calloop event loop")?;
    let loop_handle: LoopHandle<App> = event_loop.handle();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow!("insert wayland source: {}", e.error))?;
    let watch_name = std::sync::Arc::new(watch_name);
    let inotify_src = Generic::new(inotify, Interest::READ, Mode::Level);
    loop_handle
        .insert_source(inotify_src, {
            let watch_name = watch_name.clone();
            move |_readiness, inotify, app: &mut App| {
                // SAFETY: we only read events from the inotify instance and don't drop it.
                let inotify = unsafe { inotify.get_mut() };
                let mut buf = [0u8; 4096];
                let mut matched = false;
                loop {
                    match inotify.read_events(&mut buf) {
                        Ok(events) => {
                            let mut any = false;
                            for ev in events {
                                any = true;
                                if ev.name == Some(watch_name.as_os_str()) {
                                    matched = true;
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
                if matched {
                    log::info!("wallpaper file changed, reloading");
                    // Small grace period: `ln -sfn` and editors may emit several events.
                    std::thread::sleep(Duration::from_millis(50));
                    match app.reload_image() {
                        Ok(()) => app.redraw_all(),
                        Err(e) => log::error!("reload failed: {:#}", e),
                    }
                }
                Ok(PostAction::Continue)
            }
        })
        .map_err(|e| anyhow!("insert inotify source: {}", e))?;

    log::info!("event loop ready");
    event_loop
        .run(None, &mut app, |_| {})
        .context("event loop")?;
    Ok(())
}
