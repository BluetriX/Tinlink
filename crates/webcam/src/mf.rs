//! Media Foundation camera capture (Windows).
//!
//! A synchronous source reader delivers one sample per `ReadSample` call.
//! With advanced video processing enabled it decodes MJPEG and converts any
//! camera format to NV12, so callers only ever see one pixel format.

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, PWSTR};

use crate::{CameraInfo, Error, Frame, Request, best_mode};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Platform(e.to_string())
    }
}

/// COM and Media Foundation for the current thread, for as long as it lives.
struct Runtime {
    com: bool,
    mf: bool,
}

impl Runtime {
    fn start() -> Result<Self, Error> {
        // SAFETY: initialises COM and Media Foundation for this thread; both
        // are balanced in Drop.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        // SAFETY: as above.
        let started = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) };
        let runtime = Self {
            com,
            mf: started.is_ok(),
        };
        started?;
        Ok(runtime)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if self.mf {
            // SAFETY: balances the successful MFStartup; the owner drops its
            // MF objects before this (field order in `Reader`).
            let _ = unsafe { MFShutdown() };
        }
        if self.com {
            // SAFETY: balances the successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn list() -> Result<Vec<CameraInfo>, Error> {
    let _runtime = Runtime::start()?;
    Ok(devices()?
        .iter()
        .filter_map(|activate| {
            Some(CameraInfo {
                id: string(
                    activate,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                )?,
                name: string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                    .unwrap_or_else(|| "Camera".into()),
            })
        })
        .collect())
}

/// All video capture devices.
fn devices() -> Result<Vec<IMFActivate>, Error> {
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFEnumDeviceSources allocates `count` activation objects in a
    // CoTaskMemAlloc'ed array. Each element is moved out (so it is released
    // exactly once) before the array itself is freed.
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.ok_or(Error::NoCamera)?;
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        MFEnumDeviceSources(&attributes, &mut list, &mut count)?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        let items = std::slice::from_raw_parts_mut(list, count as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(list as *const _));
        Ok(items)
    }
}

fn string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: on success the string is CoTaskMemAlloc'ed, copied and freed.
    unsafe {
        activate
            .GetAllocatedString(key, &mut value, &mut len)
            .ok()?;
        let s = value.to_string().ok();
        CoTaskMemFree(Some(value.0 as *const _));
        s
    }
}

