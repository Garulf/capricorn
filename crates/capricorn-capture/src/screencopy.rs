use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use image::RgbaImage;
use memmap2::MmapMut;
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::wl;

const COMPOSITOR_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_NAME_VERSION: u32 = 4;

/// Byte order of one pixel in memory. wl_shm formats are little-endian words, so
/// `Xrgb8888` is laid out as B, G, R, X.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOrder {
    Bgrx,
    Rgbx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmLayout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub order: ChannelOrder,
}

pub fn channel_order(format: wl_shm::Format) -> Option<ChannelOrder> {
    match format {
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => Some(ChannelOrder::Bgrx),
        wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => Some(ChannelOrder::Rgbx),
        _ => None,
    }
}

/// Converts a shm frame to upright, opaque RGBA.
pub fn to_rgba(data: &[u8], layout: ShmLayout, y_invert: bool) -> anyhow::Result<RgbaImage> {
    let (width, height) = (layout.width as usize, layout.height as usize);
    let stride = layout.stride as usize;
    let row_bytes = width * 4;
    if stride < row_bytes {
        bail!("stride {stride} is shorter than a {width}-pixel row");
    }
    let needed = stride * height.saturating_sub(1) + if height > 0 { row_bytes } else { 0 };
    if data.len() < needed {
        bail!(
            "shm buffer holds {} bytes, frame needs {needed}",
            data.len()
        );
    }
    let mut rgba = Vec::with_capacity(row_bytes * height);
    for y in 0..height {
        let source_row = if y_invert { height - 1 - y } else { y };
        let start = source_row * stride;
        for &[c0, c1, c2, _] in data[start..start + row_bytes].as_chunks::<4>().0 {
            let [r, g, b] = match layout.order {
                ChannelOrder::Bgrx => [c2, c1, c0],
                ChannelOrder::Rgbx => [c0, c1, c2],
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    RgbaImage::from_raw(layout.width, layout.height, rgba).context("frame size overflow")
}

/// Grabs one frame of the output whose `wl_output` name (v4) is `output_name`.
pub fn capture_output(conn: &Connection, output_name: &str) -> anyhow::Result<RgbaImage> {
    let (globals, mut queue) = registry_queue_init::<Grab>(conn)?;
    let qh = queue.handle();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm")?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .context("zwlr_screencopy_manager_v1 (is screencopy permitted for this binary?)")?;
    let outputs: Vec<WlOutput> = wl::bind_all(&globals, &qh, OUTPUT_NAME_VERSION);
    let mut grab = Grab::default();
    queue.roundtrip(&mut grab)?;

    let result = grab_output(&mut queue, &mut grab, &shm, &manager, &outputs, output_name);

    for output in &outputs {
        if output.version() >= 3 {
            output.release();
        }
    }
    manager.destroy();
    queue.roundtrip(&mut grab)?;
    result
}

fn grab_output(
    queue: &mut EventQueue<Grab>,
    grab: &mut Grab,
    shm: &WlShm,
    manager: &ZwlrScreencopyManagerV1,
    outputs: &[WlOutput],
    output_name: &str,
) -> anyhow::Result<RgbaImage> {
    let output = outputs
        .iter()
        .find(|o| {
            grab.output_names
                .get(&o.id())
                .is_some_and(|n| n == output_name)
        })
        .ok_or_else(|| anyhow!("no wl_output named {output_name} (needs wl_output v4)"))?;
    let qh = queue.handle();
    let frame = manager.capture_output(0, output, &qh, ());
    let result = copy_frame(queue, grab, shm, &frame);
    frame.destroy();
    result
}

fn copy_frame(
    queue: &mut EventQueue<Grab>,
    grab: &mut Grab,
    shm: &WlShm,
    frame: &ZwlrScreencopyFrameV1,
) -> anyhow::Result<RgbaImage> {
    let announces_buffer_done = frame.version() >= 3;
    wl::dispatch_until(queue, grab, COMPOSITOR_TIMEOUT, |g| {
        g.failed || g.buffer_done || (!announces_buffer_done && g.buffer_announced)
    })?;
    if grab.failed {
        bail!("compositor refused the screencopy frame");
    }
    let (format, layout) = grab
        .offer
        .ok_or_else(|| anyhow!("compositor offered no XRGB/ARGB/XBGR/ABGR 8888 shm buffer"))?;

    let qh = queue.handle();
    let mut buffer = ShmBuffer::new(shm, &qh, format, layout)?;
    frame.copy(&buffer.buffer);
    wl::dispatch_until(queue, grab, COMPOSITOR_TIMEOUT, |g| g.ready || g.failed)?;
    let result = if grab.ready {
        to_rgba(&buffer.map, layout, grab.y_invert)
    } else {
        Err(anyhow!("screencopy copy failed"))
    };
    buffer.map.fill(0);
    buffer.destroy();
    result
}

struct ShmBuffer {
    map: MmapMut,
    pool: WlShmPool,
    buffer: WlBuffer,
}

impl ShmBuffer {
    /// Backed by a memfd, so pixels live in RAM only and never touch a filesystem.
    fn new(
        shm: &WlShm,
        qh: &QueueHandle<Grab>,
        format: wl_shm::Format,
        layout: ShmLayout,
    ) -> anyhow::Result<Self> {
        let size = u64::from(layout.stride) * u64::from(layout.height);
        let pool_size = i32::try_from(size).context("frame too large for a shm pool")?;
        let fd = memfd_create("capricorn-frame", MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, size)?;
        let file = File::from(fd);
        // SAFETY: the memfd is private to this process and the compositor, and only
        // the compositor writes to it, before `ready`, while we do not read.
        let map = unsafe { MmapMut::map_mut(&file)? };
        let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
        let buffer = pool.create_buffer(
            0,
            layout.width as i32,
            layout.height as i32,
            layout.stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self { map, pool, buffer })
    }

    fn destroy(self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[derive(Default)]
struct Grab {
    output_names: HashMap<ObjectId, String>,
    offer: Option<(wl_shm::Format, ShmLayout)>,
    buffer_announced: bool,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

impl Grab {
    fn on_buffer_offer(
        &mut self,
        format: WEnum<wl_shm::Format>,
        width: u32,
        height: u32,
        stride: u32,
    ) {
        self.buffer_announced = true;
        let WEnum::Value(format) = format else {
            return;
        };
        let Some(order) = channel_order(format) else {
            return;
        };
        self.offer.get_or_insert((
            format,
            ShmLayout {
                width,
                height,
                stride,
                order,
            },
        ));
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Grab {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for Grab {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_names.insert(output.id(), name);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Grab {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => state.on_buffer_offer(format, width, height, stride),
            Event::BufferDone => state.buffer_done = true,
            Event::Flags { flags } => {
                state.y_invert = matches!(flags, WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            Event::Ready { .. } => state.ready = true,
            Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(Grab: ignore WlShm);
delegate_noop!(Grab: ignore WlBuffer);
delegate_noop!(Grab: WlShmPool);
delegate_noop!(Grab: ZwlrScreencopyManagerV1);

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn layout(width: u32, height: u32, stride: u32, order: ChannelOrder) -> ShmLayout {
        ShmLayout {
            width,
            height,
            stride,
            order,
        }
    }

    #[test]
    fn supported_formats_map_to_channel_orders() {
        assert_eq!(
            channel_order(wl_shm::Format::Xrgb8888),
            Some(ChannelOrder::Bgrx)
        );
        assert_eq!(
            channel_order(wl_shm::Format::Argb8888),
            Some(ChannelOrder::Bgrx)
        );
        assert_eq!(
            channel_order(wl_shm::Format::Xbgr8888),
            Some(ChannelOrder::Rgbx)
        );
        assert_eq!(
            channel_order(wl_shm::Format::Abgr8888),
            Some(ChannelOrder::Rgbx)
        );
        assert_eq!(channel_order(wl_shm::Format::Rgb565), None);
    }

    #[test]
    fn bgrx_with_stride_padding_becomes_opaque_rgba() {
        let data = [
            3, 2, 1, 0, 30, 20, 10, 0, 0xEE, 0xEE, 0xEE, 0xEE, //
            6, 5, 4, 9, 60, 50, 40, 9, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        let img = to_rgba(&data, layout(2, 2, 12, ChannelOrder::Bgrx), false).unwrap();
        assert_eq!(*img.get_pixel(0, 0), Rgba([1, 2, 3, 255]));
        assert_eq!(*img.get_pixel(1, 0), Rgba([10, 20, 30, 255]));
        assert_eq!(*img.get_pixel(0, 1), Rgba([4, 5, 6, 255]));
        assert_eq!(*img.get_pixel(1, 1), Rgba([40, 50, 60, 255]));
    }

    #[test]
    fn rgbx_is_copied_in_order() {
        let data = [1, 2, 3, 0, 4, 5, 6, 0];
        let img = to_rgba(&data, layout(2, 1, 8, ChannelOrder::Rgbx), false).unwrap();
        assert_eq!(*img.get_pixel(0, 0), Rgba([1, 2, 3, 255]));
        assert_eq!(*img.get_pixel(1, 0), Rgba([4, 5, 6, 255]));
    }

    #[test]
    fn y_invert_flips_rows() {
        let data = [1, 1, 1, 0, 2, 2, 2, 0];
        let img = to_rgba(&data, layout(1, 2, 4, ChannelOrder::Rgbx), true).unwrap();
        assert_eq!(*img.get_pixel(0, 0), Rgba([2, 2, 2, 255]));
        assert_eq!(*img.get_pixel(0, 1), Rgba([1, 1, 1, 255]));
    }

    #[test]
    fn short_buffer_or_narrow_stride_is_error() {
        assert!(to_rgba(&[0; 7], layout(2, 1, 8, ChannelOrder::Rgbx), false).is_err());
        assert!(to_rgba(&[0; 8], layout(2, 1, 4, ChannelOrder::Rgbx), false).is_err());
    }
}
