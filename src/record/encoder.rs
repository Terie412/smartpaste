//! Media Foundation 编码：H.264 + MP4，输入是 DXGI surface，全程 GPU。

use std::ffi::c_void;
use windows::core::{Interface, Result, GUID, PCWSTR};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    eAVEncH264VProfile_High, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSinkWriter,
    IMFTransform, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Video,
    MFVideoFormat_RGB32, MFVideoFormat_H264, MFVideoInterlace_Progressive, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SINK_WRITER_D3D_MANAGER, MFT_ENUM_HARDWARE_URL_Attribute, MFT_FRIENDLY_NAME_Attribute,
};

pub struct Encoder {
    writer: IMFSinkWriter,
    stream: u32,
    /// 一帧的字节数，用来补 SetCurrentLength
    frame_bytes: u32,
    pub hardware: bool,
    pub encoder: String,
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

impl Encoder {
    pub fn new(
        path: &str,
        device: &ID3D11Device,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        unsafe {
            let mut token = 0u32;
            let mut mgr: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.unwrap();
            mgr.ResetDevice(device, token)?;

            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 3)?;
            let attrs = attrs.unwrap();
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            if std::env::args().any(|a| a == "--soft") {
                attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)?;
            }
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &mgr)?;

            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let writer = MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)?;
            crate::win::trace("encoder: sink writer ok");

            let out: IMFMediaType = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;

            let stream = writer.AddStream(&out)?;
            crate::win::trace("encoder: add stream ok");

            let inp: IMFMediaType = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;

            writer.SetInputMediaType(stream, &inp, None)?;
            crate::win::trace("encoder: input type ok");
            writer.BeginWriting()?;

            let (hardware, encoder) = identify(&writer, stream);
            Ok(Self {
                writer,
                stream,
                frame_bytes: w * h * 4,
                hardware,
                encoder,
            })
        }
    }

    pub fn write(&mut self, tex: &ID3D11Texture2D, time_100ns: i64, dur_100ns: i64) -> Result<()> {
        unsafe {
            let buffer = match MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false) {
                Ok(b) => b,
                Err(e) => {
                    crate::win::trace(&format!("write: MFCreateDXGISurfaceBuffer 失败 {e:?}"));
                    return Err(e);
                }
            };
            // DXGI buffer 的 CurrentLength 默认是 0，不补这一句 WriteSample 会返回 E_INVALIDARG
            if let Err(e) = buffer.SetCurrentLength(self.frame_bytes) {
                crate::win::trace(&format!("write: SetCurrentLength 失败 {e:?}"));
                return Err(e);
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            sample.SetSampleDuration(dur_100ns)?;
            match self.writer.WriteSample(self.stream, &sample) {
                Ok(()) => Ok(()),
                Err(e) => {
                    crate::win::trace(&format!("write: WriteSample 失败 {e:?}"));
                    Err(e)
                }
            }
        }
    }

    pub fn finish(&self) -> Result<()> {
        unsafe { self.writer.Finalize() }
    }
}

/// 读编码器信息。`MFT_ENUM_HARDWARE_URL_Attribute` 存在就说明是硬件 MFT。
fn identify(writer: &IMFSinkWriter, stream: u32) -> (bool, String) {
    unsafe {
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let zero = GUID::zeroed();
        if writer
            .GetServiceForStream(stream, &zero, &IMFTransform::IID, &mut ptr)
            .is_err()
            || ptr.is_null()
        {
            return (false, "unknown".into());
        }
        let tf = IMFTransform::from_raw(ptr);
        let Ok(attrs) = tf.GetAttributes() else {
            return (false, "unknown".into());
        };

        let mut buf = [0u16; 256];
        let encoder = attrs
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None)
            .map(|_| {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                String::from_utf16_lossy(&buf[..len])
            })
            .unwrap_or_else(|_| "unknown".into());

        // MFT_ENUM_HARDWARE_URL_Attribute 不是所有厂商都写，按名字再兜一层
        let hardware = attrs.GetUINT32(&MFT_ENUM_HARDWARE_URL_Attribute).is_ok()
            || ["NVIDIA", "Intel", "AMD", "Hardware", "Quick Sync"]
                .iter()
                .any(|k| encoder.contains(k));

        (hardware, encoder)
    }
}
