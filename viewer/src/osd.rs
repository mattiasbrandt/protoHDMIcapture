//! Draws the overlay (overlay.rs) with egui, over the scaled feed in the
//! same frame (ADR 0001), at the mpv script's sizes and places: font sizes
//! from the window height, a dark translucent box (`&H101010&`, alpha
//! `&H30&`), light text with a one-pixel black outline. Pixels, not
//! points: one egui point is one pixel, as the script works in OSD pixels.

use egui::{Align2, Color32, FontId, LayerId, Pos2, Rect, Vec2};

/// The box: `&H101010&` at ASS alpha `&H30&` (0x30 transparent).
const BOX: Color32 = Color32::from_rgba_unmultiplied_const(16, 16, 16, 255 - 0x30);
const TEXT: Color32 = Color32::from_rgb(0xDD, 0xDD, 0xDD);
const TITLE: Color32 = Color32::WHITE;
/// The status symbols' colours (the wizard's OK, Warning and Failed lines).
const GOOD: Color32 = Color32::from_rgb(0x6C, 0xD0, 0x6C);
const WARN: Color32 = Color32::from_rgb(0xF0, 0xB4, 0x3C);
const BAD: Color32 = Color32::from_rgb(0xF0, 0x5A, 0x5A);

/// A page line as drawn: the wizard's status prefixes ("OK: ",
/// "Warning: ", "Failed: ") become a coloured symbol before the rest, and a
/// heading (unindented, ending in ":") takes the title colour. egui's
/// default fonts have these symbols; colour emoji they do not (#31).
fn styled(line: &str) -> (Option<(&'static str, Color32)>, &str, Color32) {
    for (prefix, symbol, color) in [("OK: ", "✔", GOOD), ("Warning: ", "⚠", WARN), ("Failed: ", "✖", BAD)] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return (Some((symbol, color)), rest, TEXT);
        }
    }
    let heading = line.ends_with(':') && !line.starts_with(char::is_whitespace);
    (None, line, if heading { TITLE } else { TEXT })
}

/// The symbol and the space after it, before a styled line's rest.
fn symbol_lead(symbol: &str) -> String {
    format!("{symbol} ")
}

/// The columns a page is sized for: the wizard wraps its text at 72, and
/// its bullets and the panel's rows sit a little wider.
const PAGE_COLS: usize = 80;

/// One drawn row of a page.
#[derive(Debug, PartialEq)]
struct Row {
    /// The status symbol, on a status line's first row.
    symbol: Option<(&'static str, Color32)>,
    /// A status line's later rows: indented by this symbol's width.
    under: Option<&'static str>,
    text: String,
    color: Color32,
}

/// A page's lines as rows, `cols` columns wide at most: the first line is
/// the title, the rest styled (`styled`), each wrapped with a hanging
/// indent (`hang`).
fn page_rows(lines: &[String], cols: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let (symbol, rest, color) = if i == 0 { (None, l.as_str(), TITLE) } else { styled(l) };
        let width = if symbol.is_some() { cols.saturating_sub(2) } else { cols };
        for (j, text) in wrap_cols(rest, width, hang(rest)).into_iter().enumerate() {
            let (symbol, under) = if j == 0 { (symbol, None) } else { (None, symbol.map(|(s, _)| s)) };
            rows.push(Row { symbol, under, text, color });
        }
    }
    rows
}

/// Where a line's wrapped rows start: under a bullet's text, a
/// "Label   : value" row's value, a choice's label ("▸  1  label"), else
/// under the line's own indent.
fn hang(line: &str) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let ws = chars.iter().take_while(|c| c.is_whitespace()).count();
    if line.trim_start().starts_with("• ") {
        return ws + 2;
    }
    let choice = chars.len() > 6
        && matches!(chars[0], '▸' | ' ')
        && chars[1] == ' '
        && chars[3].is_ascii_digit()
        && (chars[2] == ' ' || chars[2].is_ascii_digit())
        && chars[4] == ' '
        && chars[5] == ' ';
    if choice {
        return 6;
    }
    match line.find(" : ") {
        Some(at) if line[..at].chars().count() < 12 => line[..at].chars().count() + 3,
        _ => ws,
    }
}

