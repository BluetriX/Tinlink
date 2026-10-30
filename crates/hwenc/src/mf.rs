//! Media Foundation H.264 encoder (Windows).
//!
//! Hardware encoder MFTs are asynchronous: they announce through events when
//! they accept input (`METransformNeedInput`) and when output is ready
//! (`METransformHaveOutput`). The encoder thread polls those events without
//! blocking and waits on the input channel in between, in 1 ms steps, so an
//! encoded frame is forwarded within about a millisecond of being ready.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface, PWSTR};

use crate::{Config, Control, EncodedFrame, Error, InputFrame, nal};

const POLL: Duration = Duration::from_millis(1);

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Platform(e.to_string())
    }
}

/// COM and Media Foundation for the current thread, for as long as it lives.
/// All COM objects of the thread must be released before it is dropped.
pub(crate) struct Session {
    com: bool,
}

impl Session {
    pub(crate) fn start() -> Result<Self, Error> {
        // SAFETY: initialises COM for this thread only; balanced in Drop.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        // SAFETY: as above, for Media Foundation.
        match unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            Ok(()) => Ok(Self { com }),
            Err(e) => {
                if com {
                    // SAFETY: balances the successful CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
                Err(e.into())
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: balances the successful MFStartup in `start`.
        let _ = unsafe { MFShutdown() };
        if self.com {
            // SAFETY: balances the successful CoInitializeEx in `start`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Encoder thread entry point.
pub(crate) fn run(
    config: Config,
    input: Receiver<InputFrame>,
    control: Arc<Control>,
    ready: Sender<Result<String, Error>>,
    mut on_frame: impl FnMut(EncodedFrame),
) {
    let session = match Session::start() {
        Ok(session) => session,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    match Encoder::new(config, None) {
        Ok(mut encoder) => {
            tracing::info!(encoder = %encoder.name, ?config, "hardware encoder started");
            let _ = ready.send(Ok(encoder.name.clone()));
            if let Err(e) = encoder.run(&input, &control, &mut on_frame) {
                tracing::error!("hardware encoder failed: {e}");
            }
            encoder.shutdown();
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
    drop(session);
}

pub(crate) struct Encoder {
    pub(crate) name: String,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    input_id: u32,
    output_id: u32,
    provides_samples: bool,
    output_size: u32,
    /// SPS and PPS, prepended to keyframes that come without them.
    parameter_sets: Vec<u8>,
    config: Config,
    /// Inputs the encoder has asked for and not yet received.
    pub(crate) credits: u32,
}

impl Encoder {
    /// With a DXGI device manager the encoder takes D3D11 textures
    /// ([`Encoder::process_texture`]), else NV12 in system memory.
    pub(crate) fn new(config: Config, d3d: Option<&IMFDXGIDeviceManager>) -> Result<Self, Error> {
        let (activate, name) = find_hardware_encoder()?;
        // SAFETY: all calls below are plain COM calls on valid interface
        // pointers owned by this function; out parameters point to locals.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject()?;
            let attributes = transform.GetAttributes()?;
            // Hardware MFTs are asynchronous and must be unlocked explicitly.
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            if let Some(manager) = d3d {
                // Must come before the media types are set.
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            }

            let (mut input_ids, mut output_ids) = ([0u32], [0u32]);
            // E_NOTIMPL means the stream ids are simply 0.
            let _ = transform.GetStreamIDs(&mut input_ids, &mut output_ids);
            let (input_id, output_id) = (input_ids[0], output_ids[0]);

            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                configure_codec(codec, &config);
            }

            set_output_type(&transform, output_id, &config)?;
            set_input_type(&transform, input_id, &config)?;

            let info = transform.GetOutputStreamInfo(output_id)?;
            let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let output_size = info.cbSize.max(config.width * config.height * 3 / 2);
            let parameter_sets = sequence_header(&transform, output_id).unwrap_or_default();

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = transform.cast::<IMFMediaEventGenerator>()?;

            Ok(Self {
                name,
                transform,
                events,
                codec,
                input_id,
                output_id,
                provides_samples,
                output_size,
                parameter_sets,
                config,
                credits: 0,
            })
        }
    }

    fn run(
        &mut self,
        input: &Receiver<InputFrame>,
        control: &Control,
        on_frame: &mut impl FnMut(EncodedFrame),
    ) -> Result<(), Error> {
        loop {
            self.drain_events(on_frame)?;
            if self.credits == 0 {
                // The encoder is busy; check its events again shortly.
                std::thread::sleep(POLL);
                continue;
            }
            match input.recv_timeout(POLL) {
                Ok(frame) => {
                    self.apply_control(control);
                    self.process_input(frame)?;
                    self.credits -= 1;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    pub(crate) fn drain_events(
        &mut self,
        on_frame: &mut impl FnMut(EncodedFrame),
    ) -> Result<(), Error> {
        loop {
            // SAFETY: COM call on a valid interface.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            // SAFETY: COM call on a valid interface.
            let kind = unsafe { event.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.credits += 1;
            } else if kind == METransformHaveOutput.0 as u32
                && let Some(frame) = self.process_output()?
            {
                on_frame(frame);
            }
        }
    }

    pub(crate) fn apply_control(&self, control: &Control) {
        let Some(codec) = &self.codec else { return };
        if control.keyframe.swap(false, Ordering::Relaxed) {
            set_value(codec, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1));
        }
        let bitrate = control.bitrate_bps.swap(0, Ordering::Relaxed);
        if bitrate != 0 {
            set_value(
                codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(bitrate),
            );
        }
    }

    fn process_input(&self, frame: InputFrame) -> Result<(), Error> {
        let len = u32::try_from(frame.nv12.len())
            .map_err(|_| Error::Platform("frame too large".into()))?;
        // SAFETY: the buffer is locked for exactly `len` bytes before the copy
        // (MFCreateMemoryBuffer allocates at least that much) and unlocked after.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(frame.nv12.as_ptr(), ptr, frame.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;

            self.process_buffer(&buffer, frame.timestamp_us)
        }
    }

    /// Encodes an NV12 texture of the device given to [`Encoder::new`].
    pub(crate) fn process_texture(
        &self,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        timestamp_us: i64,
    ) -> Result<(), Error> {
        // SAFETY: COM calls on valid interfaces.
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(
                &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::IID,
                texture,
                0,
                false,
            )?;
            let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(length)?;
            self.process_buffer(&buffer, timestamp_us)
        }
    }

    fn process_buffer(&self, buffer: &IMFMediaBuffer, timestamp_us: i64) -> Result<(), Error> {
        // SAFETY: COM calls on valid interfaces.
        unsafe {
            let sample = MFCreateSample()?;
            sample.AddBuffer(buffer)?;
            // Media Foundation counts in 100 ns units.
            sample.SetSampleTime(timestamp_us * 10)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.config.fps.max(1)))?;
            self.transform.ProcessInput(self.input_id, &sample, 0)?;
        }
        Ok(())
    }

    fn process_output(&mut self) -> Result<Option<EncodedFrame>, Error> {
        // SAFETY: COM calls on valid interfaces. `MFT_OUTPUT_DATA_BUFFER`
        // holds its COM pointers in ManuallyDrop; both are taken back out
        // after the call so they are released exactly once.
        unsafe {
            let provided = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                Some(sample)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));

            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder picked its final output format; accept it.
                    let media_type = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform
                        .SetOutputType(self.output_id, &media_type, 0)?;
                    if let Some(header) = sequence_header(&self.transform, self.output_id) {
                        self.parameter_sets = header;
                    }
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else {
                return Ok(None);
            };

            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;

            let timestamp_us = sample.GetSampleTime().unwrap_or(0) / 10;
            let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let data = nal::with_parameter_sets(data, &self.parameter_sets);
            let keyframe = clean_point || nal::is_keyframe(&data);
            Ok(Some(EncodedFrame {
                data,
                keyframe,
                timestamp_us,
            }))
        }
    }

    pub(crate) fn shutdown(&self) {
        // SAFETY: COM calls on valid interfaces; errors are irrelevant here.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            // Asynchronous MFTs keep worker threads until shut down.
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

/// Returns the preferred hardware encoder that turns NV12 into H.264.
fn find_hardware_encoder() -> Result<(IMFActivate, String), Error> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates `count` activation objects in a
    // CoTaskMemAlloc'ed array. Each element is moved out (so it is released
    // exactly once) before the array itself is freed.
    let activates: Vec<IMFActivate> = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )?;
        if list.is_null() {
            Vec::new()
        } else {
            let items = std::slice::from_raw_parts_mut(list, count as usize)
                .iter_mut()
                .filter_map(Option::take)
                .collect();
            CoTaskMemFree(Some(list as *const _));
            items
        }
    };
    let activate = activates.into_iter().next().ok_or(Error::NoEncoder)?;
    let name = friendly_name(&activate).unwrap_or_else(|| "hardware H.264 encoder".into());
    Ok((activate, name))
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut value = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: on success the string is CoTaskMemAlloc'ed, copied and freed.
    unsafe {
        activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
            .ok()?;
        let name = value.to_string().ok();
        CoTaskMemFree(Some(value.0 as *const _));
        name
    }
}

fn configure_codec(codec: &ICodecAPI, config: &Config) {
    set_value(codec, &CODECAPI_AVLowLatencyMode, variant_bool(true));
    set_value(
        codec,
        &CODECAPI_AVEncCommonRateControlMode,
        variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
    );
    set_value(
        codec,
        &CODECAPI_AVEncCommonMeanBitRate,
        variant_u32(config.bitrate_bps),
    );
    // Keyframes come on request (PLI); a periodic one bounds recovery time.
    set_value(
        codec,
        &CODECAPI_AVEncMPVGOPSize,
        variant_u32(config.fps * 10),
    );
    set_value(
        codec,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        variant_u32(0),
    );
}

fn variant(vt: windows::Win32::System::Variant::VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

fn variant_bool(value: bool) -> VARIANT {
    let value = if value { VARIANT_TRUE } else { VARIANT_FALSE };
    variant(VT_BOOL, VARIANT_0_0_0 { boolVal: value })
}

/// Encoders may not support every property; failures are only logged.
fn set_value(codec: &ICodecAPI, api: &GUID, value: VARIANT) {
    // SAFETY: plain-data VARIANT (no owned pointers) that outlives the call.
    let result = unsafe { codec.SetValue(api, &value) };
    if let Err(e) = result {
        tracing::debug!(?api, "codec property not set: {e}");
    }
}

fn set_output_type(transform: &IMFTransform, id: u32, config: &Config) -> Result<(), Error> {
    // WebRTC negotiates Constrained Baseline (no B-frames, no reordering
    // delay); fall back to Baseline for encoders that do not list it.
    let mut last = None;
    for profile in [eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_Base] {
        // SAFETY: COM calls on valid interfaces.
        let result = unsafe {
            let media_type = video_type(&MFVideoFormat_H264, config)?;
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32)?;
            transform.SetOutputType(id, &media_type, 0)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map(Error::from).unwrap_or(Error::NoEncoder))
}

fn set_input_type(transform: &IMFTransform, id: u32, config: &Config) -> Result<(), Error> {
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let media_type = video_type(&MFVideoFormat_NV12, config)?;
        transform.SetInputType(id, &media_type, 0)?;
    }
    Ok(())
}

/// # Safety
/// Only creates and fills a new media type.
unsafe fn video_type(subtype: &GUID, config: &Config) -> windows::core::Result<IMFMediaType> {
    // SAFETY: COM calls on a freshly created media type.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(config.fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(media_type)
    }
}

/// Media Foundation packs two u32 values (e.g. width and height) into one u64.
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// SPS/PPS from the current output type, if the encoder publishes them.
fn sequence_header(transform: &IMFTransform, id: u32) -> Option<Vec<u8>> {
    // SAFETY: COM calls on valid interfaces; the blob is copied into a buffer
    // of the size the API reported.
    unsafe {
        let media_type = transform.GetOutputCurrentType(id).ok()?;
        let size = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; size as usize];
        media_type
            .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
            .ok()?;
        Some(blob)
    }
}
