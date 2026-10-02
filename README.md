# Bufferless

A lightweight always-on replay buffer for Windows. Press a hotkey and the last
N seconds of your screen (with system audio and optionally your mic) are saved
as an MP4, like NVIDIA Instant Replay without the overlay.

- Single exe (300 KB, or 55 KB packed), no runtime and no DLLs beyond what ships with Windows
- Works on NVIDIA, AMD and Intel GPUs (hardware H.264 through Media Foundation)
- Saving a clip takes milliseconds because nothing is re-encoded

## Usage

Run `bufferless.exe`. It sits in the tray as a red record icon.

| Action | How |
| --- | --- |
| Save the last N seconds | `Alt+F10` (default, changeable) or tray menu → Save clip |
| Settings | Double-click the tray icon, or right-click → Settings |
| Open clips | Click the "Clip saved" notification, or tray menu → Open clips folder |

Clips go to `Videos\Bufferless` and are named after the app in the foreground,
for example `cs2 2026-10-02 21-14-03.mp4`.

Settings live in `%APPDATA%\bufferless\config.toml` and the log is next to it.

## How it works

```
Desktop Duplication ──► D3D11 video processor ──► hardware H.264 MFT ──┐
 (BGRA texture, GPU)     (BGRA→NV12 + scaling)    (NVENC/AMF/QSV)       ├─► ring buffer of
WASAPI loopback + mic ─► mixer (QPC timeline) ──► AAC encoder MFT ─────┘   encoded packets
                                                                            │ hotkey
                                                                            ▼
                                                              MP4 muxer (faststart, no re-encode)
```

- Frames never leave the GPU: capture, colour conversion, scaling and encoding all
  happen on D3D11 textures.
- The ring buffer stores *encoded* packets, not raw frames. A minute of 1080p60 at
  20 Mbps is about 150 MB.
- Video is paced at a constant frame rate. When the screen hasn't changed, the last
  frame is encoded again.
- Audio packets are placed on the same QPC clock as video using WASAPI's device
  timestamps. Gaps (loopback sends nothing during silence) are filled with silence.
- One keyframe per second, so a clip starts at most one second earlier than asked.

| File | Role |
| --- | --- |
| `src/capture.rs` | Desktop duplication, GPU conversion, frame pacing |
| `src/encoder.rs` | Hardware H.264 encoder (async MFT) |
| `src/audio.rs` | WASAPI capture, mixing, AAC encoding |
| `src/ring.rs` | Replay buffer |
| `src/mux.rs` | MP4 writer |
| `src/app.rs` | Tray icon, hotkey, saving |
| `src/settings.rs` | Settings window |

## Building

Requires Rust (MSVC toolchain) on Windows 10 or 11.

```
cargo build --release
```

The same code builds three variants:

| Variant | Size | How | Notes |
| --- | --- | --- | --- |
| `bufferless.exe` | ~300 KB | `cargo build --release` | Standard build. The safest bet with antivirus. |
| `bufferless-small.exe` | ~107 KB | `.\build-tiny.ps1` | No Rust std and no C runtime. |
| `bufferless-tiny.exe` | ~55 KB | `.\build-tiny.ps1` | The small build packed with UPX. Packed exes are often flagged by antivirus heuristics. |

The small builds use the `nostd` feature: `src/rt.rs` provides the few things the
app needs from std (threads, a mutex, files, the allocator, the process entry
point) as thin Win32 wrappers, which every build goes through. `build-tiny.ps1`
needs a nightly toolchain and [UPX](https://upx.github.io/)
(`winget install UPX.UPX`); pass `-NoUpx` to skip the packed one.

GitHub Actions builds all three on every push (download them from the run's
artifacts) and attaches them to a release when a `v*` tag is pushed.

## Known limitations

- The mouse cursor isn't drawn into clips (Desktop Duplication delivers it separately).
- HDR desktops are captured as SDR.
- Portrait (rotated) monitors aren't rotated back.
- If the NVIDIA overlay is running it owns `Alt+F10`, so pick another hotkey or turn
  off its Instant Replay.
