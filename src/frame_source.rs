use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use image::ImageFormat;
use v4l::buffer::Type;
use v4l::format::{Format, FourCC};
use v4l::io::traits::CaptureStream;
use v4l::prelude::{Device, MmapStream};
use v4l::video::Capture;

#[derive(Clone, Debug)]
pub enum VideoSourceConfig {
    LumixUdp { port: u16 },
    V4l2(V4l2Config),
}

#[derive(Clone, Debug)]
pub struct V4l2Config {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: V4l2PixelFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum V4l2PixelFormat {
    Mjpeg,
    Yuyv,
    Bgr3,
}

impl V4l2PixelFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "mjpeg" | "mjpg" => Ok(Self::Mjpeg),
            "yuyv" => Ok(Self::Yuyv),
            "bgr3" | "bgr24" => Ok(Self::Bgr3),
            other => bail!("unsupported V4L2 input format `{other}`"),
        }
    }

    fn fourcc(self) -> FourCC {
        match self {
            Self::Mjpeg => FourCC::new(b"MJPG"),
            Self::Yuyv => FourCC::new(b"YUYV"),
            Self::Bgr3 => FourCC::new(b"BGR3"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mjpeg => "mjpeg",
            Self::Yuyv => "yuyv",
            Self::Bgr3 => "bgr3",
        }
    }
}

