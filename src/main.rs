use std::{
    collections::HashMap, env, error::Error, fs, io, num::NonZeroU32, path::PathBuf, sync::Arc,
};

use hayro::{
    PixmapSettings, RenderCache, RenderSettings, hayro_interpret::InterpreterSettings,
    hayro_syntax::Pdf, render, vello_cpu::color::palette::css::WHITE,
};

use softbuffer::{Context, Surface};

use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 900;

const PAGE_MARGIN: u32 = 24;
const PAGE_GAP: u32 = 24;

// Wheel line movement in physical pixels.
const WHEEL_LINE_PIXELS: f64 = 80.0;

// Background: 0x00RRGGBB (softbuffer format).
const BACKGROUND: u32 = 0x001B1B1B;

#[derive(Clone, Copy)]
struct PageInfo {
    width: f32,
    height: f32,
}

struct CachedPage {
    width: u32,
    height: u32,
    // softbuffer-compatible 0x00RRGGBB pixels.
    pixels: Vec<u32>,
}

struct PageLayout {
    x: f64,
    y: i32,
    width: u32,
    height: u32,
    scale: f32,
}

struct LayoutSet {
    pages: Vec<PageLayout>,
    total_width: f64,
}

impl LayoutSet {
    fn max_scroll(&self, viewport_width: u32) -> f64 {
        (self.total_width - viewport_width as f64).max(0.0)
    }
}

struct Viewer {
    pdf: Pdf,
    page_info: Vec<PageInfo>,
    // Pages are rendered only when needed.
    cache: HashMap<usize, CachedPage>,
    interpreter: InterpreterSettings,
    scroll_x: f64,
    document_name: String,
}

impl Viewer {
    fn open(path: &PathBuf) -> Result<Self, Box<dyn Error>> {
        let bytes = fs::read(path)?;

        // Parse PDF structure, without rasterizing every page.
        let pdf = Pdf::new(bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to load PDF: {e:?}"),
            )
        })?;

        // Page geometry is cheap compared with rasterization.
        let page_info: Vec<PageInfo> = pdf
            .pages()
            .iter()
            .map(|page| {
                let (width, height) = page.render_dimensions();

                PageInfo {
                    width: width.max(1.0),
                    height: height.max(1.0),
                }
            })
            .collect();

        if page_info.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "PDF contains no pages").into());
        }

        let document_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF".to_string());

        Ok(Self {
            pdf,
            page_info,
            cache: HashMap::new(),
            interpreter: InterpreterSettings::default(),
            scroll_x: 0.0,
            document_name,
        })
    }

    fn page_count(&self) -> usize {
        self.page_info.len()
    }

    /// Horizontal page layout.
    fn layout(&self, viewport_width: u32, viewport_height: u32) -> LayoutSet {
        let available_height = viewport_height.saturating_sub(PAGE_MARGIN * 2).max(1) as f32;

        let mut layouts = Vec::with_capacity(self.page_count());

        let mut x = PAGE_MARGIN as f64;

        for info in &self.page_info {
            // Fit each page to the available window height.
            let fit_scale = available_height / info.height;

            // Hayro's Pixmap dimensions are u16.
            let max_scale = u16::MAX as f32 / info.width.max(info.height);

            let scale = fit_scale.min(max_scale);

            let width = ((info.width * scale) as u32).max(1);
            let height = ((info.height * scale) as u32).max(1);

            let y = ((viewport_height as f64 - height as f64) / 2.0) as i32;

            layouts.push(PageLayout {
                x,
                y,
                width,
                height,
                scale,
            });

            // Horizontal continuous page strip.
            x += width as f64 + PAGE_GAP as f64;
        }

        let total_width = layouts
            .last()
            .map(|page| page.x + page.width as f64 + PAGE_MARGIN as f64)
            .unwrap_or(viewport_width as f64);

        LayoutSet {
            pages: layouts,
            total_width,
        }
    }

    /// Lazily render the PDF pages.
    fn render_page(&self, index: usize, scale: f32) -> CachedPage {
        let page = &self.pdf.pages()[index];

        // A cache is created for this render operation.
        // Only one page is rasterized here.
        let render_cache = RenderCache::new();

        let pixmap_settings = PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        };

        let pixmap = render(
            page,
            &render_cache,
            &self.interpreter,
            &RenderSettings::default(),
            &pixmap_settings,
        );

        let width = pixmap.width() as u32;
        let height = pixmap.height() as u32;

        // Hayro returns premultiplied RGBA8.
        // Convert to softbuffer's 0x00RRGGBB format.
        let rgba = pixmap.data_as_u8_slice();

        let pixels: Vec<u32> = rgba
            .chunks_exact(4)
            .map(|p| {
                let a = p[3] as u32;

                // Composite onto white background.
                let r = (p[0] as u32 + 255 - a).min(255);
                let g = (p[1] as u32 + 255 - a).min(255);
                let b = (p[2] as u32 + 255 - a).min(255);

                (r << 16) | (g << 8) | b
            })
            .collect();

        CachedPage {
            width,
            height,
            pixels,
        }
    }

    fn current_page(&self, layouts: &LayoutSet, viewport_width: u32) -> usize {
        let focus_x = self.scroll_x + viewport_width as f64 * 0.5;

        layouts
            .pages
            .iter()
            .position(|page| page.x + page.width as f64 >= focus_x)
            .unwrap_or_else(|| self.page_count() - 1)
    }

    fn step_page(&mut self, direction: i32, viewport_width: u32, viewport_height: u32) {
        let layouts = self.layout(viewport_width, viewport_height);

        let current = self.current_page(&layouts, viewport_width);

        let target = if direction > 0 {
            (current + 1).min(self.page_count() - 1)
        } else {
            current.saturating_sub(1)
        };

        // Align the target page to the left margin.
        self.scroll_x = (layouts.pages[target].x - PAGE_MARGIN as f64)
            .clamp(0.0, layouts.max_scroll(viewport_width));
    }

    fn scroll_vertical(&mut self, delta_y: f64, viewport_width: u32, viewport_height: u32) {
        let layouts = self.layout(viewport_width, viewport_height);

        // Wheel up   -> left
        // Wheel down -> right
        self.scroll_x = (self.scroll_x - delta_y).clamp(0.0, layouts.max_scroll(viewport_width));
    }
}

