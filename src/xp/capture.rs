//! Platform-specific screen capture backends.
//!
//! Each platform has its own capture method:
//! - Windows: DXGI Desktop Duplication (via `windows` crate)
//! - macOS: CGDisplayCreateImage (via `core-graphics` crate)
//! - Linux: Stub (use native Wayland binary or portal screencast)

use anyhow::Result;

/// A captured screen frame in RGBA format.
pub struct CapturedScreen {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Trait for platform-specific screen capture.
pub trait ScreenCapture: Send {
    /// Capture the primary display. Returns RGBA pixel data.
    fn capture_primary(&mut self) -> Result<CapturedScreen>;
}

// ═══════════════════════════════════════════════════════════════════════════
// Windows: DXGI Desktop Duplication
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(all(target_os = "windows", feature = "capture-win"))]
pub mod platform_capture {
    use super::*;
    use anyhow::{Context, bail};
    use std::ffi::c_void;
    use std::ptr;

    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Direct3D::Fxc::*;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::core::Interface;

    /// Windows screen capture via DXGI Desktop Duplication.
    pub struct DxgiCapture {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        staging: ID3D11Texture2D,
        width: u32,
        height: u32,
    }

    impl DxgiCapture {
        fn create_device_and_duplication() -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication, u32, u32)> {
            unsafe {
                // Create D3D11 device.
                let mut feature_level = D3D_FEATURE_LEVEL_11_0;
                let mut device = None;
                let mut context = None;

                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut feature_level),
                    Some(&mut context),
                )
                .context("Failed to create D3D11 device")?;

                let device = device.context("No D3D11 device returned")?;
                let context = context.context("No D3D11 context returned")?;

                // Get DXGI adapter and output.
                let dxgi_device: IDXGIDevice = device.cast()?;
                let adapter: IDXGIAdapter = dxgi_device.GetParent()?;
                let output: IDXGIOutput = adapter.EnumOutputs(0)?;

                // Get output description for dimensions.
                let desc = output.GetDesc()?;
                let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;

                // Create output duplication.
                let output1: IDXGIOutput1 = output.cast()?;
                let duplication = output1.DuplicateOutput(&device)?;

                // Create staging texture for reading frames.
                let tex_desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut staging: Option<ID3D11Texture2D> = None;
                device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;
                let staging = staging.context("No staging texture")?;