/// `text` in rows of at most `cols` characters, broken at spaces where it
/// can be; the later rows start with `hang` spaces.
fn wrap_cols(text: &str, cols: usize, hang: usize) -> Vec<String> {
    let cols = cols.max(1);
    let hang = hang.min(cols / 3);
    let mut rows = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    let mut prefix = 0;
    loop {
        let room = cols - prefix;
        if rest.len() <= room {
            rows.push(" ".repeat(prefix) + &rest.iter().collect::<String>());
            return rows;
        }
        // A tag ("◂ capture chip") runs to the next double space and is
        // never broken.
        let mut in_tag = false;
        let breakable: Vec<bool> = (0..rest.len())
            .map(|k| {
                if rest[k] == '◂' {
                    in_tag = true;
                } else if rest[k] == ' ' && rest.get(k + 1) == Some(&' ') {
                    in_tag = false;
                }
                rest[k] == ' ' && !in_tag
            })
            .collect();
        // The last such space that leaves the row non-empty, else a hard
        // break.
        let cut = (1..=room).rev().find(|&k| breakable[k] && rest[..k].iter().any(|c| *c != ' ')).unwrap_or(room);
        let row: String = rest[..cut].iter().collect();
        rows.push(" ".repeat(prefix) + row.trim_end());
        rest = rest[cut..].iter().copied().skip_while(|c| *c == ' ').collect();
        prefix = hang;
        if rest.is_empty() {
            return rows;
        }
    }
}

/// One frame's overlay.
#[derive(Default)]
pub struct Scene<'a> {
    /// The panel or the tips page.
    pub page: Option<&'a [String]>,
    pub key_bar: Option<&'a str>,
    /// The banner's detail line.
    pub no_signal: Option<&'a str>,
    pub message: Option<&'a str>,
}

pub struct Osd {
    ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
}

impl Osd {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        Self { ctx: egui::Context::default(), renderer: egui_wgpu::Renderer::new(device, format, Default::default()) }
    }

    /// Draws `scene` over `target` (its pixels kept) in a pass of its own,
    /// recorded into `encoder`. Returns command buffers egui needs
    /// submitted before `encoder`'s.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: (u32, u32),
        scene: &Scene,
    ) -> Vec<wgpu::CommandBuffer> {
        let (w, h) = (size.0 as f32, size.1 as f32);
        let mut input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
            ..Default::default()
        };
        if let Some(v) = input.viewports.get_mut(&input.viewport_id) {
            v.native_pixels_per_point = Some(1.0);
        }
        let mut output = self.ctx.run_ui(input, |ui| {
            let painter = ui.ctx().layer_painter(LayerId::background());
            paint(&painter, w, h, scene);
        });
        // Drained: egui checks that every delta was applied.
        let mut textures = std::mem::take(&mut output.textures_delta);
        for (id, deltas) in textures.set.drain() {
            for delta in &deltas {
                self.renderer.update_texture(device, queue, id, delta);
            }
        }
        let prims = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [size.0, size.1], pixels_per_point: output.pixels_per_point };
        let extra = self.renderer.update_buffers(device, queue, encoder, &prims, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.renderer.render(&mut pass.forget_lifetime(), &prims, &screen);
        }
        for id in textures.free.drain() {
            self.renderer.free_texture(&id);
        }
        extra
    }
}

/// Text with the script's one-pixel black border.
fn outlined(p: &egui::Painter, pos: Pos2, anchor: Align2, text: &str, font: FontId, color: Color32) {
    for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
        p.text(pos + Vec2::new(dx, dy), anchor, text, font.clone(), Color32::BLACK);
    }
    p.text(pos, anchor, text, font, color);
}

fn text_width(p: &egui::Painter, text: &str, font: &FontId) -> f32 {
    p.layout_no_wrap(text.to_string(), font.clone(), TEXT).size().x
}

