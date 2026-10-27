//! Webcam capture for video calls.
//!
//! On Windows the camera is opened through a Media Foundation source reader,
//! which also decodes MJPEG and converts every camera format to NV12. Other
//! platforms report [`Error::Unsupported`] for now.

#[cfg(windows)]
mod mf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("webcams are not supported on this platform yet")]
    Unsupported,
    #[error("no camera found")]
    NoCamera,
    #[error("{0}")]
    Platform(String),
}

/// A camera as shown in the device selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Stable id (the device's symbolic link on Windows).
    pub id: String,
    pub name: String,
}

/// The capture mode asked for; the camera's closest mode is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// One captured frame in NV12: `height` rows of Y with `stride` bytes each,
/// then `height / 2` rows of interleaved UV with the same stride.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_us: i64,
}

impl Frame {
    pub fn y(&self) -> &[u8] {
        &self.data[..self.uv_offset()]
    }

    pub fn uv(&self) -> &[u8] {
        &self.data[self.uv_offset()..]
    }

    fn uv_offset(&self) -> usize {
        self.stride as usize * self.height as usize
    }
}

/// Lists the connected cameras.
pub fn list() -> Result<Vec<CameraInfo>, Error> {
    #[cfg(windows)]
    {
        mf::list()
    }
    #[cfg(not(windows))]
    {
        Err(Error::Unsupported)
    }
}

/// An open camera. Not `Send`: use it on the thread that opened it.
pub struct Camera {
    #[cfg(windows)]
    inner: mf::Reader,
}

impl Camera {
    /// Opens the camera with `id`, or the first one.
    pub fn open(id: Option<&str>, request: Request) -> Result<Self, Error> {
        #[cfg(windows)]
        {
            Ok(Self {
                inner: mf::Reader::open(id, request)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (id, request);
            Err(Error::Unsupported)
        }
    }

    /// The name of the opened camera.
    pub fn name(&self) -> &str {
        #[cfg(windows)]
        {
            &self.inner.name
        }
        #[cfg(not(windows))]
        {
            ""
        }
    }

    /// Blocks until the next frame; `None` for a gap in the stream.
    pub fn read(&mut self) -> Result<Option<Frame>, Error> {
        #[cfg(windows)]
        {
            self.inner.read()
        }
        #[cfg(not(windows))]
        {
            Err(Error::Unsupported)
        }
    }
}

/// Picks the mode closest to the request: the smallest one that is at least
/// as tall (else the tallest), preferring modes that reach the frame rate.
pub fn best_mode(modes: &[Request], want: Request) -> Option<Request> {
    let score = |m: &Request| {
        let too_small = m.height < want.height;
        let too_slow = m.fps < want.fps.min(30);
        // Lower is better.
        (
            too_slow,
            too_small,
            if too_small {
                want.height - m.height
            } else {
                m.height - want.height
            },
            u32::MAX - m.fps,
        )
    };
    modes.iter().copied().min_by_key(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, fps: u32) -> Request {
        Request { width, height, fps }
    }

    #[test]
    fn picks_closest_mode() {
        let modes = [
            mode(640, 480, 30),
            mode(1280, 720, 30),
            mode(1920, 1080, 30),
            mode(1920, 1080, 5),
            mode(1280, 720, 10),
        ];
        assert_eq!(
            best_mode(&modes, mode(1280, 720, 30)),
            Some(mode(1280, 720, 30))
        );
        assert_eq!(
            best_mode(&modes, mode(960, 540, 30)),
            Some(mode(1280, 720, 30))
        );
        assert_eq!(
            best_mode(&modes, mode(640, 360, 30)),
            Some(mode(640, 480, 30))
        );
        // Rather a slow mode than none; too slow loses against too small.
        assert_eq!(
            best_mode(
                &[mode(1920, 1080, 5), mode(640, 480, 30)],
                mode(1920, 1080, 30)
            ),
            Some(mode(640, 480, 30))
        );
        assert_eq!(best_mode(&[], mode(1280, 720, 30)), None);
    }

    #[test]
    fn frame_planes() {
        let frame = Frame {
            data: vec![0; 64 * 4 * 3 / 2],
            width: 60,
            height: 4,
            stride: 64,
            timestamp_us: 0,
        };
        assert_eq!(frame.y().len(), 256);
        assert_eq!(frame.uv().len(), 128);
    }
}
