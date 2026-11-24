//! Screenshot GPU waits happen on a worker; the owning thread receives mutable pixels.
use super::screenshot::{HdrScreenshot, PendingScreenshot};
use super::{
    Result, ScreenshotFailure, ScreenshotRequest, hdr10_to_rgb16, padded_row, square_downscale,
};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const READBACK_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Reader {
    pub device: wgpu::Device,
    pub config: wgpu::SurfaceConfiguration,
    pub submission: wgpu::SubmissionIndex,
}

fn wait_for_mapping(
    receiver: Receiver<std::result::Result<(), wgpu::BufferAsyncError>>,
    deadline: Instant,
) -> Result<()> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| format!("screenshot buffer mapping: {error}"))?
        .map_err(|error| error.to_string().into())
}
impl Reader {
    /// The pixels `copy_frame` read back into `buffer`, 4 bytes each, row after row.
    fn read_frame(&self, buffer: &wgpu::Buffer, deadline: Instant) -> Result<Vec<[u8; 4]>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer.map_async(wgpu::MapMode::Read, .., move |r| {
            let _ = sender.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(self.submission.clone()),
                timeout: Some(deadline.saturating_duration_since(Instant::now())),
            })
            .map_err(|error| format!("the screenshot frame didn't finish on the GPU: {error}"))?;
        wait_for_mapping(receiver, deadline)?;
        let (width, height, row) = (
            self.config.width as usize,
            self.config.height as usize,
            padded_row(self.config.width) as usize,
        );
        let mut pixels = Vec::with_capacity(width * height);
        {
            let data = buffer.get_mapped_range(..).map_err(|e| e.to_string())?;
            for y in 0..height {
                pixels.extend_from_slice(data[y * row..y * row + width * 4].as_chunks::<4>().0);
            }
        }
        buffer.unmap();
        Ok(pixels)
    }
    pub(super) fn read_screenshot(
        &self,
        request: &ScreenshotRequest,
        buffer: &wgpu::Buffer,
        hdr_buffer: Option<&wgpu::Buffer>,
    ) -> Result<PendingScreenshot> {
        let deadline = Instant::now() + READBACK_TIMEOUT;
        let bgra = matches!(
            self.config.format.remove_srgb_suffix(),
            wgpu::TextureFormat::Bgra8Unorm
        );
        let mut pixels = Vec::with_capacity((self.config.width * self.config.height * 4) as usize);
        for px in self.read_frame(buffer, deadline)? {
            let (r, b) = if bgra { (px[2], px[0]) } else { (px[0], px[2]) };
            pixels.extend_from_slice(&[r, px[1], b, 255]);
        }
        // NativeImage.resizeSubRectTo on takeAutoScreenshot's grab: the icon
        // is the frame's centre square averaged down to its size.
        let (width, height, pixels) = match request.size {
            Some(size) => (
                size[0],
                size[1],
                square_downscale(&pixels, self.config.width, self.config.height, size),
            ),
            None => (self.config.width, self.config.height, pixels),
        };
        let hdr = hdr_buffer.and_then(|buffer| match self.read_frame(buffer, deadline) {
            Ok(pixels) => Some(HdrScreenshot {
                size: [self.config.width, self.config.height],
                pixels: hdr10_to_rgb16(&pixels),
            }),
            Err(e) => {
                log::warn!("Horizon renderer: HDR screenshot readback: {e}");
                None
            }
        });
        Ok(PendingScreenshot {
            path: request.path.clone(),
            background: request.background,
            image: mod_api::ScreenshotImage::new(width, height, pixels),
            hdr,
        })
    }
}

type Outcome = std::result::Result<PendingScreenshot, String>;

#[derive(Default)]
pub(super) struct Readbacks(
    Vec<(PathBuf, Receiver<Outcome>)>,
    #[cfg(test)] Vec<std::thread::JoinHandle<()>>,
);
impl Readbacks {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn start(
        &mut self,
        reader: Reader,
        request: ScreenshotRequest,
        buffer: wgpu::Buffer,
        hdr: Option<wgpu::Buffer>,
    ) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        let path = request.path.clone();
        let worker = std::thread::Builder::new()
            .name("Screenshot readback".into())
            .spawn(move || {
                let result = reader
                    .read_screenshot(&request, &buffer, hdr.as_ref())
                    .map_err(|e| e.to_string());
                let _ = sender.send(result);
            })?;
        #[cfg(test)]
        self.1.push(worker);
        #[cfg(not(test))]
        drop(worker);
        self.0.push((path, receiver));
        Ok(())
    }

    pub fn poll(&mut self) -> Option<std::result::Result<PendingScreenshot, ScreenshotFailure>> {
        for (index, (_, receiver)) in self.0.iter().enumerate() {
            let outcome = match receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => Err("screenshot readback worker stopped".into()),
            };
            let (path, _) = self.0.remove(index);
            return Some(outcome.map_err(|message| ScreenshotFailure { path, message }));
        }
        None
    }
}

