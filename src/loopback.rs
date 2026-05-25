use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use v4l::capability::Flags;
use v4l::prelude::Device;

#[derive(Clone, Debug)]
pub struct LoopbackConfig {
    pub input_device: String,
    pub webcam_output: String,
    pub preview_output: String,
    pub input_format: String,
    pub output_format: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub struct LoopbackBridge {
    child: Child,
}

impl LoopbackBridge {
    pub fn spawn(config: &LoopbackConfig) -> Result<Self> {
        check_output_device(&config.webcam_output)?;
        check_output_device(&config.preview_output)?;
        let ffmpeg = find_ffmpeg().context("failed to find ffmpeg in PATH")?;

        let video_size = format!("{}x{}", config.width, config.height);
        let mut child = Command::new(ffmpeg)
            .arg("-loglevel")
            .arg("error")
            .arg("-f")
            .arg("v4l2")
            .arg("-input_format")
            .arg(&config.input_format)
            .arg("-video_size")
            .arg(video_size)
            .arg("-framerate")
            .arg(config.fps.to_string())
            .arg("-i")
            .arg(&config.input_device)
            .arg("-map")
            .arg("0:v")
            .arg("-vcodec")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg(&config.output_format)
            .arg("-f")
            .arg("v4l2")
            .arg(&config.webcam_output)
            .arg("-map")
            .arg("0:v")
            .arg("-vcodec")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg(&config.output_format)
            .arg("-f")
            .arg("v4l2")
            .arg(&config.preview_output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start ffmpeg from `{}`", config.input_device))?;

        thread::sleep(Duration::from_millis(250));
        if let Some(status) = child
            .try_wait()
            .context("failed to check ffmpeg after startup")?
        {
            bail!("ffmpeg exited during startup with status {status}");
        }

        wait_for_capture_device(&config.preview_output, Duration::from_secs(5))?;

        Ok(Self { child })
    }
}

impl Drop for LoopbackBridge {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn check_output_device(device: &str) -> Result<()> {
    if Path::new(device).exists() {
        return Ok(());
    }

    bail!(
        "loopback device `{device}` does not exist\n\n{}",
        loopback_setup_text()
    )
}

fn wait_for_capture_device(device: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let dev = Device::with_path(device)
            .with_context(|| format!("failed to open loopback preview device `{device}`"))?;
        let caps = dev
            .query_caps()
            .with_context(|| format!("failed to query loopback preview device `{device}`"))?;
        if caps.capabilities.contains(Flags::VIDEO_CAPTURE) {
            return Ok(());
        }

        if start.elapsed() >= timeout {
            bail!(
                "loopback preview device `{device}` did not become a capture device after ffmpeg started"
            );
        }

        thread::sleep(Duration::from_millis(100));
    }
}

pub fn loopback_setup_text() -> &'static str {
    "Create the loopback devices at boot with these files:\n\n\
     /etc/modules-load.d/lumix-v4l2loopback.conf:\n\
     v4l2loopback\n\n\
     /etc/modprobe.d/lumix-v4l2loopback.conf:\n\
     options v4l2loopback video_nr=10,11 card_label=\"Lumix Webcam\",\"Lumix Preview\" exclusive_caps=1,1"
}

fn find_ffmpeg() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|entry| entry.join("ffmpeg"))
        .find(|candidate| candidate.is_file())
}
