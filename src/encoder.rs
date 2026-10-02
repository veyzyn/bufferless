//! Hardware H.264 encoding through Media Foundation. Hardware MFTs wrap
//! NVENC / AMF / QuickSync, so this works on any vendor's GPU. Input frames
//! are NV12 D3D11 textures, so pixels never leave the GPU.

use crate::prelude::*;

use core::mem::ManuallyDrop;

use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, Result};

use crate::log;
use crate::ring::Packet;

pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

pub struct H264Encoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    _device_manager: IMFDXGIDeviceManager,
    input_id: u32,
    output_id: u32,
    /// Number of METransformNeedInput events we haven't answered yet.
    need_input: u32,
    provides_samples: bool,
    output_size: u32,
    pub name: String,
}

fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

/// Enumerate hardware H.264 encoders living on the given adapter.
fn find_encoders(adapter: LUID) -> Result<Vec<IMFActivate>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.unwrap();
        let luid_bytes = core::slice::from_raw_parts(&adapter as *const LUID as *const u8, size_of::<LUID>());
        attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid_bytes)?;

        let mut list: *mut Option<IMFActivate> = core::ptr::null_mut();
        let mut count = 0u32;
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attrs,
            &mut list,
            &mut count,
        )?;
        let mut out = Vec::new();
        for i in 0..count as usize {
            if let Some(a) = core::ptr::read(list.add(i)) {
                out.push(a);
            }
        }
        if !list.is_null() {
            CoTaskMemFree(Some(list as _));
        }
        Ok(out)
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let mut len = 0u32;
        if activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
            return String::from_utf16_lossy(&buf[..len as usize]);
        }
    }
    "unknown encoder".into()
}

fn set_codec_api(codec: &ICodecAPI, s: &EncoderSettings, verbose: bool) {
    let set = |api: &windows::core::GUID, v: VARIANT, what: &str| unsafe {
        if let Err(e) = codec.SetValue(api, &v) {
            if verbose {
                log!("encoder: couldn't set {what}: {e}");
            }
        }
    };
    set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32), "rate control");
    set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(s.bitrate), "bitrate");
    // One keyframe per second: clips start at most 1s earlier than asked.
    set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(s.fps), "GOP size");
    // No B-frames keeps latency down. Low latency mode implies it on most
    // encoders anyway (NVIDIA rejects the explicit setting), and the muxer
    // copes with reordered frames regardless.
    set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "B-frames");
    set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency");
}

impl H264Encoder {
    pub fn new(device: &ID3D11Device, adapter: LUID, s: &EncoderSettings) -> Result<Self> {
        let candidates = find_encoders(adapter)?;
        if candidates.is_empty() {
            return Err(windows::core::Error::new(
                MF_E_TOPO_CODEC_NOT_FOUND,
                "no hardware H.264 encoder found on this GPU",
            ));
        }
        let mut last_err = None;
        for activate in candidates {
            let name = friendly_name(&activate);
            match Self::open(&activate, device, s, name.clone()) {
                Ok(enc) => {
                    log!("encoder: using {name}");
                    return Ok(enc);
                }
                Err(e) => {
                    log!("encoder: {name} failed: {e}");
                    let _ = unsafe { activate.ShutdownObject() };
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap())
    }

    fn open(activate: &IMFActivate, device: &ID3D11Device, s: &EncoderSettings, name: String) -> Result<Self> {
        unsafe {
            let mft: IMFTransform = activate.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            // Hardware MFTs are async and refuse to work until unlocked.
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let (mut input_id, mut output_id) = ([0u32], [0u32]);
            if mft.GetStreamIDs(&mut input_id, &mut output_id).is_err() {
                // E_NOTIMPL means fixed stream ids starting at 0.
                input_id = [0];
                output_id = [0];
            }
            let (input_id, output_id) = (input_id[0], output_id[0]);

            let codec: ICodecAPI = mft.cast()?;
            set_codec_api(&codec, s, false);

            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(s.width, s.height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(s.fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            out_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            mft.SetOutputType(output_id, &out_type, 0)?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(s.width, s.height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(s.fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(input_id, &in_type, 0)?;

            // Some drivers only honour these once the types are set.
            set_codec_api(&codec, s, true);

            let info = mft.GetOutputStreamInfo(output_id)?;
            let provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                != 0;

            let events: IMFMediaEventGenerator = mft.cast()?;
            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                mft,
                events,
                _device_manager: manager,
                input_id,
                output_id,
                need_input: 0,
                provides_samples,
                output_size: info.cbSize,
                name,
            })
        }
    }

    /// Process pending encoder events without blocking. Finished packets go to
    /// `on_packet`; a new sequence header (SPS/PPS) goes to `on_seq_header`.
    pub fn poll(&mut self, on_packet: &mut impl FnMut(Packet), on_seq_header: &mut impl FnMut(Vec<u8>)) -> Result<()> {
        loop {
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            };
            let kind = unsafe { event.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.pull_output(on_packet, on_seq_header)?;
            }
        }
    }

    pub fn wants_input(&self) -> bool {
        self.need_input > 0
    }

    pub fn submit(&mut self, texture: &ID3D11Texture2D, pts: i64, duration: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            if let Ok(b2d) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2d.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(duration)?;
            self.mft.ProcessInput(self.input_id, &sample, 0)?;
        }
        self.need_input -= 1;
        Ok(())
    }

    fn pull_output(
        &mut self,
        on_packet: &mut impl FnMut(Packet),
        on_seq_header: &mut impl FnMut(Vec<u8>),
    ) -> Result<()> {
        unsafe {
            let own_sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.output_size.max(1 << 20))?)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.mft.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            drop(ManuallyDrop::take(&mut out[0].pEvents));

            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder wants a renegotiated output type, which also
                    // carries the final SPS/PPS.
                    let t = self.mft.GetOutputAvailableType(self.output_id, 0)?;
                    self.mft.SetOutputType(self.output_id, &t, 0)?;
                    if let Some(seq) = sequence_header(&self.mft.GetOutputCurrentType(self.output_id)?) {
                        on_seq_header(seq);
                    }
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e),
            }
            let Some(sample) = sample else { return Ok(()) };
            let pts = sample.GetSampleTime().unwrap_or(0);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = core::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = core::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            on_packet(Packet { pts, key, data });
        }
        Ok(())
    }

    pub fn sequence_header(&self) -> Option<Vec<u8>> {
        unsafe { self.mft.GetOutputCurrentType(self.output_id).ok().and_then(|t| sequence_header(&t)) }
    }
}

fn sequence_header(t: &IMFMediaType) -> Option<Vec<u8>> {
    unsafe {
        let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut buf = vec![0u8; size as usize];
        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).ok()?;
        Some(buf)
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.mft.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}