                Ok((device, context, duplication, width, height))
            }
        }
    }

    impl DxgiCapture {
        pub fn init() -> Result<Self> {
            let (device, context, duplication, width, height) =
                Self::create_device_and_duplication()?;

            tracing::info!("DXGI Desktop Duplication initialized: {width}x{height}");

            // Create staging texture.
            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            unsafe {
                device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;
            }
            let staging = staging.context("No staging texture")?;

            Ok(Self { device, context, duplication, staging, width, height })
        }
    }

    impl ScreenCapture for DxgiCapture {
        fn capture_primary(&mut self) -> Result<CapturedScreen> {
            unsafe {
                // Acquire next frame.
                let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                self.duplication.AcquireNextFrame(
                    100, // timeout ms
                    &mut frame_info,
                    &mut resource,
                )?;
                let resource = resource.context("No frame resource")?;

                // Get the desktop texture.
                let desktop_tex: ID3D11Texture2D = resource.cast()?;

                // Copy to staging texture.
                self.context.CopyResource(&self.staging, &desktop_tex);
                self.duplication.ReleaseFrame()?;

                // Map staging texture to read pixels.
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(
                    &self.staging,
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )?;

                let src = mapped.pData as *const u8;
                let src_pitch = mapped.RowPitch as usize;
                let mut rgba = vec![0u8; (self.width * self.height * 4) as usize];

                // Copy BGRA → RGBA, handling pitch.
                for y in 0..self.height as usize {
                    let src_row = unsafe { std::slice::from_raw_parts(src.add(y * src_pitch), self.width as usize * 4) };
                    let dst_row = &mut rgba[y * self.width as usize * 4..][..self.width as usize * 4];
                    for x in 0..self.width as usize {
                        let si = x * 4;
                        dst_row[si] = src_row[si + 2]; // R ← B
                        dst_row[si + 1] = src_row[si + 1]; // G ← G
                        dst_row[si + 2] = src_row[si]; // B ← R
                        dst_row[si + 3] = src_row[si + 3]; // A ← A
                    }
                }

                self.context.Unmap(&self.staging, 0);

                Ok(CapturedScreen {
                    rgba,
                    width: self.width,
                    height: self.height,
                })
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// macOS: Core Graphics screen capture
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(all(target_os = "macos", feature = "capture-mac"))]
pub mod platform_capture {
    use super::*;
    use anyhow::bail;
    use core_graphics::display::{CGDirectDisplayID, CGMainDisplayID, CGDisplayPixelsWide, CGDisplayPixelsHigh};
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use std::ffi::c_void;

    // Bitmap info constant: premultiplied alpha, last component.
    const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;

    // Raw FFI declarations for Core Graphics functions.
    // We use raw pointers throughout to avoid type mismatches with the safe
    // core-graphics crate wrappers (which use foreign-types NonNull wrappers
    // that don't play well with our bitmap context workflow).
    unsafe extern "C" {
        fn CGDisplayCreateImage(display: CGDirectDisplayID) -> *mut c_void;
        fn CGImageGetWidth(image: *const c_void) -> usize;
        fn CGImageGetHeight(image: *const c_void) -> usize;
        fn CGImageRelease(image: *const c_void);
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *const c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *const c_void,
            bitmap_info: u32,
        ) -> *mut c_void;
        fn CGContextDrawImage(ctx: *const c_void, rect: CGRect, image: *const c_void);
        fn CGContextRelease(ctx: *const c_void);
    }

    pub struct MacosCapture {
        display_id: CGDirectDisplayID,
        width: u32,
        height: u32,
    }

    impl MacosCapture {
        pub fn init() -> Result<Self> {
            // SAFETY: CGMainDisplayID() is always safe.
            let display_id = unsafe { CGMainDisplayID() };
            let width = unsafe { CGDisplayPixelsWide(display_id) } as u32;
            let height = unsafe { CGDisplayPixelsHigh(display_id) } as u32;

            tracing::info!("macOS capture initialized: display {display_id} ({width}x{height})");

            Ok(Self { display_id, width, height })
        }
    }

    impl ScreenCapture for MacosCapture {
        fn capture_primary(&mut self) -> Result<CapturedScreen> {
            // SAFETY: CGDisplayCreateImage is always safe to call.
            let image = unsafe { CGDisplayCreateImage(self.display_id) };
            if image.is_null() {
                bail!("CGDisplayCreateImage failed — grant Screen Recording permission in \
                    System Settings → Privacy & Security → Screen Recording, then restart Maggie");
            }

            let img_width = unsafe { CGImageGetWidth(image) } as u32;
            let img_height = unsafe { CGImageGetHeight(image) } as u32;
            let bytes_per_row = img_width * 4;
            let mut rgba = vec![0u8; (bytes_per_row * img_height) as usize];

            unsafe {
                let cs = CGColorSpaceCreateDeviceRGB();
                if cs.is_null() {
                    CGImageRelease(image);
                    bail!("Failed to create RGB color space");
                }

                let ctx = CGBitmapContextCreate(
                    rgba.as_mut_ptr() as *mut c_void,
                    img_width as usize,
                    img_height as usize,
                    8,
                    bytes_per_row as usize,
                    cs as *const c_void,
                    K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
                );
                if ctx.is_null() {
                    CGImageRelease(image);
                    CGColorSpaceRelease(cs);
                    bail!("Failed to create bitmap context");
                }

                let rect = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(
                    img_width as f64,
                    img_height as f64,
                ));
                CGContextDrawImage(ctx, rect, image);

                CGContextRelease(ctx);
                CGColorSpaceRelease(cs);
                CGImageRelease(image);
            }

            // CGImages are bottom-up; flip to top-down.
            rgba.chunks_exact_mut(bytes_per_row as usize)
                .collect::<Vec<_>>()
                .reverse();

            Ok(CapturedScreen { rgba, width: img_width, height: img_height })
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Linux: XDG Desktop Portal + PipeWire
// ═══════════════════════════════════════════════════════════════════════════

// When capture-linux is enabled, use the XDG Desktop Portal ScreenCast API
// (via ashpd) to negotiate a PipeWire stream with the compositor. This works
// on GNOME, KDE, Sway, Hyprland, Niri, COSMIC, and other modern Wayland
// compositors — as well as X11 sessions that have xdg-desktop-portal installed.
//
// Without capture-linux, fall back to a stub that directs the user to the
// native Wayland binary.

#[cfg(all(target_os = "linux", feature = "capture-linux"))]
pub mod platform_capture {
    use super::*;
    use anyhow::{Context, bail};
    use ashpd::desktop::{
        PersistMode,
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    };
    use pipewire as pw;
    use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
    use std::thread;

    /// Shared state between the PipeWire thread and the main thread.
    struct SharedState {
        frame: Option<CapturedScreen>,
        width: u32,
        height: u32,
    }

    /// Linux screen capture via XDG Desktop Portal ScreenCast + PipeWire.
    pub struct LinuxScreenCapture {
        shared: Arc<Mutex<SharedState>>,
        running: Arc<AtomicBool>,
        _pw_thread: thread::JoinHandle<()>,
    }

    impl LinuxScreenCapture {
        pub fn init() -> Result<Self> {
            // Use a short-lived tokio runtime for the async ashpd portal call.
            let (node_id, fd) = {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("Failed to create tokio runtime for portal call")?;
                rt.block_on(open_portal())
                    .context("Failed to open XDG Desktop Portal screencast session")?
            };

            tracing::info!("Linux capture: PipeWire node_id={node_id}");

            let shared = Arc::new(Mutex::new(SharedState {
                frame: None,
                width: 0,
                height: 0,
            }));
            let running = Arc::new(AtomicBool::new(true));

            let shared_clone = shared.clone();
            let running_clone = running.clone();

            let pw_thread = thread::Builder::new()
                .name("maggie-pipewire".into())
                .spawn(move || {
                    if let Err(e) = run_pipewire(node_id, fd, shared_clone, running_clone) {
                        tracing::error!("PipeWire capture thread failed: {e:#}");
                    }
                })
                .context("Failed to spawn PipeWire thread")?;

            Ok(Self {
                shared,
                running,
                _pw_thread: pw_thread,
            })
        }
    }

    impl ScreenCapture for LinuxScreenCapture {
        fn capture_primary(&mut self) -> Result<CapturedScreen> {
            let state = self.shared.lock().map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
            state.frame.as_ref().map(|f| CapturedScreen {
                rgba: f.rgba.clone(),
                width: f.width,
                height: f.height,
            }).ok_or_else(|| anyhow::anyhow!("No frame captured yet — waiting for PipeWire stream"))
        }
    }

    impl Drop for LinuxScreenCapture {
        fn drop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
        }
    }

    // ── Portal session ──────────────────────────────────────────────────────

    /// Open an XDG Desktop Portal ScreenCast session and return the PipeWire
    /// node ID and remote file descriptor.
    async fn open_portal() -> Result<(u32, std::os::fd::OwnedFd)> {
        let proxy = Screencast::new().await
            .context("Failed to connect to org.freedesktop.portal.ScreenCast")?;
        let session = proxy.create_session(Default::default()).await
            .context("Failed to create screencast session")?;

        proxy.select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(Some(SourceType::Monitor).map(Into::into))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        ).await.context("Failed to select sources")?.response()
            .context("Source selection was dismissed or failed")?;

        let response = proxy
            .start(&session, None, Default::default()).await
            .context("Failed to start screencast")?
            .response()
            .context("Screencast start was dismissed or failed")?;

        let stream = response.streams()
            .first()
            .context("Portal returned no streams")
            .cloned()?;

        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await
            .context("Failed to open PipeWire remote")?;

        Ok((stream.pipe_wire_node_id(), fd))
    }

    // ── PipeWire streaming ──────────────────────────────────────────────────

    /// Run the PipeWire main loop on this thread, receiving frames from the
    /// compositor and writing the latest one into `shared`.
    fn run_pipewire(
        node_id: u32,
        fd: std::os::fd::OwnedFd,
        shared: Arc<Mutex<SharedState>>,
        running: Arc<AtomicBool>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopBox::new(None)
            .context("Failed to create PipeWire main loop")?;
        let context = pw::context::ContextBox::new(&mainloop.loop_(), None)
            .context("Failed to create PipeWire context")?;
        let core = context.connect_fd(fd, None)
            .context("Failed to connect to PipeWire remote")?;

        let stream = pw::stream::StreamBox::new(
            &core,
            "maggie-capture",
            pw::properties::properties!
            {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        ).context("Failed to create PipeWire stream")?;

        // State carried into PipeWire callbacks via user_data.
        struct PwData {
            format: pw::spa::param::video::VideoInfoRaw,
            shared: Arc<Mutex<SharedState>>,
            running: Arc<AtomicBool>,
        }

        let user_data = PwData {
            format: Default::default(),
            shared,
            running: running.clone(),
        };

        let _listener = stream
            .add_local_listener_with_user_data(user_data)
            .state_changed(|_, data, _old, new| {
                tracing::debug!("PipeWire stream state: {new:?}");
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    data.running.store(false, Ordering::SeqCst);
                }
            })
            .param_changed(move |_, data, id, param| {
                let Some(param) = param else { return; };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) =
                    match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                if media_type != pw::spa::param::format::MediaType::Video
                    || media_subtype != pw::spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                data.format.parse(param).expect("Failed to parse video format");
                let w = data.format.size().width;
                let h = data.format.size().height;
                let fmt = data.format.format();
                tracing::info!("PipeWire video format: {fmt:?} {w}x{h}");
                if let Ok(mut state) = data.shared.lock() {
                    state.width = w;
                    state.height = h;
                }
            })
            .process(move |stream, data| {
                if !data.running.load(Ordering::SeqCst) {
                    return;
                }
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    tracing::warn!("PipeWire: no buffers available");
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let chunk_size = {
                    let data0 = &mut datas[0];
                    data0.chunk().size() as usize
                };
                if chunk_size == 0 {
                    return;
                }
                let ptr = datas[0].data().unwrap_or(&mut []);
                let w = data.format.size().width;
                let h = data.format.size().height;
                let stride = w * 4; // We negotiate RGBA
                let expected = (stride * h) as usize;
                let raw = if ptr.len() >= chunk_size {
                    &ptr[..chunk_size]
                } else {
                    &ptr[..]
                };
                let rgba = if raw.len() >= expected {
                    raw[..expected].to_vec()
                } else {
                    // Partial frame — pad with zeros.
                    let mut buf = vec![0u8; expected];
                    buf[..raw.len()].copy_from_slice(raw);
                    buf
                };
                if let Ok(mut state) = data.shared.lock() {
                    state.frame = Some(CapturedScreen { rgba, width: w, height: h });
                }
            })
            .register()
            .context("Failed to register PipeWire stream listener")?;

        // Negotiate format: prefer RGBA, also accept RGB and BGRx.
        use pw::spa::{pod, utils::{SpaTypes, Fraction, Rectangle}};
        let obj = pod::object!
        {
            SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                pw::spa::param::video::VideoFormat::RGBA,
                pw::spa::param::video::VideoFormat::RGBA,
                pw::spa::param::video::VideoFormat::RGB,
                pw::spa::param::video::VideoFormat::RGBx,
                pw::spa::param::video::VideoFormat::BGRx
            ),
            pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                Rectangle { width: 1920, height: 1080 },
                Rectangle { width: 1, height: 1 },
                Rectangle { width: 7680, height: 4320 }
            ),
            pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                Fraction { num: 30, denom: 1 },
                Fraction { num: 0, denom: 1 },
                Fraction { num: 120, denom: 1 }
            ),
        };

        let values: Vec<u8> = pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pod::Value::Object(obj),
        ).context("Failed to serialize format negotiation pod")?
            .0.into_inner();

        let mut params = [pod::Pod::from_bytes(&values)
            .context("Failed to parse serialized pod")?];

        stream.connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        ).context("Failed to connect PipeWire stream")?;

        tracing::info!("PipeWire stream connected, entering main loop");

        // Run until the running flag is cleared (e.g. on drop).
        while running.load(Ordering::SeqCst) {
            mainloop.loop_().iterate(std::time::Duration::from_millis(100));
        }

        tracing::info!("PipeWire capture thread exiting");
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Linux: stub (when capture-linux feature is NOT enabled)
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(all(target_os = "linux", not(feature = "capture-linux")))]
pub mod platform_capture {
    use super::*;

