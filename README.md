# LumixCam

Use a Panasonic Lumix camera as a webcam on Linux, with live preview and remote
camera control.

## Why

Panasonic's official Lumix Tether and Lumix Webcam Software are Windows/macOS
only. On Linux there is no supported way to get a live video feed from a Lumix
camera — even though the cameras expose a perfectly usable WiFi streaming
interface.

LumixCam talks to the camera over its HTTP/CGI control API and receives the
MJPEG video stream it sends via UDP. It can also read from any V4L2 device
(USB capture cards, v4l2loopback, etc.), so you can switch between sources on
the fly.

## Features

- Live video preview from the camera's WiFi UDP stream or any V4L2 device
- Switch between sources at runtime (keys `1`/`2` or click the toolbar)
- Remote shutter release (`C`)
- Cycle focus modes: AFS / AFC / MF (`F`)
- Cycle AF area modes: Tracking, 1-Area, Pinpoint, Face Detect, 23-Area, 49-Area (`A`)
- Click-to-focus anywhere on the preview (right-click to focus + capture)
- One-shot autofocus (`O`)
- Battery and battery-grip status in the overlay
- V4L2 pixel format support: MJPEG, YUYV, BGR3, YU12

## Requirements

- Linux (Wayland or X11)
- Rust 2024 edition (1.85+)
- A Panasonic Lumix camera with WiFi (tested with cameras that expose the
  `cam.cgi` HTTP interface)
- The camera connected to the same network as the PC (the camera's own WiFi AP
  works — default IP is `192.168.54.1`)

## Build

```bash
cargo build --release
```

The binary is at `target/release/lumixcam`.

## Usage

Connect to the camera's WiFi, then:

```bash
# Default: stream from the camera over UDP
lumixcam

# Use a V4L2 capture card instead
lumixcam --source v4l2 --video-device /dev/video2

# Custom camera IP and UDP port
lumixcam --camera-ip 192.168.1.100 --udp-port 50000

# V4L2 with specific format
lumixcam --source v4l2 --video-device /dev/video0 \
    --input-format yuyv --video-size 1280x720 --framerate 30
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--camera-ip <ip>` | `192.168.54.1` | Camera IP address |
| `--source <name>` | `lumix-udp` | `lumix-udp` or `v4l2` |
| `--udp-port <port>` | `49152` | UDP port for the Lumix video stream |
| `--video-device <path>` | `/dev/video2` | V4L2 device path |
| `--input-format <fmt>` | `mjpeg` | V4L2 pixel format: `mjpeg`, `yuyv`, `bgr3` |
| `--video-size <WxH>` | `1920x1080` | V4L2 capture resolution |
| `--framerate <fps>` | `60` | V4L2 capture frame rate |

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `1` | Switch to Lumix UDP source |
| `2` | Switch to V4L2 source |
| `C` | Capture (shutter release) |
| `F` | Cycle focus mode (AFS / AFC / MF) |
| `A` | Cycle AF area mode |
| `O` | One-shot autofocus |
| `Q` / `Esc` | Quit |

Click on the video to focus at that point. Right-click to focus and capture.

## How it works

1. On startup, LumixCam registers with the camera over HTTP (`cam.cgi`) and
   puts it into recording mode.
2. It asks the camera to stream MJPEG video via UDP to a local port.
3. A background thread receives UDP packets, strips the proprietary header, and
   extracts JPEG frames.
4. The GUI (egui/eframe) renders the latest frame and overlays status
   information.
5. Camera commands (capture, focus, AF settings) are sent over HTTP on a
   separate worker thread so the UI never blocks.
6. A keepalive thread polls the camera state every few seconds to track battery
   level and keep the connection alive.

## License

MIT
