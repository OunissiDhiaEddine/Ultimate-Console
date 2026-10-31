//! Direct2D + DirectWrite drawing onto a DirectComposition swapchain (plan
//! §2.9). Everything here is created when the overlay opens and dropped when
//! it closes.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_ROUNDED_RECT, D2D1CreateFactory, ID2D1DeviceContext,
    ID2D1Factory1, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionEffectGroup, IDCompositionTarget,
    IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
};
use windows::core::{Interface, PCWSTR, Result, w};

use crate::layout::{Layout, ROW_H, Rect};
use crate::model::{Confirm, Model, Tab, VolumeRow};
use crate::osd;

#[derive(Clone, Copy)]
struct Rgba(f32, f32, f32, f32);

const fn hex(rgb: u32, a: f32) -> Rgba {
    Rgba(
        ((rgb >> 16) & 0xFF) as f32 / 255.0,
        ((rgb >> 8) & 0xFF) as f32 / 255.0,
        (rgb & 0xFF) as f32 / 255.0,
        a,
    )
}

const BACKDROP: Rgba = hex(0x000000, 0.55);
const CARD: Rgba = hex(0x151922, 0.97);
const CARD_EDGE: Rgba = hex(0xFFFFFF, 0.08);
const TILE: Rgba = hex(0x1E2330, 1.0);
const TILE_SELECTED: Rgba = hex(0x283044, 1.0);
const THUMB_BG: Rgba = hex(0x0D1016, 1.0);
const ACCENT: Rgba = hex(0x4C9AFF, 1.0);
const TEXT: Rgba = hex(0xF2F4F8, 1.0);
const DIM: Rgba = hex(0x9AA3B2, 1.0);
const TRACK: Rgba = hex(0x323A4A, 1.0);
const MUTED: Rgba = hex(0x6B7385, 1.0);
const DANGER: Rgba = hex(0xFF6B6B, 1.0);

struct Fonts {
    big: IDWriteTextFormat,
    tab: IDWriteTextFormat,
    tab_right: IDWriteTextFormat,
    body: IDWriteTextFormat,
    body_right: IDWriteTextFormat,
    body_center: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_right: IDWriteTextFormat,
    /// Segoe MDL2 Assets glyphs, for the volume popup's speaker icon.
    icon: IDWriteTextFormat,
}

impl Fonts {
    fn new() -> Result<Self> {
        // SAFETY: plain factory creation.
        let dw: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let make_in = |family: PCWSTR,
                       size: f32,
                       weight: DWRITE_FONT_WEIGHT,
                       align: DWRITE_TEXT_ALIGNMENT|
         -> Result<IDWriteTextFormat> {
            // SAFETY: the font name and locale are static wide strings; the
            // format and trimming sign are owned COM objects.
            unsafe {
                let f = dw.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )?;
                f.SetTextAlignment(align)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let sign = dw.CreateEllipsisTrimmingSign(&f)?;
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                f.SetTrimming(&trimming, &sign)?;
                Ok(f)
            }
        };
        let make = |size, weight, align| make_in(w!("Segoe UI"), size, weight, align);
        let (n, sb) = (DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD);
        let (l, r, c) = (
            DWRITE_TEXT_ALIGNMENT_LEADING,
            DWRITE_TEXT_ALIGNMENT_TRAILING,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        );
        Ok(Self {
            big: make(30.0, sb, l)?,
            tab: make(21.0, sb, l)?,
            tab_right: make(21.0, sb, r)?,
            body: make(17.0, n, l)?,
            body_right: make(17.0, n, r)?,
            body_center: make(17.0, n, c)?,
            small: make(14.0, n, l)?,
            small_right: make(14.0, n, r)?,
            icon: make_in(w!("Segoe MDL2 Assets"), 26.0, n, c)?,
        })
    }
}

fn d2d_rect(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.right(),
        bottom: r.bottom(),
    }
}