    pub struct LinuxCapture;

    impl LinuxCapture {
        pub fn init() -> anyhow::Result<Self> { Ok(Self) }
    }

    impl ScreenCapture for LinuxCapture {
        fn capture_primary(&mut self) -> anyhow::Result<CapturedScreen> {
            anyhow::bail!("Use the native Wayland binary (maggie) for screen capture on Linux")
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Factory
// ═══════════════════════════════════════════════════════════════════════════

/// Create the platform-appropriate screen capture backend.
pub fn create_capture() -> Result<Box<dyn ScreenCapture>> {
    #[cfg(all(target_os = "windows", feature = "capture-win"))]
    { return Ok(Box::new(platform_capture::DxgiCapture::init()?)); }
    #[cfg(all(target_os = "macos", feature = "capture-mac"))]
    { return Ok(Box::new(platform_capture::MacosCapture::init()?)); }
    #[cfg(all(target_os = "linux", feature = "capture-linux"))]
    { return Ok(Box::new(platform_capture::LinuxScreenCapture::init()?)); }
    #[cfg(all(target_os = "linux", not(feature = "capture-linux")))]
    { return Ok(Box::new(platform_capture::LinuxCapture::init()?)); }
    // Fallback: no capture backend available
    anyhow::bail!("No capture backend available for this platform/feature configuration")
}
