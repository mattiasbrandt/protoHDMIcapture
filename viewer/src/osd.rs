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
        // Sized by the height, then smaller where the widest line would
        // run past a narrow window (a tiled one); monospace scales evenly.
        let widest = lines.iter().map(|l| text_width(p, l, &FontId::monospace(panel_fs))).fold(0.0, f32::max);
        let room = w - 5.0 * panel_fs;
        let fs = if widest > room && widest > 0.0 { (panel_fs * room / widest).floor().max(8.0) } else { panel_fs };
        let font = FontId::monospace(fs);
        let lh = (fs * 1.3).floor();
        let pad = fs;
        let widest = lines.iter().map(|l| text_width(p, l, &font)).fold(0.0, f32::max);
        let (x0, y0) = ((fs * 1.5).floor(), (fs * 3.5).floor());
        let size = Vec2::new((widest + 2.0 * pad).floor(), lines.len() as f32 * lh + 2.0 * pad);
        p.rect_filled(Rect::from_min_size(Pos2::new(x0, y0), size), 0.0, BOX);
        for (i, l) in lines.iter().enumerate() {
            let color = if i == 0 { TITLE } else { TEXT };
            outlined(p, Pos2::new(x0 + pad, y0 + pad + i as f32 * lh), Align2::LEFT_TOP, l, font.clone(), color);
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
    #[ignore = "needs a GPU"]
    fn osd_drawn_offscreen() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("a GPU");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let (w, h) = (1920u32, 1080u32);
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
        for (name, scene) in [
            ("panel", Scene { page: Some(&panel), message: Some("Volume 80%"), ..Default::default() }),
            ("tips", Scene { page: Some(&tips), ..Default::default() }),
            (
                "bar-nosignal",
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
