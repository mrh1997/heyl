//! The built-in approval window (Windows, macOS).
//!
//! `heyl` runs this in a **second process** of itself — `heyl
//! __approval-window <unlock|pair>` — and talks to it over the protocol in
//! `heyl_platform::approval_ui`: the request arrives in the environment, the
//! end of stdin closes the window, and `cancel` on stdout aborts the wait.
//! Keeping it out of the main process keeps the GUI stack away from the seed,
//! and gives the window the main thread macOS insists on.
//!
//! No widget toolkit: winit opens the window, softbuffer takes pixels, and the
//! text, the code and the one button are drawn by hand. The font is Noto Sans
//! Mono, pre-rasterised — monospace makes wrapping a division.
//!
//! Linux has no Rust window at all: see `heyl_platform::approval_ui::LINUX_SCRIPT`.

use std::io::{Read as _, Write as _};
use std::num::NonZeroU32;
use std::process::ExitCode;
use std::sync::Arc;

use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowButtons, WindowId, WindowLevel};

/// The process could not put a window on screen; heyl falls back.
const NO_WINDOW: u8 = 3;

const BACKGROUND: u32 = 0x00F4_F4F4;
const TEXT: u32 = 0x0020_2020;
const MUTED: u32 = 0x0060_6060;
const BUTTON: u32 = 0x00E2_E2E2;
const BUTTON_HOVER: u32 = 0x00D0_D0D0;
const BORDER: u32 = 0x00A0_A0A0;
const WHITE: u32 = 0x00FF_FFFF;
const BLACK: u32 = 0x0000_0000;

/// Logical sizes, before the monitor's scale factor.
const PADDING: f64 = 24.0;
const GAP: f64 = 14.0;
const MODULE: f64 = 5.0;
const QUIET_ZONE: usize = 4;
/// Characters per line of body text.
const COLUMNS: usize = 56;

/// What the window shows.
enum Content {
    Unlock {
        heading: String,
        message: String,
    },
    Pair {
        message: String,
        url: String,
        code: qrcode::QrCode,
    },
}

/// Run the window. `kind` is `unlock` or `pair`.
pub fn run(kind: &str) -> ExitCode {
    let message = std::env::var("HEYL_UI_MESSAGE").unwrap_or_default();
    let content = match kind {
        "pair" => {
            let url = std::env::var("HEYL_UI_URL").unwrap_or_default();
            let Ok(code) = qrcode::QrCode::new(url.as_bytes()) else {
                return ExitCode::from(NO_WINDOW);
            };
            Content::Pair { message, url, code }
        }
        _ => Content::Unlock {
            heading: std::env::var("HEYL_UI_HEADING")
                .unwrap_or_else(|_| "Approve on your phone".to_owned()),
            message,
        },
    };

    let Ok(event_loop) = event_loop() else {
        return ExitCode::from(NO_WINDOW);
    };
    watch_stdin(event_loop.create_proxy());

    let mut app = App {
        content,
        window: None,
        surface: None,
        cursor: PhysicalPosition::new(-1.0, -1.0),
        button: Rect::default(),
        hover: false,
        status: None,
    };
    if event_loop.run_app(&mut app).is_err() && app.status.is_none() {
        return ExitCode::from(NO_WINDOW);
    }
    ExitCode::from(app.status.unwrap_or(NO_WINDOW))
}

/// Sent when stdin ends: heyl is done with us.
struct Done;

#[cfg(target_os = "macos")]
fn event_loop() -> Result<EventLoop<Done>, winit::error::EventLoopError> {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS as _};
    // Accessory: a window without a Dock icon, for a process that is a dialog
    // and nothing else. It still comes to the front.
    EventLoop::with_user_event()
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_activate_ignoring_other_apps(true)
        .build()
}

#[cfg(not(target_os = "macos"))]
fn event_loop() -> Result<EventLoop<Done>, winit::error::EventLoopError> {
    EventLoop::with_user_event().build()
}

/// Read stdin to its end, then wake the event loop.
///
/// The final line says how the wait ended; the window closes the same way
/// whatever it says, so it is read and discarded.
fn watch_stdin(proxy: EventLoopProxy<Done>) {
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        let _ = proxy.send_event(Done);
    });
}

