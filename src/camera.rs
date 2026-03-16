use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeOption {
    pub value: &'static str,
    pub label: &'static str,
}

pub const FOCUS_MODES: [ModeOption; 3] = [
    ModeOption {
        value: "afs",
        label: "AFS",
    },
    ModeOption {
        value: "afc",
        label: "AFC",
    },
    ModeOption {
        value: "mf",
        label: "MF",
    },
];

pub const AF_AREA_MODES: [ModeOption; 6] = [
    ModeOption {
        value: "aftracking",
        label: "Tracking",
    },
    ModeOption {
        value: "1area",
        label: "1-Area",
    },
    ModeOption {
        value: "pinpoint",
        label: "Pinpoint",
    },
    ModeOption {
        value: "facedetection",
        label: "Face Detect",
    },
    ModeOption {
        value: "23area",
        label: "23-Area",
    },
    ModeOption {
        value: "49area",
        label: "49-Area",
    },
];

#[derive(Clone, Debug)]
pub struct CameraClient {
    ip: String,
}

impl CameraClient {
    pub fn new(ip: impl Into<String>) -> Self {
        Self { ip: ip.into() }
    }

    pub fn initialize(&self) -> Result<()> {
        self.request(
            "mode=accctrl&type=req_acc&value=4D454930-0100-1000-8000-AABBCCDDEEFF&value2=LumixEgui",
        )?;
        thread::sleep(Duration::from_millis(300));

        let recmode = self.request("mode=camcmd&value=recmode")?;
        ensure_ok("recmode", &recmode)?;
        thread::sleep(Duration::from_millis(300));

        let afmode = self.request("mode=setsetting&type=afmode&value=aftracking")?;
        ensure_ok("afmode", &afmode)?;
        Ok(())
    }

    pub fn start_stream(&self, udp_port: u16) -> Result<()> {
        self.request(&format!("mode=startstream&value={udp_port}"))?;
        Ok(())
    }

    pub fn stop_stream(&self) {
        let _ = self.request("mode=stopstream");
    }

    pub fn capture(&self) -> Result<()> {
        let body = self.request("mode=camcmd&value=capture")?;
        ensure_ok("capture", &body)
    }

    pub fn set_focus_mode(&self, mode: ModeOption) -> Result<()> {
        let body = self.request(&format!(
            "mode=setsetting&type=focusmode&value={}",
            mode.value
        ))?;
        ensure_ok("focusmode", &body)
    }

    pub fn set_af_area_mode(&self, mode: ModeOption) -> Result<()> {
        let body = self.request(&format!("mode=setsetting&type=afmode&value={}", mode.value))?;
        ensure_ok("afmode", &body)
    }

    pub fn one_shot_af(&self) -> Result<()> {
        let body = self.request("mode=camcmd&value=oneshot_af")?;
        ensure_ok("oneshot_af", &body)
    }

    pub fn touch_focus(&self, x: u16, y: u16) -> Result<()> {
        let _ = self.request("mode=camcmd&value=autoreviewunlock");
        let body = self.request(&format!("mode=camctrl&type=touch&value={x}/{y}&value2=on"))?;
        ensure_ok("touch", &body)
    }

    pub fn touch_focus_and_capture(&self, x: u16, y: u16) -> Result<()> {
        let _ = self.request("mode=camcmd&value=autoreviewunlock");
        let focus = self.request(&format!("mode=camctrl&type=touch&value={x}/{y}&value2=on"))?;
        ensure_ok("touch", &focus)?;
        thread::sleep(Duration::from_millis(300));
        self.capture()
    }

    pub fn get_state(&self) -> Result<()> {
        self.request("mode=getstate").map(|_| ())
    }

    fn request(&self, params: &str) -> Result<String> {
        let url = format!("http://{}/cam.cgi?{params}", self.ip);
        let response = ureq::get(&url)
            .timeout(Duration::from_secs(3))
            .call()
            .map_err(|err| anyhow!("request failed for `{params}`: {err}"))?;
        let mut body = String::new();
        response
            .into_reader()
            .read_to_string(&mut body)
            .with_context(|| format!("failed to read response body for `{params}`"))?;
        Ok(body)
    }
}

fn ensure_ok(op: &str, body: &str) -> Result<()> {
    if body.to_ascii_lowercase().contains("ok") {
        Ok(())
    } else {
        Err(anyhow!("{op} failed: {}", body.trim()))
    }
}

#[derive(Debug)]
pub enum CameraCommand {
    Capture,
    SetFocusMode(ModeOption),
    SetAfAreaMode(ModeOption),
    OneShotAf,
    TouchFocus { x: u16, y: u16 },
    TouchFocusAndCapture { x: u16, y: u16 },
}

impl fmt::Display for CameraCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CameraCommand::Capture => write!(f, "capture"),
            CameraCommand::SetFocusMode(mode) => write!(f, "focus mode {}", mode.label),
            CameraCommand::SetAfAreaMode(mode) => write!(f, "AF area {}", mode.label),
            CameraCommand::OneShotAf => write!(f, "one-shot AF"),
            CameraCommand::TouchFocus { x, y } => write!(f, "touch focus {x}/{y}"),
            CameraCommand::TouchFocusAndCapture { x, y } => {
                write!(f, "touch focus+capture {x}/{y}")
            }
        }
    }
}

pub struct KeepaliveHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl KeepaliveHandle {
    pub fn spawn(camera: Arc<CameraClient>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                if let Err(err) = camera.get_state() {
                    eprintln!("[keepalive] {err}");
                }
                for _ in 0..40 {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for KeepaliveHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