fn blit_page(
    dst: &mut [u32],
    dst_width: u32,
    dst_height: u32,
    src: &CachedPage,
    layout: &PageLayout,
    scroll_x: f64,
) {
    let dx = (layout.x - scroll_x).round() as i64;
    let dy = layout.y as i64;

    let dst_left = dx.max(0);
    let dst_top = dy.max(0);

    let dst_right = (dx + src.width as i64).min(dst_width as i64);

    let dst_bottom = (dy + src.height as i64).min(dst_height as i64);

    if dst_left >= dst_right || dst_top >= dst_bottom {
        return;
    }

    let src_left = (dst_left - dx) as usize;
    let src_top = (dst_top - dy) as usize;

    let copy_width = (dst_right - dst_left) as usize;
    let copy_height = (dst_bottom - dst_top) as usize;

    for row in 0..copy_height {
        let src_start = (src_top + row) * src.width as usize + src_left;

        let dst_start = (dst_top as usize + row) * dst_width as usize + dst_left as usize;

        dst[dst_start..dst_start + copy_width]
            .copy_from_slice(&src.pixels[src_start..src_start + copy_width]);
    }
}

struct App {
    viewer: Viewer,
    window: Option<Arc<Window>>,
    // Surface must be dropped before Context and Window.
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    _context: Option<Context<Arc<Window>>>,
}