/// The GPU objects for one open overlay.
pub struct Gfx {
    _d3d: ID3D11Device,
    swapchain: IDXGISwapChain1,
    dc: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    effect: IDCompositionEffectGroup,
    fonts: Fonts,
}

fn create_d3d(driver: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut device = None;
    // SAFETY: `device` is a valid out pointer for the duration of the call.
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
}

impl Gfx {
    /// Build the device chain for a `width` × `height` pixel window at `dpi`.
    pub fn new(hwnd: HWND, width: u32, height: u32, dpi: f32) -> Result<Self> {
        let d3d = create_d3d(D3D_DRIVER_TYPE_HARDWARE).or_else(|e| {
            tracing::warn!("hardware D3D11 device failed ({e}), using WARP");
            create_d3d(D3D_DRIVER_TYPE_WARP)
        })?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        // SAFETY: every call below passes live COM objects and pointers to
        // locals that outlive the call.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swapchain = factory.CreateSwapChainForComposition(&dxgi, &desc, None)?;

            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dc = d2d
                .CreateDevice(&dxgi)?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let surface: IDXGISurface = swapchain.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                ..Default::default()
            };
            let bitmap = dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            dc.SetTarget(&bitmap);
            dc.SetDpi(dpi, dpi);
            let brush = dc.CreateSolidColorBrush(&color(TEXT), None)?;

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            // Not topmost: DWM thumbnails registered on the window draw above
            // the swapchain content.
            let target = dcomp.CreateTargetForHwnd(hwnd, false)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            let effect = dcomp.CreateEffectGroup()?;
            effect.SetOpacity2(0.0)?;
            visual.SetEffect(&effect)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;

