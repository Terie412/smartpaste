//! DXGI Desktop Duplication 采集。
//!
//! 一个 Capturer 对应一块显示器上的一个选区。帧始终留在 GPU 上：
//! `AcquireNextFrame` 拿到整屏纹理，`CopySubresourceRegion` 把选区裁到画布。
//! 画布是多张轮转的——SinkWriter 可能还握着上一张不放。

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{E_INVALIDARG, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

/// 同时存在的画布数量。MF 是异步的，队列可能压好几帧，宁可多留几张。
pub const CANVAS_COUNT: usize = 6;

pub struct Capturer {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// 最近一次 DDA 的原始画面，永远不带光标
    clean: ID3D11Texture2D,
    /// 交给编码器的成品，轮转使用
    outputs: Vec<ID3D11Texture2D>,
    staging: ID3D11Texture2D,
    /// 光标块的上传缓冲。用 CopySubresourceRegion 贴回画布——
    /// UpdateSubresource 的更新 MF 读不到（画面能更新，局部块却是旧的）。
    upload: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// 选区左上角在显示器上的偏移
    src_x: u32,
    src_y: u32,
    /// 已经拿到过至少一帧
    pub primed: bool,
}

impl Capturer {
    /// `x, y, w, h` 是屏幕物理像素坐标。选区必须完整落在一块显示器内。
    pub fn new(x: i32, y: i32, w: u32, h: u32, block: u32) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            crate::win::trace("dda: factory ok");

            let mut found: Option<(IDXGIAdapter1, u32, RECT)> = None;
            let mut ai = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                let mut oi = 0u32;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    if let Ok(desc) = output.GetDesc() {
                        let r = desc.DesktopCoordinates;
                        if x >= r.left && y >= r.top && x + w as i32 <= r.right && y + h as i32 <= r.bottom {
                            found = Some((adapter.clone(), oi, r));
                            crate::win::trace(&format!(
                                "dda: adapter={ai} output={oi} rect=({},{},{},{})",
                                r.left, r.top, r.right, r.bottom
                            ));
                            break;
                        }
                    }
                    oi += 1;
                }
                if found.is_some() {
                    break;
                }
                ai += 1;
            }

            let Some((adapter, output_index, out_rect)) = found else {
                return Err(windows::core::Error::new(
                    E_INVALIDARG,
                    "选区没有完整落在一块显示器内（v1 不支持跨屏）",
                ));
            };

            let base: IDXGIAdapter = adapter.cast()?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                Some(&base),
                // 指定了 adapter 时 driver type 必须是 UNKNOWN，否则 E_INVALIDARG
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let context = context.unwrap();
            // MF 在另一个线程读纹理，必须打开多线程保护，否则读到旧内容（光标会消失）
            if let Ok(mt) = device.cast::<windows::Win32::Graphics::Direct3D10::ID3D10Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            crate::win::trace(&format!("dda: device ok level={level:?}"));

            let output = adapter.EnumOutputs(output_index)?;
            let output1: IDXGIOutput1 = output.cast()?;
            let dup = output1.DuplicateOutput(&device)?;
            crate::win::trace("dda: duplication ok");

            let desc = dup.GetDesc();
            if desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return Err(windows::core::Error::new(
                    E_INVALIDARG,
                    format!(
                        "显示器不是 BGRA 8bit（看到 {:?}）。HDR 屏幕 v1 不支持，请先关闭 HDR",
                        desc.ModeDesc.Format
                    ),
                ));
            }

            let make_tex =
                |usage: D3D11_USAGE, bind: u32, cpu: u32, tw: u32, th: u32| -> Result<ID3D11Texture2D> {
                    let d = D3D11_TEXTURE2D_DESC {
                        Width: tw,
                        Height: th,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: usage,
                        BindFlags: bind,
                        CPUAccessFlags: cpu,
                        MiscFlags: 0,
                    };
                    let mut tex: Option<ID3D11Texture2D> = None;
                    device.CreateTexture2D(&d, None, Some(&mut tex))?;
                    Ok(tex.unwrap())
                };

            let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let clean = make_tex(D3D11_USAGE_DEFAULT, bind, 0, w, h)?;
            let mut outputs = Vec::with_capacity(CANVAS_COUNT);
            for _ in 0..CANVAS_COUNT {
                outputs.push(make_tex(D3D11_USAGE_DEFAULT, bind, 0, w, h)?);
            }
            let staging = make_tex(
                D3D11_USAGE_STAGING,
                0,
                D3D11_CPU_ACCESS_READ.0 as u32,
                block,
                block,
            )?;
            // DYNAMIC 纹理不能当 CopySubresourceRegion 的源，这里必须用 DEFAULT
            let upload = make_tex(D3D11_USAGE_DEFAULT, 0, 0, block, block)?;
            crate::win::trace("dda: textures ok");

            Ok(Self {
                device,
                context,
                dup,
                clean,
                outputs,
                staging,
                upload,
                width: w,
                height: h,
                src_x: (x - out_rect.left) as u32,
                src_y: (y - out_rect.top) as u32,
                primed: false,
            })
        }
    }

    /// 抓一帧到 clean。返回 false 表示这次没有新帧（超时）。
    pub fn grab(&mut self, timeout_ms: u32) -> Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {
                    if let Some(res) = res {
                        let tex: ID3D11Texture2D = match res.cast() {
                            Ok(t) => t,
                            Err(e) => {
                                crate::win::trace(&format!("grab: cast 失败 {e:?}"));
                                let _ = self.dup.ReleaseFrame();
                                return Err(e);
                            }
                        };
                        let region = D3D11_BOX {
                            left: self.src_x,
                            top: self.src_y,
                            front: 0,
                            right: self.src_x + self.width,
                            bottom: self.src_y + self.height,
                            back: 1,
                        };
                        self.context.CopySubresourceRegion(
                            &self.clean,
                            0,
                            0,
                            0,
                            0,
                            &tex,
                            0,
                            Some(&region),
                        );
                        self.primed = true;
                    } else {
                        crate::win::trace("grab: 帧资源为 null");
                    }
                    let _ = self.dup.ReleaseFrame();
                    Ok(true)
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(false),
                Err(e) => {
                    crate::win::trace(&format!("grab: AcquireNextFrame 失败 {e:?}"));
                    Err(e)
                }
            }
        }
    }

    /// 把干净画面整张拷到一张输出画布。
    pub fn copy_clean_to(&mut self, slot: usize) {
        unsafe {
            self.context.CopyResource(&self.outputs[slot], &self.clean);
        }
    }

    pub fn canvas(&self, slot: usize) -> &ID3D11Texture2D {
        &self.outputs[slot]
    }

    /// 把画布上的一块读回 CPU（BGRA）。`w/h` 不能超过 `block`。
    pub fn read_block(&mut self, slot: usize, x: u32, y: u32, w: u32, h: u32, out: &mut [u32]) -> Result<()> {
        unsafe {
            let region = D3D11_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + w,
                bottom: y + h,
                back: 1,
            };
            self.context.CopySubresourceRegion(
                &self.staging,
                0,
                0,
                0,
                0,
                &self.outputs[slot],
                0,
                Some(&region),
            );

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;
            for row in 0..h as usize {
                let s = src.add(row * pitch) as *const u32;
                std::ptr::copy_nonoverlapping(s, out.as_mut_ptr().add(row * w as usize), w as usize);
            }
            self.context.Unmap(&self.staging, 0);
            Ok(())
        }
    }

    /// 把一块 BGRA 写回画布。
    pub fn write_block(&mut self, slot: usize, x: u32, y: u32, w: u32, h: u32, data: &[u32]) {
        unsafe {
            self.context.UpdateSubresource(
                &self.upload,
                0,
                None,
                data.as_ptr() as *const std::ffi::c_void,
                w * 4,
                w * h * 4,
            );

            let src = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            self.context.CopySubresourceRegion(
                &self.outputs[slot],
                0,
                x,
                y,
                0,
                &self.upload,
                0,
                Some(&src),
            );
            self.context.Flush();
        }
    }
}