#[cfg(test)]
impl Drop for Readbacks {
    fn drop(&mut self) {
        self.0.clear();
        let mut panic = None;
        for worker in self.1.drain(..) {
            if let Err(error) = worker.join() {
                panic.get_or_insert(error);
            }
        }
        if !std::thread::panicking()
            && let Some(error) = panic
        {
            std::panic::resume_unwind(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_does_not_wait_for_a_missing_mapping_callback_after_the_deadline() {
        let (sender, receiver) = mpsc::channel();
        let error = wait_for_mapping(receiver, Instant::now()).unwrap_err();
        assert!(error.to_string().contains("screenshot buffer mapping"));
        drop(sender);

        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(())).unwrap();
        assert!(wait_for_mapping(receiver, Instant::now()).is_ok());
    }

    #[test]
    fn failed_and_disconnected_readbacks_keep_their_capture_paths() {
        let mut readbacks = Readbacks::default();
        let (icon_tx, icon_rx) = mpsc::channel();
        let (shot_tx, shot_rx) = mpsc::channel();
        readbacks.0.push(("icon.png".into(), icon_rx));
        readbacks.0.push(("screenshot.png".into(), shot_rx));
        shot_tx.send(Err("GPU readback failed".into())).unwrap();
        let Some(Err(shot)) = readbacks.poll() else {
            panic!("a completed capture must not wait for the earlier icon");
        };
        assert_eq!(shot.path, PathBuf::from("screenshot.png"));
        assert_eq!(shot.message, "GPU readback failed");
        assert!(readbacks.poll().is_none());

        drop(icon_tx);
        let Some(Err(icon)) = readbacks.poll() else {
            panic!("expected the disconnected icon readback");
        };
        assert_eq!(icon.path, PathBuf::from("icon.png"));
        assert_eq!(icon.message, "screenshot readback worker stopped");
        assert!(readbacks.is_empty());
    }

    #[test]
    #[ignore = "daily"]
    fn a_background_save_writes_the_png_off_the_render_thread() {
        let _gpu = crate::gfx::test_gpu::lock();
        let instance = crate::gfx::test_gpu::instance();
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else {
            return;
        };
        let Ok((device, queue)) = pollster::block_on(adapter.request_device(&Default::default()))
        else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("horizon-shot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        let [w, h] = [3u32, 2];
        // A frame of blue-first pixels, each row padded as `copy_frame` reads them back.
        let row = padded_row(w) as usize;
        let mut bytes = vec![0u8; row * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                bytes[y * row + x * 4..][..4].copy_from_slice(&[10 * x as u8, 20, 200, 0]);
            }
        }
        let source = wgpu::util::DeviceExt::create_buffer_init(
            &device,
            &wgpu::util::BufferInitDescriptor {
                label: None,
                contents: &bytes,
                usage: wgpu::BufferUsages::COPY_SRC,
            },
        );
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(&source, 0, &buffer, 0, bytes.len() as u64);
        let submission = queue.submit([encoder.finish()]);
        let request = ScreenshotRequest {
            path: path.clone(),
            size: None,
            scaled: false,
            background: true,
        };
        let mut readbacks = Readbacks::default();
        readbacks
            .start(
                Reader {
                    device,
                    submission,
                    config: wgpu::SurfaceConfiguration {
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        width: w,
                        height: h,
                        present_mode: wgpu::PresentMode::Immediate,
                        desired_maximum_frame_latency: 2,
                        alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                        view_formats: vec![],
                        color_space: wgpu::SurfaceColorSpace::default(),
                    },
                },
                request,
                buffer,
                None,
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pending = loop {
            if let Some(result) = readbacks.poll() {
                break result.unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "never read back");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(
            !path.exists(),
            "the owning-thread hook must run before any file is written"
        );
        let save = pending.save_background(None);
        let (saved_path, result) = loop {
            if let Some(outcome) = save.try_complete() {
                break outcome;
            }
            assert!(std::time::Instant::now() < deadline, "never written");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(saved_path, path);
        assert_eq!(result.map(|saved| saved.size), Ok([w, h]));
        let decoder =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut out).unwrap();
        // Red first, opaque: the last pixel of the first row was [20, 20, 200] blue-first.
        assert_eq!(&out[8..12], &[200, 20, 20, 255]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
