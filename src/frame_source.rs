use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use image::ImageFormat;

#[derive(Clone, Debug)]
pub enum VideoSourceConfig {
    LumixUdp { port: u16 },
    V4l2 { device: String },
}

impl VideoSourceConfig {
    pub fn description(&self) -> String {
        match self {
            VideoSourceConfig::LumixUdp { port } => format!("Lumix UDP :{port}"),
            VideoSourceConfig::V4l2 { device } => format!("V4L2 {device}"),
        }
    }

    pub fn uses_lumix_stream(&self) -> bool {
        matches!(self, VideoSourceConfig::LumixUdp { .. })
    }
}

#[derive(Clone, Debug)]
pub struct FrameImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct RunningFrameSource {
    receiver: Receiver<FrameImage>,
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
            VideoSourceConfig::V4l2 { device } => {
                bail!(
                    "video source `v4l2` is not implemented yet for `{device}`; the app is structured to add it next"
                );
            }
        };

        Ok(Self {
            receiver: rx,
            stop,
            join: Some(join),
        })
    }

    pub fn receiver(&self) -> &Receiver<FrameImage> {
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

fn lumix_udp_loop(port: u16, stop: Arc<AtomicBool>, tx: Sender<FrameImage>) {
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

        match decode_jpeg(jpeg) {
            Ok(frame) => {
                let _ = tx.try_send(frame);
            }
            Err(err) => eprintln!("[stream] JPEG decode failed: {err}"),
        }
    }
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