/// Tell heyl something, one word per line.
fn say(word: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{word}");
    let _ = out.flush();
}

type Surface = softbuffer::Surface<Arc<Window>, Arc<Window>>;

struct App {
    content: Content,
    window: Option<Arc<Window>>,
    surface: Option<Surface>,
    cursor: PhysicalPosition<f64>,
    button: Rect,
    hover: bool,
    /// Set when the window is done; the process exits with it.
    status: Option<u8>,
}

impl App {
    fn finish(&mut self, event_loop: &ActiveEventLoop, status: u8) {
        self.status.get_or_insert(status);
        event_loop.exit();
    }

    fn cancel(&mut self, event_loop: &ActiveEventLoop) {
        say("cancel");
        self.finish(event_loop, 0);
    }

    fn open(&mut self, event_loop: &ActiveEventLoop) -> Option<()> {
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        let scale = monitor
            .as_ref()
            .map_or(1.0, winit::monitor::MonitorHandle::scale_factor);
        let style = Style::for_scale(scale);
        let (width, height) = layout(&self.content, &style, None).size;

        let title = match self.content {
            Content::Unlock { .. } => "heyl: approve on your phone",
            Content::Pair { .. } => "heyl: pair with your phone",
        };
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(width, height))
            .with_resizable(false)
            .with_enabled_buttons(WindowButtons::CLOSE)
            .with_window_level(WindowLevel::AlwaysOnTop)
            // Hidden until centred, so it does not appear in one place and
            // jump to another.
            .with_visible(false);
        let window = Arc::new(event_loop.create_window(attributes).ok()?);

        if let Some(monitor) = monitor {
            let area = monitor.size();
            let origin = monitor.position();
            let outer = window.outer_size();
            let half =
                |free: u32, used: u32| i32::try_from(free.saturating_sub(used) / 2).unwrap_or(0);
            let x = origin.x + half(area.width, outer.width);
            let y = origin.y + half(area.height, outer.height);
            window.set_outer_position(PhysicalPosition::new(x, y));
        }

        let context = softbuffer::Context::new(Arc::clone(&window)).ok()?;
        let surface = softbuffer::Surface::new(&context, Arc::clone(&window)).ok()?;
        window.set_visible(true);
        window.focus_window();
        window.request_redraw();
        self.surface = Some(surface);
        self.window = Some(window);
        Some(())
    }

    fn redraw(&mut self) -> Option<()> {
        let window = self.window.as_ref()?;
        let surface = self.surface.as_mut()?;
        let size = window.inner_size();
        let (w, h) = (NonZeroU32::new(size.width)?, NonZeroU32::new(size.height)?);
        surface.resize(w, h).ok()?;
        let mut buffer = surface.buffer_mut().ok()?;
        let style = Style::for_scale(window.scale_factor());
        let mut canvas = Canvas {
            pixels: &mut buffer,
            width: size.width as usize,
            height: size.height as usize,
        };
        let placed = layout(&self.content, &style, Some((&mut canvas, self.hover)));
        self.button = placed.button;
        buffer.present().ok()
    }
}

impl ApplicationHandler<Done> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() && self.open(event_loop).is_none() {
            self.finish(event_loop, NO_WINDOW);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: Done) {
        self.finish(event_loop, 0);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested
            | WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => self.cancel(event_loop),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                let hover = self.button.contains(position);
                if hover != self.hover {
                    self.hover = hover;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = PhysicalPosition::new(-1.0, -1.0);
                if self.hover {
                    self.hover = false;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } if self.button.contains(self.cursor) => self.cancel(event_loop),
            WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Resized(_) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let first = self.button == Rect::default();
                if self.redraw().is_none() {
                    self.finish(event_loop, NO_WINDOW);
                } else if first {
                    say("shown");
                }
            }
            _ => {}
        }
    }
}

/// Font sizes and spacing for one scale factor.
struct Style {
    scale: f64,
    heading: RasterHeight,
    body: RasterHeight,
}

impl Style {
    fn for_scale(scale: f64) -> Self {
        let mut style = Self {
            scale,
            heading: RasterHeight::Size24,
            body: RasterHeight::Size16,
        };
        style.heading = raster_for(style.px(24.0));
        style.body = raster_for(style.px(16.0));
        style
    }

