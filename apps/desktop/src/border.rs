//! Coloured frame around the monitor being shared, so the sharer always
//! knows what others see.
//!
//! The frame is a transparent, undecorated, click-through window on top of
//! the captured area. It is content protected (`WDA_EXCLUDEFROMCAPTURE` on
//! Windows), so it does not appear in the shared stream.

use anyhow::Result;
use tauri::{
    AppHandle, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

/// Closes the frame when dropped.
pub struct ShareBorder {
    window: WebviewWindow,
}

impl ShareBorder {
    /// `area` is the captured region in physical pixels, relative to the
    /// top-left corner of the virtual desktop (as libwebrtc reports it).
    pub fn show(app: &AppHandle, (x, y, width, height): (i32, i32, u32, u32)) -> Result<Self> {
        let monitors: Vec<_> = app
            .available_monitors()?
            .iter()
            .map(|m| (m.position().x, m.position().y))
            .collect();
        let (x, y) = to_desktop((x, y), &monitors);
        let builder =
            WebviewWindowBuilder::new(app, "share-border", WebviewUrl::App("border.html".into()));
        let window = crate::webview_environment(app, builder)?
            .title("Tinlink sharing")
            .transparent(true)
            .decorations(false)
            .shadow(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .content_protected(true)
            .visible(false)
            .build()?;
        window.set_ignore_cursor_events(true)?;
        window.show()?;
        // Placed after showing: while hidden the WebView keeps its initial
        // size and the frame's right and bottom edges end up off-window.
        window.set_position(PhysicalPosition::new(x, y))?;
        window.set_size(PhysicalSize::new(width, height))?;
        Ok(Self { window })
    }
}

impl Drop for ShareBorder {
    fn drop(&mut self) {
        let _ = self.window.close();
    }
}

/// Converts virtual-desktop-relative coordinates into desktop coordinates:
/// monitors left of or above the primary have negative positions.
fn to_desktop((x, y): (i32, i32), monitor_positions: &[(i32, i32)]) -> (i32, i32) {
    let origin_x = monitor_positions
        .iter()
        .map(|p| p.0)
        .min()
        .unwrap_or(0)
        .min(0);
    let origin_y = monitor_positions
        .iter()
        .map(|p| p.1)
        .min()
        .unwrap_or(0)
        .min(0);
    (x + origin_x, y + origin_y)
}

#[cfg(test)]
mod tests {
    use super::to_desktop;

    #[test]
    fn converts_virtual_desktop_coordinates() {
        // Secondary monitor left of the primary (Erik's setup).
        let monitors = [(-2560, 0), (0, 0)];
        assert_eq!(to_desktop((2560, 0), &monitors), (0, 0));
        assert_eq!(to_desktop((0, 0), &monitors), (-2560, 0));
        // Single monitor or secondary on the right: nothing to shift.
        assert_eq!(to_desktop((1920, 0), &[(0, 0), (1920, 0)]), (1920, 0));
        // Monitor above the primary.
        assert_eq!(to_desktop((0, 1080), &[(0, -1080), (0, 0)]), (0, 0));
    }
}
