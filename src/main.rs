mod camera;
mod frame_source;

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Sender, unbounded};
use eframe::egui::{
    self, Color32, ColorImage, Key, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};

use crate::camera::{AF_AREA_MODES, CameraClient, CameraCommand, FOCUS_MODES, KeepaliveHandle};
use crate::frame_source::{FrameImage, RunningFrameSource, VideoSourceConfig};

fn main() -> Result<()> {
    let config = AppConfig::parse()?;
    let camera = Arc::new(CameraClient::new(config.camera_ip.clone()));
    camera.initialize().context("failed to initialize camera")?;

    if let VideoSourceConfig::LumixUdp { port } = config.video_source {
        camera
            .start_stream(port)
            .with_context(|| format!("failed to start Lumix UDP stream on port {port}"))?;
    }

    let frame_source = RunningFrameSource::spawn(&config.video_source)
        .context("failed to start selected video source")?;
    let keepalive = KeepaliveHandle::spawn(Arc::clone(&camera));
    let command_worker = CameraCommandWorker::spawn(Arc::clone(&camera));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]),
        ..Default::default()
    };

    let title = format!("Lumix Control ({})", config.video_source.description());
    let app = LumixApp::new(config, camera, frame_source, keepalive, command_worker);
    eframe::run_native(&title, options, Box::new(move |_| Ok(Box::new(app))))
        .map_err(|err| anyhow!("failed to launch egui window: {err}"))?;
    Ok(())
}

#[derive(Clone, Debug)]
struct AppConfig {
    camera_ip: String,
    video_source: VideoSourceConfig,
}

impl AppConfig {
    fn parse() -> Result<Self> {
        let mut camera_ip = String::from("192.168.54.1");
        let mut source_name = String::from("lumix-udp");
        let mut udp_port: u16 = 49_152;
        let mut video_device = String::from("/dev/video2");

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--camera-ip" => {
                    camera_ip = next_value(&mut args, "--camera-ip")?;
                }
                "--source" => {
                    source_name = next_value(&mut args, "--source")?;
                }
                "--udp-port" => {
                    let value = next_value(&mut args, "--udp-port")?;
                    udp_port = value
                        .parse()
                        .with_context(|| format!("invalid UDP port `{value}`"))?;
                }
                "--video-device" => {
                    video_device = next_value(&mut args, "--video-device")?;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown argument `{other}`"),
            }
        }

        let video_source = match source_name.as_str() {
            "lumix-udp" => VideoSourceConfig::LumixUdp { port: udp_port },
            "v4l2" => VideoSourceConfig::V4l2 {
                device: video_device,
            },
            other => bail!("unsupported source `{other}`"),
        };

        Ok(Self {
            camera_ip,
            video_source,
        })
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("missing value for `{flag}`"))
}

fn print_help() {
    println!("Usage: cargo run -- [options]");
    println!();
    println!("Options:");
    println!("  --camera-ip <ip>        Camera IP address (default: 192.168.54.1)");
    println!("  --source <name>         `lumix-udp` or `v4l2` (default: lumix-udp)");
    println!("  --udp-port <port>       Lumix UDP stream port (default: 49152)");
    println!("  --video-device <path>   V4L2 device path (default: /dev/video2)");
}

struct LumixApp {
    config: AppConfig,
    camera: Arc<CameraClient>,
    frame_source: RunningFrameSource,
    keepalive: KeepaliveHandle,
    command_worker: CameraCommandWorker,
    texture: Option<TextureHandle>,
    focus_mode_idx: usize,
    af_area_idx: usize,
    focus_uv: Option<[f32; 2]>,
    status_line: String,
}