/// Two u32 packed into one attribute (frame size, frame rate).
fn split(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

fn mode_of(media_type: &IMFMediaType) -> Option<Request> {
    // SAFETY: attribute reads on a valid media type.
    let (size, rate) = unsafe {
        (
            media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?,
            media_type.GetUINT64(&MF_MT_FRAME_RATE).ok()?,
        )
    };
    let (width, height) = split(size);
    let (num, den) = split(rate);
    Some(Request {
        width,
        height,
        fps: num.checked_div(den).unwrap_or(0),
    })
}

pub(crate) struct Reader {
    pub name: String,
    reader: IMFSourceReader,
    source: IMFMediaSource,
    width: u32,
    height: u32,
    stride: u32,
    // Last: shut down after the MF objects above are released.
    _runtime: Runtime,
}

impl Reader {
    pub fn open(id: Option<&str>, request: Request) -> Result<Self, Error> {
        let runtime = Runtime::start()?;
        let devices = devices()?;
        let activate = match id {
            Some(id) => devices.iter().find(|a| {
                string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).as_deref()
                    == Some(id)
            }),
            None => None,
        }
        .or(devices.first())
        .ok_or(Error::NoCamera)?;
        let name = string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
            .unwrap_or_else(|| "Camera".into());

        // SAFETY: plain COM calls on valid interfaces owned by this function;
        // out parameters point to locals.
        unsafe {
            let source: IMFMediaSource = activate.ActivateObject()?;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 2)?;
            let attributes = attributes.ok_or(Error::NoCamera)?;
            // Decoding (MJPEG) and conversion to NV12 inside the reader.
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            let reader = MFCreateSourceReaderFromMediaSource(&source, &attributes)?;

            // Pick the camera's own mode closest to the request ...
            let mut natives = Vec::new();
            for index in 0.. {
                match reader.GetNativeMediaType(STREAM, index) {
                    Ok(media_type) => {
                        if let Some(mode) = mode_of(&media_type) {
                            natives.push((mode, media_type));
                        }
                    }
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                    Err(e) => return Err(e.into()),
                }
            }
            let modes: Vec<Request> = natives.iter().map(|(m, _)| *m).collect();
            let chosen = best_mode(&modes, request);
            if let Some(chosen) = chosen
                && let Some((_, native)) = natives.iter().find(|(m, _)| *m == chosen)
            {
                // Not fatal: the reader falls back to the default mode.
                if let Err(e) = reader.SetCurrentMediaType(STREAM, None, native) {
                    tracing::debug!("cannot select camera mode {chosen:?}: {e}");
                }
            }

            // ... and let the reader convert it to NV12 of the same size.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            if let Some(mode) = chosen {
                output.SetUINT64(&MF_MT_FRAME_SIZE, pack(mode.width & !1, mode.height & !1))?;
                if mode.fps > 0 {
                    output.SetUINT64(&MF_MT_FRAME_RATE, pack(mode.fps, 1))?;
                }
            }
            reader.SetCurrentMediaType(STREAM, None, &output)?;
            reader.SetStreamSelection(STREAM, true)?;

            let mut this = Self {
                name,
                reader,
                source,
                width: 0,
                height: 0,
                stride: 0,
                _runtime: runtime,
            };
            this.read_format()?;
            tracing::info!(camera = %this.name, ?chosen, width = this.width, height = this.height, "camera opened");
            Ok(this)
        }
    }

    fn read_format(&mut self) -> Result<(), Error> {
        // SAFETY: attribute reads on the reader's current media type.
        unsafe {
            let current = self.reader.GetCurrentMediaType(STREAM)?;
            let (width, height) = split(current.GetUINT64(&MF_MT_FRAME_SIZE)?);
            // Top-down NV12 has a positive stride; default to the width.
            let stride = current
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .ok()
                .map(|s| s as i32)
                .filter(|s| *s > 0)
                .map_or(width, |s| s as u32);
            (self.width, self.height, self.stride) = (width, height, stride.max(width));
        }
        Ok(())
    }

    pub fn read(&mut self) -> Result<Option<Frame>, Error> {
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample = None;
        // SAFETY: synchronous ReadSample with out parameters pointing to locals.
        unsafe {
            self.reader.ReadSample(
                STREAM,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
        }
        let flag = |f: MF_SOURCE_READER_FLAG| flags & f.0 as u32 != 0;
        if flag(MF_SOURCE_READERF_ERROR) {
            return Err(Error::Platform("camera stream failed".into()));
        }
        if flag(MF_SOURCE_READERF_ENDOFSTREAM) {
            return Err(Error::Platform("camera disconnected".into()));
        }
        if flag(MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED) {
            self.read_format()?;
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        let (width, height, stride) = (self.width, self.height, self.stride);
        let y_len = stride as usize * height as usize;
        let uv_len = stride as usize * (height as usize / 2);

        // SAFETY: the buffer is locked while its `len` bytes are read and
        // unlocked right after; all reads stay within `len`.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize);
            // The Y plane may be padded to an aligned height; the UV plane
            // then starts after the padding (two thirds of the buffer).
            let rows = bytes.len() / stride.max(1) as usize;
            let uv_offset = (rows * 2 / 3).max(height as usize) * stride as usize;
            let frame = if bytes.len() >= uv_offset + uv_len {
                let mut data = Vec::with_capacity(y_len + uv_len);
                data.extend_from_slice(&bytes[..y_len]);
                data.extend_from_slice(&bytes[uv_offset..uv_offset + uv_len]);
                Some(Frame {
                    data,
                    width,
                    height,
                    stride,
                    // Media Foundation counts in 100 ns units.
                    timestamp_us: timestamp / 10,
                })
            } else {
                tracing::debug!(len, width, height, stride, "camera sample too small");
                None
            };
            buffer.Unlock()?;
            Ok(frame)
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: releases the camera (its light goes off) before COM shuts down.
        unsafe {
            let _ = self.source.Shutdown();
        }
    }
}
