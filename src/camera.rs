use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, unbounded};

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

#[derive(Clone, Debug, Default)]
pub struct CameraState {
    pub battery: Option<String>,
    pub battery_grip: Option<String>,
}

impl CameraState {
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();

        if let Some(battery) = self.battery.as_deref() {
            parts.push(format!("battery {battery}"));
        }

        if let Some(grip) = self.battery_grip.as_deref() {
            if grip != "-1/0" {
                parts.push(format!("grip {grip}"));
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" | "))
        }
    }
}

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

    pub fn get_state(&self) -> Result<CameraState> {
        let body = self.request("mode=getstate")?;
        Ok(CameraState {
            battery: extract_tag(&body, "batt"),
            battery_grip: extract_tag(&body, "batt_grip"),
        })
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

fn extract_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let (_, remainder) = body.split_once(&open)?;
    let (value, _) = remainder.split_once(&close)?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
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
    receiver: Receiver<CameraState>,
}

impl KeepaliveHandle {
    pub fn spawn(camera: Arc<CameraClient>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let (sender, receiver) = unbounded();
        let join = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match camera.get_state() {
                    Ok(state) => {
                        let _ = sender.send(state);
                    }
                    Err(err) => eprintln!("[keepalive] {err}"),
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
            receiver,
        }
    }

    pub fn receiver(&self) -> &Receiver<CameraState> {
        &self.receiver
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