impl LumixApp {
    fn new(
        config: AppConfig,
        camera: Arc<CameraClient>,
        frame_source: RunningFrameSource,
        keepalive: KeepaliveHandle,
        command_worker: CameraCommandWorker,
    ) -> Self {
        let status_line = format!(
            "{} | {} | source: {}",
            FOCUS_MODES[0].label,
            AF_AREA_MODES[0].label,
            config.video_source.description()
        );

        Self {
            config,
            camera,
            frame_source,
            keepalive,
            command_worker,
            texture: None,
            focus_mode_idx: 0,
            af_area_idx: 0,
            focus_uv: None,
            status_line,
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        let mut newest: Option<FrameImage> = None;
        while let Ok(frame) = self.frame_source.receiver().try_recv() {
            newest = Some(frame);
        }

        let Some(frame) = newest else {
            return;
        };

        let image = ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture("lumix-frame", image, TextureOptions::LINEAR));
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.key_pressed(Key::Q) || input.key_pressed(Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if ctx.input(|input| input.key_pressed(Key::C)) {
            self.send_command(CameraCommand::Capture);
        }

        if ctx.input(|input| input.key_pressed(Key::F)) {
            self.focus_mode_idx = (self.focus_mode_idx + 1) % FOCUS_MODES.len();
            let mode = FOCUS_MODES[self.focus_mode_idx];
            self.send_command(CameraCommand::SetFocusMode(mode));
            self.refresh_status();
        }

        if ctx.input(|input| input.key_pressed(Key::A)) {
            self.af_area_idx = (self.af_area_idx + 1) % AF_AREA_MODES.len();
            let mode = AF_AREA_MODES[self.af_area_idx];
            self.send_command(CameraCommand::SetAfAreaMode(mode));
            self.refresh_status();
        }

        if ctx.input(|input| input.key_pressed(Key::O)) {
            self.send_command(CameraCommand::OneShotAf);
        }
    }

    fn send_command(&mut self, command: CameraCommand) {
        if let Err(err) = self.command_worker.sender.send(command) {
            self.status_line = format!("camera worker unavailable: {err}");
        }
    }

    fn refresh_status(&mut self) {
        self.status_line = format!(
            "{} | {} | source: {}",
            FOCUS_MODES[self.focus_mode_idx].label,
            AF_AREA_MODES[self.af_area_idx].label,
            self.config.video_source.description()
        );
    }

    fn draw_video(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let (response, painter) = ui.allocate_painter(available, Sense::click());

        if let Some(texture) = &self.texture {
            let texture_size = texture.size_vec2();
            let image_rect = fitted_rect(response.rect, texture_size);
            painter.image(
                texture.id(),
                image_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );

            if response.clicked_by(egui::PointerButton::Primary) {
                if let Some(pos) = response.interact_pointer_pos() {
                    self.focus_at(pos, image_rect, false);
                }
            }

            if response.clicked_by(egui::PointerButton::Secondary) {
                if let Some(pos) = response.interact_pointer_pos() {
                    self.focus_at(pos, image_rect, true);
                }
            }

            if let Some([u, v]) = self.focus_uv {
                let point = Pos2::new(
                    image_rect.left() + image_rect.width() * u,
                    image_rect.top() + image_rect.height() * v,
                );
                draw_crosshair(&painter, point);
            }

            draw_status_bar(&painter, image_rect, &self.status_line);
        } else {
            painter.rect_filled(response.rect, 0.0, Color32::from_rgb(16, 16, 16));
            painter.text(
                response.rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for video...",
                egui::FontId::proportional(28.0),
                Color32::WHITE,
            );
        }
    }

    fn focus_at(&mut self, pos: Pos2, image_rect: Rect, capture: bool) {
        if !image_rect.contains(pos) {
            return;
        }

        let u = ((pos.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
        let v = ((pos.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
        self.focus_uv = Some([u, v]);

        let x = (u * 1000.0).round().clamp(0.0, 1000.0) as u16;
        let y = (v * 1000.0).round().clamp(0.0, 1000.0) as u16;

        let command = if capture {
            CameraCommand::TouchFocusAndCapture { x, y }
        } else {
            CameraCommand::TouchFocus { x, y }
        };
        self.send_command(command);
    }
}

impl eframe::App for LumixApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.update_texture(ctx);
        self.handle_keys(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| self.draw_video(ui));

        ctx.request_repaint_after(Duration::from_millis(16));
        let _ = frame;
    }
}

impl Drop for LumixApp {
    fn drop(&mut self) {
        if self.config.video_source.uses_lumix_stream() {
            self.camera.stop_stream();
        }

        let _ = &self.keepalive;
        let _ = &self.frame_source;
        let _ = &self.command_worker;
    }
}

fn fitted_rect(bounds: Rect, image_size: Vec2) -> Rect {
    if image_size.x <= 0.0 || image_size.y <= 0.0 {
        return bounds;
    }

    let image_aspect = image_size.x / image_size.y;
    let bounds_aspect = bounds.width() / bounds.height().max(1.0);

    if bounds_aspect > image_aspect {
        let height = bounds.height();
        let width = height * image_aspect;
        let left = bounds.center().x - width / 2.0;
        Rect::from_min_size(Pos2::new(left, bounds.top()), Vec2::new(width, height))
    } else {
        let width = bounds.width();
        let height = width / image_aspect;
        let top = bounds.center().y - height / 2.0;
        Rect::from_min_size(Pos2::new(bounds.left(), top), Vec2::new(width, height))
    }
}

fn draw_crosshair(painter: &egui::Painter, point: Pos2) {
    let stroke = Stroke::new(1.5, Color32::from_rgb(0, 255, 0));
    let size = 20.0;
    painter.line_segment(
        [
            Pos2::new(point.x - size, point.y),
            Pos2::new(point.x + size, point.y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(point.x, point.y - size),
            Pos2::new(point.x, point.y + size),
        ],
        stroke,
    );
    painter.rect_stroke(
        Rect::from_center_size(point, Vec2::splat(size * 2.0)),
        0.0,
        stroke,
        egui::StrokeKind::Inside,
    );
}

fn draw_status_bar(painter: &egui::Painter, image_rect: Rect, text: &str) {
    let padding = Vec2::new(10.0, 8.0);
    let galley = painter.layout_no_wrap(
        text.to_owned(),
        egui::FontId::monospace(16.0),
        Color32::from_rgb(0, 255, 0),
    );
    let rect = Rect::from_min_size(
        Pos2::new(image_rect.left() + 12.0, image_rect.top() + 12.0),
        galley.size() + padding * 2.0,
    );
    painter.rect_filled(rect, 6.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180));
    painter.galley(
        Pos2::new(rect.left() + padding.x, rect.top() + padding.y),
        galley,
        Color32::from_rgb(0, 255, 0),
    );
}

struct CameraCommandWorker {
    sender: Sender<CameraCommand>,
    join: Option<JoinHandle<()>>,
}

impl CameraCommandWorker {
    fn spawn(camera: Arc<CameraClient>) -> Self {
        let (sender, receiver) = unbounded();
        let join = thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                if let Err(err) = dispatch_command(&camera, command) {
                    eprintln!("[camera] {err}");
                }
            }
        });

        Self {
            sender,
            join: Some(join),
        }
    }
}

impl Drop for CameraCommandWorker {
    fn drop(&mut self) {
        let (replacement, _unused) = unbounded();
        let sender = std::mem::replace(&mut self.sender, replacement);
        drop(sender);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn dispatch_command(camera: &CameraClient, command: CameraCommand) -> Result<()> {
    match command {
        CameraCommand::Capture => camera.capture(),
        CameraCommand::SetFocusMode(mode) => camera.set_focus_mode(mode),
        CameraCommand::SetAfAreaMode(mode) => camera.set_af_area_mode(mode),
        CameraCommand::OneShotAf => camera.one_shot_af(),
        CameraCommand::TouchFocus { x, y } => camera.touch_focus(x, y),
        CameraCommand::TouchFocusAndCapture { x, y } => camera.touch_focus_and_capture(x, y),
    }
}
