//! Live DWM thumbnails of task windows, drawn by DWM on top of the overlay
//! window (plan §2.9). They cost nothing on our side once registered.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION,
    DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmQueryThumbnailSourceSize,
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};

use crate::layout::Rect;

/// Registered thumbnails, by source window.
#[derive(Debug, Default)]
pub struct Thumbs {
    by_source: HashMap<isize, isize>,
}

impl Thumbs {
    /// Show exactly these `(source hwnd, slot in DIPs)` thumbnails, scaled by
    /// `scale` to pixels, at `opacity` (0–1). Others are unregistered.
    pub fn sync(&mut self, dest: HWND, slots: &[(isize, Rect)], scale: f32, opacity: f32) {
        self.by_source.retain(|src, id| {
            let keep = slots.iter().any(|(s, _)| s == src);
            if !keep {
                // SAFETY: `id` came from DwmRegisterThumbnail and is
                // unregistered once, here.
                let _ = unsafe { DwmUnregisterThumbnail(*id) };
            }
            keep
        });
        for &(src, slot) in slots {
            let id = match self.by_source.get(&src) {
                Some(&id) => id,
                None => {
                    // SAFETY: both handles are windows; a stale source fails.
                    match unsafe { DwmRegisterThumbnail(dest, HWND(src as _)) } {
                        Ok(id) => {
                            self.by_source.insert(src, id);
                            id
                        }
                        Err(e) => {
                            tracing::debug!(src, "DwmRegisterThumbnail failed: {e}");
                            continue;
                        }
                    }
                }
            };
            // SAFETY: `id` is a live thumbnail handle.
            let size = unsafe { DwmQueryThumbnailSourceSize(id) }.unwrap_or_default();
            let r = slot.fit(size.cx as f32, size.cy as f32).scaled(scale);
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY
                    | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT {
                    left: r.x.round() as i32,
                    top: r.y.round() as i32,
                    right: r.right().round() as i32,
                    bottom: r.bottom().round() as i32,
                },
                opacity: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
                fVisible: true.into(),
                fSourceClientAreaOnly: true.into(),
                ..Default::default()
            };
            // SAFETY: `id` is live and `props` outlives the call.
            if let Err(e) = unsafe { DwmUpdateThumbnailProperties(id, &props) } {
                tracing::debug!(src, "DwmUpdateThumbnailProperties failed: {e}");
            }
        }
    }

    pub fn clear(&mut self) {
        for (_, id) in self.by_source.drain() {
            // SAFETY: each id came from DwmRegisterThumbnail; drained so it
            // is unregistered once.
            let _ = unsafe { DwmUnregisterThumbnail(id) };
        }
    }
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        self.clear();
    }
}
