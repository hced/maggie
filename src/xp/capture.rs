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
                let mut output: Option<IDXGIOutput> = None;
                adapter.EnumOutputs(0, &mut output)?;
                let output = output.context("No DXGI output")?;

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
                    BindFlags: D3D11_BIND_FLAG(0),
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ,
                    MiscFlags: D3D11_RESOURCE_MISC_FLAG(0),
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
                BindFlags: D3D11_BIND_FLAG(0),
                CPUAccessFlags: D3D11_CPU_ACCESS_READ,
                MiscFlags: D3D11_RESOURCE_MISC_FLAG(0),
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
                    Some(&mut resource),
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
    use anyhow::{Context, bail};

    pub struct MacosCapture {
        display_id: u32,
        width: u32,
        height: u32,
    }

    impl MacosCapture {
        pub fn init() -> Result<Self> {
            // SAFETY: CGMainDisplayID() is always safe.
            let display_id = unsafe { core_graphics_sys::display::CGMainDisplayID() };
            let width = unsafe { core_graphics_sys::display::CGDisplayPixelsWide(display_id) } as u32;
            let height = unsafe { core_graphics_sys::display::CGDisplayPixelsHigh(display_id) } as u32;

            tracing::info!("macOS capture initialized: display {display_id} ({width}x{height})");

            Ok(Self { display_id, width, height })
        }
    }

    impl ScreenCapture for MacosCapture {
        fn capture_primary(&mut self) -> Result<CapturedScreen> {
            // SAFETY: CGDisplayCreateImage returns an owned CGImageRef.
            let image = unsafe {
                core_graphics_sys::image::CGDisplayCreateImage(self.display_id)
            };
            if image.is_null() {
                bail!("CGDisplayCreateImage failed");
            }

            let img_width = unsafe { core_graphics_sys::image::CGImageGetWidth(image) } as u32;
            let img_height = unsafe { core_graphics_sys::image::CGImageGetHeight(image) } as u32;
            let bytes_per_row = img_width * 4;
            let mut rgba = vec![0u8; (bytes_per_row * img_height) as usize];

            unsafe {
                let cs = core_graphics_sys::color_space::CGColorSpaceCreateDeviceRGB();
                let ctx = core_graphics_sys::context::CGBitmapContextCreate(
                    rgba.as_mut_ptr() as *mut _,
                    img_width as usize,
                    img_height as usize,
                    8,
                    bytes_per_row as usize,
                    cs,
                    core_graphics_sys::base::kCGImageAlphaPremultipliedLast,
                );
                if ctx.is_null() {
                    core_graphics_sys::base::CGImageRelease(image);
                    core_graphics_sys::base::CGColorSpaceRelease(cs);
                    bail!("Failed to create bitmap context");
                }

                let rect = core_graphics_sys::geometry::CGRect {
                    origin: core_graphics_sys::geometry::CGPoint { x: 0.0, y: 0.0 },
                    size: core_graphics_sys::geometry::CGSize {
                        width: img_width as f64,
                        height: img_height as f64,
                    },
                };
                core_graphics_sys::context::CGContextDrawImage(ctx, rect, image);

                core_graphics_sys::base::CGContextRelease(ctx);
                core_graphics_sys::base::CGColorSpaceRelease(cs);
                core_graphics_sys::base::CGImageRelease(image);
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
// Linux: stub
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    { return Ok(Box::new(platform_capture::LinuxCapture::init()?)); }
    // Fallback: no capture backend available
    anyhow::bail!("No capture backend available for this platform/feature configuration")
}