    fn px(&self, logical: f64) -> usize {
        // Rounded, non-negative logical sizes: the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = (logical * self.scale).round().max(0.0) as usize;
        px
    }
}

/// The largest pre-rasterised size not above `px`, never below the smallest.
fn raster_for(px: usize) -> RasterHeight {
    [
        RasterHeight::Size32,
        RasterHeight::Size24,
        RasterHeight::Size20,
    ]
    .into_iter()
    .find(|h| px >= h.val())
    .unwrap_or(RasterHeight::Size16)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    /// Window coordinates are far below 2^52: the casts are exact.
    #[allow(clippy::cast_precision_loss)]
    fn contains(&self, point: PhysicalPosition<f64>) -> bool {
        let (left, top) = (self.x as f64, self.y as f64);
        let (right, bottom) = (left + self.w as f64, top + self.h as f64);
        point.x >= left && point.y >= top && point.x < right && point.y < bottom
    }
}

/// Where everything went.
struct Placed {
    size: (u32, u32),
    button: Rect,
}

/// Lay the content out, and draw it when given a canvas.
///
/// One function for both, so the window is sized by exactly the code that
/// fills it.
fn layout(content: &Content, style: &Style, mut draw: Option<(&mut Canvas<'_>, bool)>) -> Placed {
    let pad = style.px(PADDING);
    let gap = style.px(GAP);
    let body_w = get_raster_width(FontWeight::Regular, style.body);
    let line_h = style.body.val() + style.px(4.0);
    let inner_w = COLUMNS * body_w;

    if let Some((canvas, _)) = draw.as_mut() {
        canvas.fill(
            Rect {
                x: 0,
                y: 0,
                w: canvas.width,
                h: canvas.height,
            },
            BACKGROUND,
        );
    }

    let mut y = pad;
    let (heading, message) = match content {
        Content::Unlock { heading, message } => (heading.as_str(), message),
        Content::Pair { message, .. } => ("Pair with your phone", message),
    };
    // The heading carries a user-chosen name, so it wraps like the body does.
    let heading_columns = (inner_w / get_raster_width(FontWeight::Bold, style.heading)).max(1);
    for line in wrap(heading, heading_columns) {
        if let Some((canvas, _)) = draw.as_mut() {
            canvas.text(pad, y, &line, FontWeight::Bold, style.heading, TEXT);
        }
        y += style.heading.val() + style.px(4.0);
    }
    y += gap - style.px(4.0).min(gap);

    for line in wrap(message, COLUMNS) {
        if let Some((canvas, _)) = draw.as_mut() {
            canvas.text(pad, y, &line, FontWeight::Regular, style.body, TEXT);
        }
        y += line_h;
    }

    if let Content::Pair { url, code, .. } = content {
        y += gap;
        let module = style.px(MODULE).max(2);
        let modules = code.width() + QUIET_ZONE * 2;
        let side = modules * module;
        let x0 = pad + inner_w.saturating_sub(side) / 2;
        if let Some((canvas, _)) = draw.as_mut() {
            canvas.qr(code, x0, y, module);
        }
        y += side + gap;
        for line in wrap_hard(url, COLUMNS) {
            if let Some((canvas, _)) = draw.as_mut() {
                canvas.text(pad, y, &line, FontWeight::Regular, style.body, MUTED);
            }
            y += line_h;
        }
    }

    y += gap;
    let label = "Cancel";
    let bw = label.len() * body_w + style.px(40.0);
    let bh = style.body.val() + style.px(16.0);
    let button = Rect {
        x: pad + inner_w - bw,
        y,
        w: bw,
        h: bh,
    };
    if let Some((canvas, hover)) = draw.as_mut() {
        canvas.button(button, label, style, *hover);
    }
    y += bh + pad;

    let width = u32::try_from(inner_w + 2 * pad).unwrap_or(u32::MAX);
    let height = u32::try_from(y).unwrap_or(u32::MAX);
    Placed {
        size: (width, height),
        button,
    }
}

/// Word-wrap to `columns`, keeping the text's own line breaks.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let needed = usize::from(!line.is_empty()) + word.chars().count();
            if !line.is_empty() && line.chars().count() + needed > columns {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.extend(wrap_hard(&line, columns));
    }
    lines
}