            Ok(Self {
                _d3d: d3d,
                swapchain,
                dc,
                brush,
                dcomp,
                _target: target,
                _visual: visual,
                effect,
                fonts: Fonts::new()?,
            })
        }
    }

    /// Whole-window opacity, used for the fade. No redraw needed.
    pub fn set_opacity(&self, opacity: f32) -> Result<()> {
        // SAFETY: live COM objects.
        unsafe {
            self.effect.SetOpacity2(opacity.clamp(0.0, 1.0))?;
            self.dcomp.Commit()
        }
    }

    fn fill(&self, r: Rect, radius: f32, c: Rgba) {
        // SAFETY: called between BeginDraw and EndDraw on live objects.
        unsafe {
            self.brush.SetColor(&color(c));
            if radius > 0.0 {
                let rr = D2D1_ROUNDED_RECT {
                    rect: d2d_rect(r),
                    radiusX: radius,
                    radiusY: radius,
                };
                self.dc.FillRoundedRectangle(&rr, &self.brush);
            } else {
                self.dc.FillRectangle(&d2d_rect(r), &self.brush);
            }
        }
    }

    fn stroke(&self, r: Rect, radius: f32, c: Rgba, width: f32) {
        // SAFETY: called between BeginDraw and EndDraw on live objects.
        unsafe {
            self.brush.SetColor(&color(c));
            let rr = D2D1_ROUNDED_RECT {
                rect: d2d_rect(r),
                radiusX: radius,
                radiusY: radius,
            };
            self.dc.DrawRoundedRectangle(&rr, &self.brush, width, None);
        }
    }

    fn text(&self, s: &str, font: &IDWriteTextFormat, r: Rect, c: Rgba) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        // SAFETY: called between BeginDraw and EndDraw; `wide` outlives the call.
        unsafe {
            self.brush.SetColor(&color(c));
            self.dc.DrawText(
                &wide,
                font,
                &d2d_rect(r),
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Draw one frame and present it.
    pub fn draw(&self, model: &Model, layout: &Layout) -> Result<()> {
        // SAFETY: live COM objects; BeginDraw/EndDraw bracket every draw call.
        unsafe {
            self.dc.BeginDraw();
            self.dc.Clear(Some(&color(Rgba(0.0, 0.0, 0.0, 0.0))));
        }
        self.scene(model, layout);
        // SAFETY: as above.
        unsafe {
            self.dc.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()
        }
    }

    /// Draw the volume popup (its own small window) and present it.
    pub fn draw_osd(&self, view: &osd::View, parts: &osd::Parts) -> Result<()> {
        // SAFETY: live COM objects; BeginDraw/EndDraw bracket every draw call.
        unsafe {
            self.dc.BeginDraw();
            self.dc.Clear(Some(&color(Rgba(0.0, 0.0, 0.0, 0.0))));
        }
        self.osd_scene(view, parts);
        // SAFETY: as above.
        unsafe {
            self.dc.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()
        }
    }

    fn osd_scene(&self, view: &osd::View, p: &osd::Parts) {
        let f = &self.fonts;
        let radius = 14.0;
        self.fill(p.card, radius, CARD);
        self.stroke(p.card.inset(0.5), radius, CARD_EDGE, 1.0);
        match view {
            osd::View::Level { level, muted } => {
                let level = level.clamp(0.0, 1.0);
                // Segoe MDL2 Assets: Mute, Volume0..Volume3.
                let glyph = match (*muted, level) {
                    (true, _) => "\u{E74F}",
                    (_, l) if l <= 0.0 => "\u{E992}",
                    (_, l) if l < 0.34 => "\u{E993}",
                    (_, l) if l < 0.67 => "\u{E994}",
                    _ => "\u{E995}",
                };
                self.text(glyph, &f.icon, p.icon, if *muted { DIM } else { TEXT });
                self.fill(p.track, 4.0, TRACK);
                let fill = Rect::new(p.track.x, p.track.y, p.track.w * level, p.track.h);
                self.fill(fill, 4.0, if *muted { MUTED } else { ACCENT });
                self.text(
                    &view.value_text(),
                    if *muted { &f.body_right } else { &f.tab_right },
                    p.value,
                    if *muted { DIM } else { TEXT },
                );
            }
            osd::View::Unavailable(reason) => {
                self.text("\u{E74F}", &f.icon, p.icon, DIM);
                let r = Rect::new(p.track.x, 0.0, p.card.w - p.track.x - 16.0, p.card.h);
                self.text(&format!("Audio unavailable: {reason}"), &f.small, r, DIM);
            }
        }
    }

    fn scene(&self, model: &Model, l: &Layout) {
        let f = &self.fonts;
        self.fill(l.screen, 0.0, BACKDROP);
        self.fill(l.card, 18.0, CARD);
        self.stroke(l.card, 18.0, CARD_EDGE, 1.0);

        for (i, tab) in Tab::ALL.iter().enumerate() {
            let r = l.tab(i);
            let active = *tab == model.tab;
            let label = Rect::new(r.x + 12.0, r.y, r.w - 12.0, r.h - 8.0);
            self.text(tab.title(), &f.tab, label, if active { TEXT } else { DIM });
            if active {
                self.fill(
                    Rect::new(r.x + 12.0, r.bottom() - 8.0, 44.0, 3.0),
                    1.5,
                    ACCENT,
                );
            }
        }
        let header_info = match model.tab {
            Tab::Tasks if !model.tasks.is_empty() => {
                format!("{} / {}", model.selected + 1, model.tasks.len())
            }
            _ => String::new(),
        };
        self.text(&header_info, &f.small_right, l.header, DIM);

        match model.tab {
            Tab::Tasks => self.tasks(model, l),
            Tab::Volume => self.volume(model, l),
            Tab::Info => self.info(model, l),
        }

        self.footer(model, l);
        if let Some(c) = &model.confirm {
            self.confirm(c, l);
        }
    }

    fn tasks(&self, model: &Model, l: &Layout) {
        let f = &self.fonts;
        if model.tasks.is_empty() {
            self.text("No open windows", &f.body_center, l.body, DIM);
            return;
        }
        for t in l.tiles(model.tasks.len(), model.scroll_row) {
            let task = &model.tasks[t.index];
            let selected = t.index == model.selected;
            self.fill(t.outer, 12.0, if selected { TILE_SELECTED } else { TILE });
            self.fill(t.thumb, 8.0, THUMB_BG);
            // Shown only if DWM has no thumbnail to draw over it.
            self.text(
                &model.process_name(task),
                &f.small,
                t.thumb.inset(12.0),
                MUTED,
            );
            self.text(
                &task.title,
                &f.body,
                t.label,
                if selected { TEXT } else { DIM },
            );
            if selected {
                self.stroke(t.outer.inset(1.5), 11.0, ACCENT, 3.0);
            }
        }
        let shown = l.visible_rows() * l.columns;
        if model.tasks.len() > (model.scroll_row * l.columns) + shown {
            let r = Rect::new(l.body.x, l.body.bottom() - 4.0, l.body.w, 16.0);
            self.text("more below", &f.small_right, r, DIM);
        }
    }

    fn volume(&self, model: &Model, l: &Layout) {
        let f = &self.fonts;
        if let Some(e) = &model.volume.error {
            self.text(
                &format!("Audio unavailable: {e}"),
                &f.body_center,
                l.body,
                DIM,
            );
            return;
        }
        let rows = model.volume_rows();
        if rows.is_empty() {
            self.text("No audio output device", &f.body_center, l.body, DIM);
            return;
        }
        let visible = l.volume_rows_visible();
        let first = (model.volume_row + 1).saturating_sub(visible);
        for (i, row) in rows.iter().enumerate().skip(first).take(visible) {
            let y = l.body.y + (i - first) as f32 * ROW_H;
            let r = Rect::new(l.body.x, y, l.body.w, ROW_H - 8.0);
            if i == model.volume_row {
                self.fill(r, 10.0, TILE_SELECTED);
                self.fill(Rect::new(r.x, r.y + 12.0, 4.0, r.h - 24.0), 2.0, ACCENT);
            }
            let name = match row {
                VolumeRow::Master(_) => "Master volume".to_owned(),
                VolumeRow::App {
                    session,
                    selected_task: true,
                } => format!("{}  (selected task)", session.name),
                VolumeRow::App { session, .. } => session.name.clone(),
            };
            let label_w = (r.w * 0.36).min(380.0);
            self.text(
                &name,
                &f.body,
                Rect::new(r.x + 20.0, r.y, label_w - 20.0, r.h),
                TEXT,
            );

            let value_w = 90.0;
            let track = Rect::new(
                r.x + label_w + 12.0,
                r.y + r.h / 2.0 - 4.0,
                (r.w - label_w - value_w - 36.0).max(10.0),
                8.0,
            );
            self.fill(track, 4.0, TRACK);
            let level = row.level().clamp(0.0, 1.0);
            let fill = Rect::new(track.x, track.y, track.w * level, track.h);
            self.fill(fill, 4.0, if row.muted() { MUTED } else { ACCENT });
            let value = if row.muted() {
                "Muted".to_owned()
            } else {
                format!("{:.0}%", level * 100.0)
            };
            let vr = Rect::new(r.right() - value_w - 16.0, r.y, value_w, r.h);
            self.text(
                &value,
                &f.body_right,
                vr,
                if row.muted() { DIM } else { TEXT },
            );
        }
    }

    fn info(&self, model: &Model, l: &Layout) {
        let f = &self.fonts;
        let Some(task) = model.selected_task() else {
            self.text("No task selected", &f.body_center, l.body, DIM);
            return;
        };
        let b = l.body;
        self.text(
            &model.process_name(task),
            &f.big,
            Rect::new(b.x, b.y, b.w, 44.0),
            TEXT,
        );
        self.text(
            &task.title,
            &f.body,
            Rect::new(b.x, b.y + 44.0, b.w, 28.0),
            DIM,
        );

        let stats = model.selected_stats();
        let pct = |v: Option<f32>| v.map_or_else(|| "…".to_owned(), |v| format!("{v:.1}%"));
        let rows: [(&str, String); 6] = [
            ("CPU", pct(stats.and_then(|s| s.cpu_percent))),
            ("GPU", pct(stats.and_then(|s| s.gpu_percent))),
            (
                "Memory (working set)",
                stats.map_or("…".into(), |s| {
                    uc_sysinfo::format_bytes(s.working_set_bytes)
                }),
            ),
            (
                "Private bytes",
                stats.map_or("…".into(), |s| uc_sysinfo::format_bytes(s.private_bytes)),
            ),
            (
                "Running for",
                stats.map_or("…".into(), |s| uc_sysinfo::format_uptime(s.uptime)),
            ),
            ("Process ID", task.pid.to_string()),
        ];
        let top = b.y + 92.0;
        let col_w = (b.w / 2.0).min(420.0);
        for (i, (k, v)) in rows.iter().enumerate() {
            let y = top + i as f32 * 44.0;
            if y + 40.0 > b.bottom() {
                break;
            }
            self.text(k, &f.body, Rect::new(b.x, y, col_w, 40.0), DIM);
            self.text(
                v,
                &f.tab,
                Rect::new(b.x + col_w, y, b.w - col_w, 40.0),
                TEXT,
            );
        }
    }

    fn footer(&self, model: &Model, l: &Layout) {
        let f = &self.fonts;
        let r = l.footer;
        self.fill(Rect::new(r.x, r.y, r.w, 1.0), 0.0, CARD_EDGE);
        let s = &model.stats.system;
        let pct = |v: Option<f32>| v.map_or_else(|| "--".to_owned(), |v| format!("{v:.0}%"));
        let system = if s.memory_total_bytes > 0 {
            format!(
                "CPU {}    GPU {}    RAM {} / {}",
                pct(s.cpu_percent),
                pct(s.gpu_percent),
                uc_sysinfo::format_bytes(s.memory_used_bytes),
                uc_sysinfo::format_bytes(s.memory_total_bytes),
            )
        } else {
            String::new()
        };
        let half = r.w / 2.0;
        self.text(&system, &f.small, Rect::new(r.x, r.y + 1.0, half, r.h), DIM);
        let hints = match model.tab {
            Tab::Tasks => "A Switch   X Close   Y End task   LB/RB Panels   B Back",
            Tab::Volume => "◀ ▶ Adjust   A Mute   LT/RT Master   LB/RB Panels   B Back",
            Tab::Info => "◀ ▶ Task   A Switch   LB/RB Panels   B Back",
        };
        self.text(
            hints,
            &f.small_right,
            Rect::new(r.x + half, r.y + 1.0, half, r.h),
            DIM,
        );
    }

    fn confirm(&self, c: &Confirm, l: &Layout) {
        let f = &self.fonts;
        self.fill(l.card, 18.0, hex(0x000000, 0.6));
        let (w, h) = (560.0_f32.min(l.card.w - 40.0), 220.0);
        let r = Rect::new(
            l.card.x + (l.card.w - w) / 2.0,
            l.card.y + (l.card.h - h) / 2.0,
            w,
            h,
        );
        self.fill(r, 14.0, CARD);
        self.stroke(r, 14.0, DANGER, 2.0);
        let inner = r.inset(28.0);
        match c {
            Confirm::Kill { name, pid } => {
                self.text(
                    "End this process?",
                    &f.big,
                    Rect::new(inner.x, inner.y, inner.w, 44.0),
                    TEXT,
                );
                self.text(
                    &format!("{name}  (PID {pid}). Unsaved work will be lost."),
                    &f.body,
                    Rect::new(inner.x, inner.y + 52.0, inner.w, 30.0),
                    DIM,
                );
            }
        }
        self.text(
            "A / Enter  End process        B / Esc  Cancel",
            &f.body,
            Rect::new(inner.x, inner.bottom() - 32.0, inner.w, 32.0),
            TEXT,
        );
    }
}

fn color(c: Rgba) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.0,
        g: c.1,
        b: c.2,
        a: c.3,
    }
}