fn paint(p: &egui::Painter, w: f32, h: f32, scene: &Scene) {
    let panel_fs = (h / 40.0).floor().max(14.0);

    // The no-signal banner, centred, under everything else.
    if let Some(detail) = scene.no_signal {
        let fs = (h / 36.0).floor().max(16.0);
        let bw = (text_width(p, detail, &FontId::monospace(fs)) + 4.0 * fs).floor();
        let bh = (fs * 5.0).floor();
        let (x0, y0) = (((w - bw) / 2.0).floor(), ((h - bh) / 2.0).floor());
        p.rect_filled(Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(bw, bh)), 0.0, BOX);
        let cx = (w / 2.0).floor();
        outlined(p, Pos2::new(cx, y0 + (bh * 0.33).floor()), Align2::CENTER_CENTER, "No signal", FontId::monospace((fs * 1.6).floor()), TITLE);
        outlined(p, Pos2::new(cx, y0 + (bh * 0.72).floor()), Align2::CENTER_CENTER, detail, FontId::monospace(fs), TEXT);
    }

    // The panel or the tips, top left, below the messages' line.
    if let Some(lines) = scene.page {
        // One size for every page in a window: from the height, smaller only
        // where PAGE_COLS columns would run past a narrow window (a tiled
        // one). A longer line wraps rather than shrinking its page.
        let cols_px = |fs: f32| text_width(p, &"0".repeat(PAGE_COLS), &FontId::monospace(fs));
        let room = w - 5.0 * panel_fs;
        let wide = cols_px(panel_fs);
        let fs = if wide > room && wide > 0.0 { (panel_fs * room / wide).floor().max(8.0) } else { panel_fs };
        let font = FontId::monospace(fs);
        let cols = ((room / (cols_px(fs) / PAGE_COLS as f32)).floor() as usize).max(PAGE_COLS.min(20));
        let lh = (fs * 1.3).floor();
        let pad = fs;
        let rows = page_rows(lines, cols);
        let lead = |sym: &str| text_width(p, &symbol_lead(sym), &font);
        let row_width = |r: &Row| r.under.or(r.symbol.map(|(s, _)| s)).map_or(0.0, lead) + text_width(p, &r.text, &font);
        let widest = rows.iter().map(row_width).fold(0.0, f32::max);
        let (x0, y0) = ((fs * 1.5).floor(), (fs * 3.5).floor());
        let size = Vec2::new((widest + 2.0 * pad).floor(), rows.len() as f32 * lh + 2.0 * pad);
        p.rect_filled(Rect::from_min_size(Pos2::new(x0, y0), size), 0.0, BOX);
        for (i, r) in rows.iter().enumerate() {
            let pos = Pos2::new(x0 + pad, y0 + pad + i as f32 * lh);
            let mut x = pos.x;
            if let Some((sym, sym_color)) = r.symbol {
                outlined(p, pos, Align2::LEFT_TOP, sym, font.clone(), sym_color);
                x += lead(sym);
            } else if let Some(sym) = r.under {
                x += lead(sym);
            }
            outlined(p, Pos2::new(x, pos.y), Align2::LEFT_TOP, &r.text, font.clone(), r.color);
        }
    }

    // The key bar, bottom centre.
    if let Some(text) = scene.key_bar {
        let fs = (h / 45.0).floor().max(14.0);
        let font = FontId::monospace(fs);
        let bw = (text_width(p, text, &font) + 2.0 * fs).floor();
        let bh = (fs * 2.0).floor();
        let (x0, y0) = (((w - bw) / 2.0).floor(), h - bh - fs);
        p.rect_filled(Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(bw, bh)), 0.0, BOX);
        outlined(p, Pos2::new((w / 2.0).floor(), y0 + (bh / 2.0).floor()), Align2::CENTER_CENTER, text, font, TEXT);
    }

    // A short message, top left, where mpv shows its OSD messages; a long
    // one (an audio error's reason) wraps inside the window.
    if let Some(text) = scene.message {
        let fs = (panel_fs * 1.4).floor();
        let x = (panel_fs * 1.5).floor();
        let font = FontId::monospace(fs);
        let wrap = (w - 2.0 * x).max(fs);
        let pos = Pos2::new(x, panel_fs.floor());
        let edge = p.layout(text.to_string(), font.clone(), Color32::BLACK, wrap);
        for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
            p.galley(pos + Vec2::new(dx, dy), edge.clone(), Color32::BLACK);
        }
        p.galley(pos, p.layout(text.to_string(), font, TITLE, wrap), TITLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay;

    /// The overlay drawn over a test frame on the GPU, written to
    /// $TMPDIR/phc-osd-<page>.png to look at:
    /// `cargo test -- --ignored --nocapture osd_drawn_offscreen`.
    #[test]
    fn a_long_line_wraps_under_its_own_text() {
        assert_eq!(wrap_cols("short", 10, 2), vec!["short"]);
        assert_eq!(wrap_cols("one two three four", 9, 2), vec!["one two", "  three", "  four"]);
        assert_eq!(wrap_cols("abcdefghijkl", 5, 0), vec!["abcde", "fghij", "kl"]);
        assert_eq!(wrap_cols("label  ◂ capture chip", 16, 0), vec!["label", "◂ capture chip"]);
        assert_eq!(hang("  • a bullet"), 4);
        assert_eq!(hang("Support : Tested"), 10);
        assert_eq!(hang("▸  1  UVC Camera"), 6);
        assert_eq!(hang("  12  Something"), 6);
        assert_eq!(hang("plain words: here"), 0);
        let rows = page_rows(&["Title".into(), "Warning: aaaa bbbb cccc".into()], 12);
        assert_eq!(rows[1], Row { symbol: Some(("⚠", WARN)), under: None, text: "aaaa bbbb".into(), color: TEXT });
        assert_eq!(rows[2], Row { symbol: None, under: Some("⚠"), text: "cccc".into(), color: TEXT });
        // Every row fits.
        let long = "• ".to_string() + &"word ".repeat(40);
        assert!(page_rows(&["T".into(), long], 30).iter().all(|r| r.text.chars().count() <= 30));
    }

    #[test]
    fn status_lines_become_symbols_and_headings_take_the_title_colour() {
        assert_eq!(styled("OK: saved."), (Some(("✔", GOOD)), "saved.", TEXT));
        assert_eq!(styled("Warning: node numbers move"), (Some(("⚠", WARN)), "node numbers move", TEXT));
        assert_eq!(styled("Failed: the card is busy"), (Some(("✖", BAD)), "the card is busy", TEXT));
        assert_eq!(styled("Known about this card (capture-cards/ms2130.md):"), (None, "Known about this card (capture-cards/ms2130.md):", TITLE));
        assert_eq!(styled("USB id  : 1e4e:7016 · MacroSilicon MS2130"), (None, "USB id  : 1e4e:7016 · MacroSilicon MS2130", TEXT));
        assert_eq!(styled("  • a bullet that ends:"), (None, "  • a bullet that ends:", TEXT));
        assert_eq!(styled("Not OK: kept"), (None, "Not OK: kept", TEXT));
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn osd_drawn_offscreen() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("a GPU");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let panel = overlay::panel_lines(&overlay::Panel {
            display: "Fullscreen",
            levels: "auto",
            scaling: "balanced",
            colour: [0, 0, 0, 0],
            audio: overlay::Audio::Running {
                level: Some(Ok(crate::level::Level { volume: 80, mute: false })),
                delay_ms: 50,
            },
            tips_name: "PlayStation 5".into(),
            changes: &["LEVELS=full".into()],
        });
        let ps5 = crate::sources::get("ps5").unwrap();
        let tips = overlay::tips_lines("ps5", Some(&ps5));
        let bar = overlay::key_bar(true);
        let wizard: Vec<String> = [
            "protoHDMIcapture setup · 1/12 · Capture card",
            "",
            "OK: pinned by USB serial 20000130041415: found wherever it enumerates.",
            "Warning: this card was on the ignored list; it is taken off.",
            "",
            "USB id  : 1e4e:7016 · MacroSilicon MS2130",
            "Known about this card (capture-cards/ms2130.md):",
            "  • Captures uncompressed YUYV 1080p60 only on a 5000 Mbit/s link.",
            "",
            "Failed: link 480 Mbit/s, a USB 2 port or hub",
            "",
            "▸  1  UVC Camera (1e4e:7016)  /dev/video0 · serial 20000130041415  ◂ current  ◂ capture chip",
            "   2  OBSBOT Meet 2: OBSBOT Meet 2 St  /dev/video2 · port pci-0000:12:00.0-usb-0:3.4.4:1.0  ◂ ignored",
        ]
        .map(String::from)
        .to_vec();
        let full = (1920u32, 1080u32);
        // A tiled window: a page sized for its width, long lines wrapped.
        let tiled = (960u32, 1123u32); // rows of 256-byte multiples for the copy
        for (name, (w, h), scene) in [
            ("wizard", full, Scene { page: Some(&wizard), ..Default::default() }),
            ("wizard-tiled", tiled, Scene { page: Some(&wizard), ..Default::default() }),
            ("panel", full, Scene { page: Some(&panel), message: Some("Volume 80%"), ..Default::default() }),
            ("tips", full, Scene { page: Some(&tips), ..Default::default() }),
            ("tips-tiled", tiled, Scene { page: Some(&tips), ..Default::default() }),
            (
                "bar-nosignal",
                full,
                Scene {
                    key_bar: Some(&bar),
                    no_signal: Some("The source stopped sending.  Press the PS button on the controller to wake it."),
                    ..Default::default()
                },
            ),
        ] {
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            // A grey ramp to draw over.
            let bg: Vec<u8> = (0..w * h).flat_map(|i| {
                let v = ((i % w) * 200 / w + 30) as u8;
                [v, v, v, 255]
            }).collect();
            queue.write_texture(
                target.as_image_copy(),
                &bg,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            let view = target.create_view(&Default::default());
            let mut osd = Osd::new(&device, format);
            let mut encoder = device.create_command_encoder(&Default::default());
            let extra = osd.draw(&device, &queue, &mut encoder, &view, (w, h), &scene);
            let out = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(w * 4 * h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &out,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            queue.submit(extra.into_iter().chain([encoder.finish()]));
            out.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let bgra = out.get_mapped_range(..).unwrap().to_vec();
            let rgb: Vec<u8> = bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
            // Something was drawn: pixels differ from the ramp.
            assert!(rgb.chunks(3).zip(bg.chunks(4)).any(|(a, b)| a[0] != b[0]), "{name}: nothing drawn");
            let path = std::env::temp_dir().join(format!("phc-osd-{name}.png"));
            let mut e = png::Encoder::new(std::fs::File::create(&path).unwrap(), w, h);
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&rgb).unwrap();
            eprintln!("{}", path.display());
        }
    }
}