impl App {
    fn new(viewer: Viewer) -> Self {
        Self {
            viewer,
            window: None,
            surface: None,
            _context: None,
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };

        let size = window.inner_size();

        if size.width == 0 || size.height == 0 {
            return;
        }

        // Recompute horizontal layout.
        let layouts = self.viewer.layout(size.width, size.height);

        // Clamp horizontal scrolling.
        self.viewer.scroll_x = self
            .viewer
            .scroll_x
            .clamp(0.0, layouts.max_scroll(size.width));

        let left = self.viewer.scroll_x;
        let right = left + size.width as f64;

        // Determine visible page range.
        let first_visible = layouts
            .pages
            .iter()
            .position(|page| page.x + page.width as f64 >= left)
            .unwrap_or(0);

        let last_visible = layouts
            .pages
            .iter()
            .rposition(|page| page.x <= right)
            .unwrap_or(first_visible)
            .max(first_visible);

        // Render one page before and after the viewport.
        let render_start = first_visible.saturating_sub(1);
        let render_end = (last_visible + 2).min(layouts.pages.len());

        // Drop pages far away from current viewport.
        self.viewer.cache.retain(|index, _| {
            *index >= first_visible.saturating_sub(2) && *index <= last_visible + 2
        });

        // On-demand rasterization.
        for index in render_start..render_end {
            if self.viewer.cache.contains_key(&index) {
                continue;
            }

            let scale = layouts.pages[index].scale;

            let rendered = self.viewer.render_page(index, scale);

            self.viewer.cache.insert(index, rendered);
        }

        let current = self.viewer.current_page(&layouts, size.width);

        let title = format!(
            "{} — {}/{}",
            self.viewer.document_name,
            current + 1,
            self.viewer.page_count(),
        );

        window.set_title(&title);

        let Some(surface) = self.surface.as_mut() else {
            return;
        };

        // Resize software rendering surface.
        if let Err(error) = surface.resize(
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        ) {
            eprintln!("surface resize error: {error}");
            return;
        }

        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(error) => {
                eprintln!("buffer error: {error}");
                return;
            }
        };

        // Clear background.
        buffer.fill(BACKGROUND);

        let output: &mut [u32] = &mut *buffer;

        // Draw cached visible pages.
        for (index, layout) in layouts.pages.iter().enumerate() {
            if let Some(page) = self.viewer.cache.get(&index) {
                blit_page(
                    output,
                    size.width,
                    size.height,
                    page,
                    layout,
                    self.viewer.scroll_x,
                );
            }
        }

        if let Err(error) = buffer.present() {
            eprintln!("present error: {error}");
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);

        if self.surface.is_some() {
            return;
        }

        if self.window.is_none() {
            let attributes = Window::default_attributes()
                .with_title(&self.viewer.document_name)
                .with_inner_size(PhysicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT));

            let window = match event_loop.create_window(attributes) {
                Ok(window) => Arc::new(window),
                Err(error) => {
                    eprintln!("window creation error: {error}");
                    event_loop.exit();
                    return;
                }
            };

            self.window = Some(window);
        }

        let window = self.window.as_ref().unwrap().clone();

        let context = match Context::new(window.clone()) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("softbuffer context error: {error}");
                event_loop.exit();
                return;
            }
        };

        let surface = match Surface::new(&context, window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("softbuffer surface error: {error}");
                event_loop.exit();
                return;
            }
        };

        self._context = Some(context);
        self.surface = Some(surface);

        window.request_redraw();
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // The old surface is no longer valid after suspension.
        self.surface.take();
        self._context.take();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let belongs_to_window = self
            .window
            .as_ref()
            .is_some_and(|window| window.id() == window_id);

        if !belongs_to_window {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(_) => {
                // Rasterized dimensions depend on viewport height.
                self.viewer.cache.clear();
                self.request_redraw();
            }

            WindowEvent::ScaleFactorChanged { .. } => {
                self.viewer.cache.clear();
                self.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            // vertical wheel -> horizontal scrolling
            WindowEvent::MouseWheel { delta, .. } => {
                let delta_y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * WHEEL_LINE_PIXELS,

                    MouseScrollDelta::PixelDelta(position) => position.y,
                };

                let size = self.window.as_ref().unwrap().inner_size();

                self.viewer
                    .scroll_vertical(delta_y, size.width, size.height);

                self.request_redraw();
            }

            // j -> right, k -> left
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let size = self.window.as_ref().unwrap().inner_size();

                match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyJ) => {
                        self.viewer.step_page(1, size.width, size.height);

                        self.request_redraw();
                    }
                    PhysicalKey::Code(KeyCode::KeyK) => {
                        self.viewer.step_page(-1, size.width, size.height);

                        self.request_redraw();
                    }
                    PhysicalKey::Code(KeyCode::Escape) => {
                        event_loop.exit();
                    }

                    _ => {}
                }
            }

            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let path = env::args_os().nth(1).map(PathBuf::from).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Usage: minimal-pdf <file.pdf>")
    })?;

    let viewer = Viewer::open(&path)?;
    println!("Opening {} ({} pages)", path.display(), viewer.page_count(),);

    let mut app = App::new(viewer);

    let event_loop = EventLoop::new()?;
    event_loop.run_app(&mut app)?;

    Ok(())
}