impl VideoSourceConfig {
    pub fn description(&self) -> String {
        match self {
            VideoSourceConfig::LumixUdp { port } => format!("Lumix UDP :{port}"),
            VideoSourceConfig::V4l2(config) => format!(
                "V4L2 {} {} {}x{}@{}",
                config.device,
                config.pixel_format.label(),
                config.width,
                config.height,
                config.fps
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FrameImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum FramePacket {
    Jpeg(Vec<u8>),
    Bgr3 {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
    Yu12 {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
    Yuyv {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
}

pub struct RunningFrameSource {
    receiver: Receiver<FramePacket>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl RunningFrameSource {
    pub fn spawn(config: &VideoSourceConfig) -> Result<Self> {
        let (tx, rx) = bounded(4);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let join = match config {
            VideoSourceConfig::LumixUdp { port } => {
                let port = *port;
                thread::spawn(move || lumix_udp_loop(port, stop_flag, tx))
            }
            VideoSourceConfig::V4l2(config) => {
                let config = config.clone();
                thread::spawn(move || {
                    if let Err(err) = v4l2_loop(config, stop_flag, tx) {
                        eprintln!("[stream] {err:#}");
                    }
                })
            }
        };

        Ok(Self {
            receiver: rx,
            stop,
            join: Some(join),
        })
    }

    pub fn receiver(&self) -> &Receiver<FramePacket> {
        &self.receiver
    }
}

impl Drop for RunningFrameSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn lumix_udp_loop(port: u16, stop: Arc<AtomicBool>, tx: Sender<FramePacket>) {
    let socket = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("[stream] failed to bind UDP port {port}: {err}");
            return;
        }
    };

    if let Err(err) = socket.set_read_timeout(Some(Duration::from_secs(1))) {
        eprintln!("[stream] failed to set UDP timeout: {err}");
        return;
    }

    let mut packet = [0_u8; 65_536];
    while !stop.load(Ordering::Relaxed) {
        let len = match socket.recv(&mut packet) {
            Ok(len) => len,
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => {
                eprintln!("[stream] UDP receive failed: {err}");
                break;
            }
        };

        let Some(jpeg) = extract_jpeg(&packet[..len]) else {
            continue;
        };

        let _ = tx.try_send(FramePacket::Jpeg(jpeg.to_vec()));
    }
}

fn v4l2_loop(config: V4l2Config, stop: Arc<AtomicBool>, tx: Sender<FramePacket>) -> Result<()> {
    let dev = Device::with_path(&config.device)
        .with_context(|| format!("failed to open V4L2 device `{}`", config.device))?;

    let requested = Format::new(config.width, config.height, config.pixel_format.fourcc());
    let actual = dev
        .set_format(&requested)
        .with_context(|| format!("failed to set V4L2 format on `{}`", config.device))?;

    let params = v4l::video::capture::Parameters::with_fps(config.fps);
    let actual_params = dev
        .set_params(&params)
        .with_context(|| format!("failed to set frame rate on `{}`", config.device))?;

    eprintln!(
        "[stream] V4L2 using {} {}x{} @ {}/{}s",
        actual.fourcc.str()?,
        actual.width,
        actual.height,
        actual_params.interval.numerator,
        actual_params.interval.denominator,
    );

    let mut stream = MmapStream::with_buffers(&dev, Type::VideoCapture, 4)
        .with_context(|| format!("failed to create MMAP stream for `{}`", config.device))?;
    stream.set_timeout(Duration::from_secs(1));

    while !stop.load(Ordering::Relaxed) {
        let (buf, meta) = match stream.next() {
            Ok(frame) => frame,
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(err) => return Err(err).context("V4L2 frame capture failed"),
        };

        let bytes_used = usize::min(meta.bytesused as usize, buf.len());
        if bytes_used == 0 {
            continue;
        }

        let packet = packet_from_v4l2_frame(&actual, &buf[..bytes_used])?;
        let _ = tx.try_send(packet);
    }

    Ok(())
}

fn extract_jpeg(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 34 {
        return None;
    }

    let header_extra = u16::from_be_bytes([data[30], data[31]]) as usize;
    let offset = 32 + header_extra;
    let payload = data.get(offset..)?;
    payload.starts_with(&[0xff, 0xd8]).then_some(payload)
}

fn decode_jpeg(bytes: &[u8]) -> Result<FrameImage> {
    let image = image::load_from_memory_with_format(bytes, ImageFormat::Jpeg)?;
    let rgba = image.to_rgba8();
    Ok(FrameImage {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

fn packet_from_v4l2_frame(format: &Format, bytes: &[u8]) -> Result<FramePacket> {
    let fourcc = format
        .fourcc
        .str()
        .map_err(|err| anyhow!("invalid V4L2 fourcc: {err}"))?;
    let width = format.width as usize;
    let height = format.height as usize;

    match fourcc {
        "MJPG" => {
            validate_mjpeg(bytes)?;
            Ok(FramePacket::Jpeg(bytes.to_vec()))
        }
        "YUYV" => {
            let expected = width * height * 2;
            if bytes.len() < expected {
                bail!(
                    "short YUYV frame: expected at least {expected} bytes, got {}",
                    bytes.len()
                );
            }
            Ok(FramePacket::Yuyv {
                width,
                height,
                bytes: bytes[..expected].to_vec(),
            })
        }
        "BGR3" => {
            let expected = width * height * 3;
            if bytes.len() < expected {
                bail!(
                    "short BGR3 frame: expected at least {expected} bytes, got {}",
                    bytes.len()
                );
            }
            Ok(FramePacket::Bgr3 {
                width,
                height,
                bytes: bytes[..expected].to_vec(),
            })
        }
        "YU12" => {
            let expected = width * height * 3 / 2;
            if bytes.len() < expected {
                bail!(
                    "short YU12 frame: expected at least {expected} bytes, got {}",
                    bytes.len()
                );
            }
            Ok(FramePacket::Yu12 {
                width,
                height,
                bytes: bytes[..expected].to_vec(),
            })
        }
        other => bail!("unsupported V4L2 pixel format `{other}`"),
    }
}

fn decode_mjpeg(bytes: &[u8]) -> Result<FrameImage> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        bail!("buffer does not start with JPEG SOI marker");
    }

    if bytes.ends_with(&[0xff, 0xd9]) {
        return decode_jpeg(bytes);
    }

    let mut repaired = Vec::with_capacity(bytes.len() + 2);
    repaired.extend_from_slice(bytes);
    repaired.extend_from_slice(&[0xff, 0xd9]);
    decode_jpeg(&repaired)
}

pub fn decode_frame_packet(packet: FramePacket) -> Result<FrameImage> {
    match packet {
        FramePacket::Jpeg(bytes) => decode_mjpeg(&bytes),
        FramePacket::Bgr3 {
            width,
            height,
            bytes,
        } => decode_bgr3(&bytes, width, height),
        FramePacket::Yu12 {
            width,
            height,
            bytes,
        } => decode_yu12(&bytes, width, height),
        FramePacket::Yuyv {
            width,
            height,
            bytes,
        } => decode_yuyv(&bytes, width, height),
    }
}

fn decode_bgr3(bytes: &[u8], width: usize, height: usize) -> Result<FrameImage> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    for pixel in bytes.chunks_exact(3) {
        rgba.push(pixel[2]);
        rgba.push(pixel[1]);
        rgba.push(pixel[0]);
        rgba.push(255);
    }

    Ok(FrameImage {
        width,
        height,
        rgba,
    })
}

fn decode_yuyv(bytes: &[u8], width: usize, height: usize) -> Result<FrameImage> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    for chunk in bytes.chunks_exact(4) {
        let y0 = chunk[0] as f32;
        let u = chunk[1] as f32 - 128.0;
        let y1 = chunk[2] as f32;
        let v = chunk[3] as f32 - 128.0;

        push_yuv_pixel(&mut rgba, y0, u, v);
        push_yuv_pixel(&mut rgba, y1, u, v);
    }

    Ok(FrameImage {
        width,
        height,
        rgba,
    })
}

fn decode_yu12(bytes: &[u8], width: usize, height: usize) -> Result<FrameImage> {
    if width % 2 != 0 || height % 2 != 0 {
        bail!("YU12 requires even dimensions, got {width}x{height}");
    }

    let y_plane_len = width * height;
    let uv_plane_len = y_plane_len / 4;
    let expected = y_plane_len + uv_plane_len * 2;
    if bytes.len() < expected {
        bail!(
            "short YU12 frame: expected at least {expected} bytes, got {}",
            bytes.len()
        );
    }

    let y_plane = &bytes[..y_plane_len];
    let u_plane = &bytes[y_plane_len..y_plane_len + uv_plane_len];
    let v_plane = &bytes[y_plane_len + uv_plane_len..expected];

    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let uv_row = (y / 2) * (width / 2);
        for x in 0..width {
            let y_value = y_plane[y * width + x] as f32;
            let uv_index = uv_row + (x / 2);
            let u = u_plane[uv_index] as f32 - 128.0;
            let v = v_plane[uv_index] as f32 - 128.0;
            push_yuv_pixel(&mut rgba, y_value, u, v);
        }
    }

    Ok(FrameImage {
        width,
        height,
        rgba,
    })
}

fn push_yuv_pixel(rgba: &mut Vec<u8>, y: f32, u: f32, v: f32) {
    let r = (y + 1.402 * v).round().clamp(0.0, 255.0) as u8;
    let g = (y - 0.344_136 * u - 0.714_136 * v)
        .round()
        .clamp(0.0, 255.0) as u8;
    let b = (y + 1.772 * u).round().clamp(0.0, 255.0) as u8;

    rgba.push(r);
    rgba.push(g);
    rgba.push(b);
    rgba.push(255);
}

fn validate_mjpeg(bytes: &[u8]) -> Result<()> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        bail!("buffer does not start with JPEG SOI marker");
    }
    Ok(())
}
