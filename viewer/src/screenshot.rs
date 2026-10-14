//! Screenshots (s): the frame at the capture's own size, after LEVELS and
//! the colour values and before scaling, as a PNG in
//! `~/Pictures/protoHDMIcapture/<date>_<time>.png` (decided in #12). The
//! copy is read back without waiting and encoded on its own thread, so a
//! screenshot never holds up a frame.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

use crate::overlay::Notify;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

/// Rows of a texture copy are padded to this many bytes.
const ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

pub struct Screenshot {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    /// Bytes per padded row in `buffer`.
    row: u32,
    state: Arc<AtomicU8>,
    /// s was pressed: the next frame is copied.
    wanted: bool,
    /// This frame's encoder holds the copy.
    copying: bool,
}

impl Screenshot {
    /// For an RGBA8 frame of `width` x `height`.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let row = (width * 4).next_multiple_of(ROW_ALIGN);
        Self {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("screenshot"),
                size: u64::from(row) * u64::from(height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            width,
            height,
            row,
            state: Arc::new(AtomicU8::new(IDLE)),
            wanted: false,
            copying: false,
        }
    }

    /// s: the next frame is saved. False while the last one is still on
    /// its way back (the key is ignored, not queued).
    pub fn request(&mut self) -> bool {
        if self.wanted || self.state.load(Ordering::Acquire) != IDLE {
            return false;
        }
        self.wanted = true;
        true
    }

    /// After the convert pass, in the same encoder: copies `frame` if a
    /// screenshot was asked for.
    pub fn copy(&mut self, encoder: &mut wgpu::CommandEncoder, frame: &wgpu::Texture) {
        if !self.wanted {
            return;
        }
        self.wanted = false;
        self.copying = true;
        encoder.copy_texture_to_buffer(
            frame.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
    }

    /// After the submit: start reading the copy back.
    pub fn submitted(&mut self) {
        if !self.copying {
            return;
        }
        self.copying = false;
        self.state.store(MAPPING, Ordering::Release);
        let state = self.state.clone();
        self.buffer.map_async(wgpu::MapMode::Read, .., move |r| {
            state.store(if r.is_ok() { MAPPED } else { FAILED }, Ordering::Release);
        });
    }

    /// Once the copy is back (the device polled), hands it to a thread that
    /// writes the file and says where, on screen and on stderr.
    pub fn poll(&mut self, notify: &Notify) -> Result<(), String> {
        let Some(rgb) = self.take()? else { return Ok(()) };
        let (w, h) = (self.width, self.height);
        let notify = notify.clone();
        thread::spawn(move || {
            let message = match save(&rgb, w, h) {
                Ok(path) => {
                    eprintln!("saved {}", path.display());
                    (format!("Saved {}", path.display()), Duration::from_secs(3))
                }
                Err(e) => {
                    eprintln!("screenshot failed: {e}");
                    (format!("Screenshot failed: {e}"), Duration::from_secs(5))
                }
            };
            notify(Some(message));
        });
        Ok(())
    }

    /// The copy as packed RGB, once it is back.
    fn take(&mut self) -> Result<Option<Vec<u8>>, String> {
        match self.state.load(Ordering::Acquire) {
            MAPPED => {}
            FAILED => {
                self.state.store(IDLE, Ordering::Release);
                return Err("reading the screenshot back failed".into());
            }
            _ => return Ok(None),
        }
        let rgb = {
            let view = self.buffer.get_mapped_range(..).map_err(|e| format!("screenshot: {e}"))?;
            unpad_rgba(&view, self.width, self.height, self.row)
        };
        self.buffer.unmap();
        self.state.store(IDLE, Ordering::Release);
        Ok(Some(rgb))
    }
}

/// Padded RGBA rows -> packed RGB.
fn unpad_rgba(data: &[u8], width: u32, height: u32, row: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height as usize {
        let line = &data[y * row as usize..][..width as usize * 4];
        for px in line.as_chunks::<4>().0 {
            out.extend_from_slice(&px[..3]);
        }
    }
    out
}

/// `~/Pictures/protoHDMIcapture`.
fn folder() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Pictures/protoHDMIcapture"))
}

/// Local time as `2026-10-14_22-31-05`.
fn stamp() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // Safety: localtime_r fills `tm` from `now` and touches nothing else.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// `<stamp>.png` in `dir`, or `<stamp>-2.png` and on when that is taken
/// (two shots in one second).
fn free_name(dir: &Path, stamp: &str) -> PathBuf {
    let mut path = dir.join(format!("{stamp}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stamp}-{n}.png"));
        n += 1;
    }
    path
}

fn encode(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, width, height);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    let mut w = e.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgb).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Writes the PNG next to its final name and renames it, so a half-written
/// file never carries the name.
fn save_in(dir: &Path, stamp: &str, rgb: &[u8], width: u32, height: u32) -> Result<PathBuf, String> {
    let png = encode(rgb, width, height)?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = free_name(dir, stamp);
    let tmp = dir.join(format!(".{stamp}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&png)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })?;
    Ok(path)
}

fn save(rgb: &[u8], width: u32, height: u32) -> Result<PathBuf, String> {
    save_in(&folder()?, &stamp(), rgb, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_lose_their_padding_and_alpha() {
        // 2x2 RGBA, rows padded to 12 bytes.
        let data = [1, 2, 3, 9, 4, 5, 6, 9, 0, 0, 0, 0, 7, 8, 9, 9, 10, 11, 12, 9, 0, 0, 0, 0];
        assert_eq!(unpad_rgba(&data, 2, 2, 12), (1..=12).collect::<Vec<u8>>());
    }

    #[test]
    fn saves_a_png_that_reads_back_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("phc-shots-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (w, h) = (5u32, 3u32);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 7) as u8).collect();
        let first = save_in(&dir, "2026-10-14_22-31-05", &rgb, w, h).unwrap();
        let second = save_in(&dir, "2026-10-14_22-31-05", &rgb, w, h).unwrap();
        assert_eq!(first.file_name().unwrap(), "2026-10-14_22-31-05.png");
        assert_eq!(second.file_name().unwrap(), "2026-10-14_22-31-05-2.png");

        let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(&first).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height, info.color_type), (w, h, png::ColorType::Rgb));
        assert_eq!(&buf[..info.buffer_size()], &rgb[..]);
        // Nothing but the two shots: the temp files were renamed.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The copy and readback on the GPU, at a width whose rows need
    /// padding: `cargo test -- --ignored screenshot_on_the_gpu`.
    #[test]
    #[ignore = "needs a GPU"]
    fn screenshot_on_the_gpu() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("a GPU");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let (w, h) = (1366u32, 3u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        let frame = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            frame.as_image_copy(),
            &rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let mut shot = Screenshot::new(&device, w, h);
        assert!(shot.request());
        assert!(!shot.request(), "one at a time");
        let mut encoder = device.create_command_encoder(&Default::default());
        shot.copy(&mut encoder, &frame);
        queue.submit([encoder.finish()]);
        shot.submitted();
        assert!(!shot.request(), "not while it comes back");
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let rgb = shot.take().unwrap().expect("the copy is back");
        let want: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        assert_eq!(rgb, want);
        assert!(shot.request(), "free again");
    }

    #[test]
    fn the_stamp_is_date_and_time() {
        let s = stamp();
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "_");
    }
}