/// Break every `columns` characters: for a URL, which has no spaces.
fn wrap_hard(text: &str, columns: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(columns)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

struct Canvas<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
}

impl Canvas<'_> {
    /// A code, black on white, quiet zone included, `module` pixels a module.
    fn qr(&mut self, code: &qrcode::QrCode, x: usize, y: usize, module: usize) {
        let side = (code.width() + QUIET_ZONE * 2) * module;
        self.fill(
            Rect {
                x,
                y,
                w: side,
                h: side,
            },
            WHITE,
        );
        for (i, color) in code.to_colors().iter().enumerate() {
            if *color == qrcode::Color::Dark {
                let (mx, my) = (i % code.width() + QUIET_ZONE, i / code.width() + QUIET_ZONE);
                self.fill(
                    Rect {
                        x: x + mx * module,
                        y: y + my * module,
                        w: module,
                        h: module,
                    },
                    BLACK,
                );
            }
        }
    }

    /// A bordered button with its label centred.
    fn button(&mut self, r: Rect, label: &str, style: &Style, hover: bool) {
        let border = style.px(1.0).max(1);
        let advance = get_raster_width(FontWeight::Regular, style.body);
        self.fill(r, BORDER);
        self.fill(
            Rect {
                x: r.x + border,
                y: r.y + border,
                w: r.w - 2 * border,
                h: r.h - 2 * border,
            },
            if hover { BUTTON_HOVER } else { BUTTON },
        );
        self.text(
            r.x + (r.w - label.chars().count() * advance) / 2,
            r.y + (r.h - style.body.val()) / 2,
            label,
            FontWeight::Regular,
            style.body,
            TEXT,
        );
    }

    fn fill(&mut self, r: Rect, color: u32) {
        for y in r.y..(r.y + r.h).min(self.height) {
            let row = y * self.width;
            for x in r.x..(r.x + r.w).min(self.width) {
                self.pixels[row + x] = color;
            }
        }
    }

    fn text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        weight: FontWeight,
        size: RasterHeight,
        color: u32,
    ) {
        let advance = get_raster_width(weight, size);
        for (i, c) in text.chars().enumerate() {
            let Some(glyph) = get_raster(c, weight, size).or_else(|| get_raster('?', weight, size))
            else {
                continue;
            };
            for (gy, row) in glyph.rows().enumerate() {
                for (gx, alpha) in row.iter().enumerate() {
                    let (px, py) = (x + i * advance + gx, y + gy);
                    if *alpha == 0 || px >= self.width || py >= self.height {
                        continue;
                    }
                    let at = py * self.width + px;
                    self.pixels[at] = blend(self.pixels[at], color, *alpha);
                }
            }
        }
    }
}

/// `fg` over `bg` at coverage `alpha`.
fn blend(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let channel = |shift: u32| {
        let b = (bg >> shift) & 0xFF;
        let f = (fg >> shift) & 0xFF;
        ((f * a + b * (255 - a)) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_wrap_and_line_breaks_survive() {
        assert_eq!(
            wrap("one two three\nfour", 7),
            vec!["one two", "three", "four"]
        );
    }

    /// A long display name grows the window downwards, never sideways.
    #[test]
    fn a_long_heading_wraps_instead_of_widening() {
        let style = Style::for_scale(1.0);
        let unlock = |heading: &str| Content::Unlock {
            heading: heading.to_owned(),
            message: "Approve in your heylogin app".to_owned(),
        };
        let short = layout(&unlock("heyl CLI requests passwords"), &style, None).size;
        let long = layout(
            &unlock("Claude Code on build-server-07 in the basement requests passwords"),
            &style,
            None,
        )
        .size;
        assert_eq!(long.0, short.0, "same width");
        assert!(long.1 > short.1, "taller: {long:?} vs {short:?}");
    }

    #[test]
    fn a_url_breaks_mid_word() {
        assert_eq!(wrap_hard("abcdefg", 3), vec!["abc", "def", "g"]);
    }

    #[test]
    fn full_coverage_is_the_foreground() {
        assert_eq!(blend(WHITE, BLACK, 255), BLACK);
        assert_eq!(blend(WHITE, BLACK, 0), WHITE);
    }
}
