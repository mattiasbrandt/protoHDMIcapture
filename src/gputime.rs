//! GPU time of the two passes (--stats): timestamp queries at each pass's
//! start and end, read back without waiting. While one readback is still
//! on its way, frames go unmeasured rather than stall the render path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

const PASSES: u32 = 2;
const QUERIES: u32 = PASSES * 2;
const BYTES: u64 = QUERIES as u64 * 8;

/// The readback's state, shared with the map callback.
const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

pub struct GpuTimer {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    state: Arc<AtomicU8>,
    /// Nanoseconds per timestamp tick.
    period: f64,
    /// This frame's passes write timestamps.
    armed: bool,
}

impl GpuTimer {
    /// None when the adapter cannot time passes.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let buffer = |label, usage| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: BYTES, usage, mapped_at_creation: false })
        };
        Some(Self {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("pass times"),
                ty: wgpu::QueryType::Timestamp,
                count: QUERIES,
            }),
            resolve: buffer("pass times", wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC),
            readback: buffer("pass times readback", wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
            state: Arc::new(AtomicU8::new(IDLE)),
            period: f64::from(queue.get_timestamp_period()),
            armed: false,
        })
    }

    /// Call before encoding a frame: whether this one is measured.
    pub fn arm(&mut self) {
        self.armed = self.state.load(Ordering::Acquire) == IDLE;
    }

    /// The timestamp writes for pass `i` (0 convert, 1 scale), if armed.
    pub fn writes(&self, i: u32) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        self.armed.then_some(wgpu::RenderPassTimestampWrites {
            query_set: &self.queries,
            beginning_of_pass_write_index: Some(i * 2),
            end_of_pass_write_index: Some(i * 2 + 1),
        })
    }

    /// Pass `i` timed over two render passes: the start on the first,
    /// the end on the second (a filter's two passes, scale.rs).
    pub fn writes_split(&self, i: u32) -> [Option<wgpu::RenderPassTimestampWrites<'_>>; 2] {
        let one = |begin, end| {
            self.armed.then_some(wgpu::RenderPassTimestampWrites {
                query_set: &self.queries,
                beginning_of_pass_write_index: begin,
                end_of_pass_write_index: end,
            })
        };
        [one(Some(i * 2), None), one(None, Some(i * 2 + 1))]
    }

    /// After the passes, in the same encoder.
    pub fn resolve(&self, encoder: &mut wgpu::CommandEncoder) {
        if self.armed {
            encoder.resolve_query_set(&self.queries, 0..QUERIES, &self.resolve, 0);
            encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, BYTES);
        }
    }

    /// After the submit: start reading the timestamps back.
    pub fn submitted(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        self.state.store(MAPPING, Ordering::Release);
        let state = self.state.clone();
        self.readback.map_async(wgpu::MapMode::Read, .., move |r| {
            state.store(if r.is_ok() { MAPPED } else { FAILED }, Ordering::Release);
        });
    }

    /// The convert and scale passes' times, once a readback has arrived.
    /// The device must have been polled for the callback to run.
    pub fn take(&mut self) -> Result<Option<[Duration; 2]>, String> {
        match self.state.load(Ordering::Acquire) {
            MAPPED => {}
            FAILED => return Err("reading the GPU pass times back failed".into()),
            _ => return Ok(None),
        }
        let t: Vec<u64> = {
            let view = self.readback.get_mapped_range(..).map_err(|e| format!("pass times: {e}"))?;
            view.as_chunks::<8>().0.iter().map(|c| u64::from_ne_bytes(*c)).collect()
        };
        self.readback.unmap();
        self.state.store(IDLE, Ordering::Release);
        let span = |i: usize| Duration::from_nanos((t[i * 2 + 1].saturating_sub(t[i * 2]) as f64 * self.period) as u64);
        Ok(Some([span(0), span(1)]))
    }
}
