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
    pub copy_input: bool,
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

        let args = ffmpeg_args(config);
        let mut child = Command::new(ffmpeg)
            .args(args)
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

fn ffmpeg_args(config: &LoopbackConfig) -> Vec<String> {
    let mut args = vec![
        String::from("-loglevel"),
        String::from("error"),
        String::from("-f"),
        String::from("v4l2"),
        String::from("-input_format"),
        ffmpeg_input_format(&config.input_format).to_owned(),
        String::from("-video_size"),
        format!("{}x{}", config.width, config.height),
        String::from("-framerate"),
        config.fps.to_string(),
        String::from("-i"),
        config.input_device.clone(),
    ];

    if config.copy_input {
        push_copy_output(&mut args, &config.webcam_output);
        push_copy_output(&mut args, &config.preview_output);
    } else {
        push_raw_output(&mut args, &config.webcam_output, &config.output_format);
        push_raw_output(&mut args, &config.preview_output, &config.output_format);
    }

    args
}

fn ffmpeg_input_format(input_format: &str) -> &str {
    match input_format {
        "yuyv" => "yuyv422",
        "bgr3" => "bgr24",
        "yu12" => "yuv420p",
        other => other,
    }
}

fn push_copy_output(args: &mut Vec<String>, output: &str) {
    args.extend([
        String::from("-map"),
        String::from("0:v"),
        String::from("-c:v"),
        String::from("copy"),
        String::from("-f"),
        String::from("v4l2"),
        output.to_owned(),
    ]);
}

fn push_raw_output(args: &mut Vec<String>, output: &str, output_format: &str) {
    args.extend([
        String::from("-map"),
        String::from("0:v"),
        String::from("-c:v"),
        String::from("rawvideo"),
        String::from("-pix_fmt"),
        output_format.to_owned(),
        String::from("-f"),
        String::from("v4l2"),
        output.to_owned(),
    ]);
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

#[cfg(test)]
mod tests {
    use super::{LoopbackConfig, ffmpeg_args};

    fn test_config(copy_input: bool) -> LoopbackConfig {
        LoopbackConfig {
            input_device: String::from("/dev/video0"),
            webcam_output: String::from("/dev/video10"),
            preview_output: String::from("/dev/video11"),
            input_format: String::from("yuyv"),
            output_format: String::from("yuv420p"),
            copy_input,
            width: 1920,
            height: 1080,
            fps: 60,
        }
    }

    #[test]
    fn yuyv_copy_keeps_input_packets_unchanged() {
        let args = ffmpeg_args(&test_config(true));

        assert_eq!(
            args,
            [
                "-loglevel",
                "error",
                "-f",
                "v4l2",
                "-input_format",
                "yuyv422",
                "-video_size",
                "1920x1080",
                "-framerate",
                "60",
                "-i",
                "/dev/video0",
                "-map",
                "0:v",
                "-c:v",
                "copy",
                "-f",
                "v4l2",
                "/dev/video10",
                "-map",
                "0:v",
                "-c:v",
                "copy",
                "-f",
                "v4l2",
                "/dev/video11",
            ]
        );
    }

    #[test]
    fn converted_output_keeps_the_requested_pixel_format() {
        let args = ffmpeg_args(&test_config(false));

        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "rawvideo").count(),
            2
        );
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "yuv420p").count(),
            2
        );
        assert!(!args.iter().any(|arg| arg == "copy"));
    }
}
