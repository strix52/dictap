# Probe notes (step 0)

Full reports: `probes/{audio,crypto,net,ui}/REPORT.md`.

## Versions in use

- cpal 0.18.2: `device.description()?.name()`; `build_input_stream` takes `StreamConfig` by value; 480-frame callbacks at 48 kHz; dropping the stream can report a transient Xrun (ignored).
- tungstenite 0.30 (`handshake`, `native-tls`): after the handshake, `MaybeTlsStream::NativeTls(tls) => tls.get_ref().set_nonblocking(true)`. `WouldBlock` arrives as `Error::Io(e)` with `e.kind() == WouldBlock`.
- ureq 3.4 needs the **`native-tls`** feature. `native-tls-no-default` compiles but panics on the first HTTPS call. Use `TlsProvider::NativeTls` + `RootCerts::PlatformVerifier`. `Error::StatusCode` drops the body, so the agent uses `http_status_as_error(false)`.
- windows 0.62: BCrypt AES-GCM passes NIST case 14; `CredReadW` reads OpenWhispr's master key.

## UI gate: eframe fails, so plain Win32

eframe 0.36.2 (glow) + tray-icon 0.25.1, release, hidden and never shown:

| | Measured | Budget |
|---|---|---|
| Private bytes | 44–48 MB | < 30 MB |
| Idle CPU | 250–300 ms per 60 s (~0.4 % of a core) | 0 |
| Exe | 5.5 MB (probe alone) | < 10 MB total |

Show/hide, virtual list and close-to-hide all worked, but the idle memory and CPU fail the gate. Per PLAN §UI gate, the history/settings window and tray are plain Win32 on the ipc thread. eframe and tray-icon are not used.

For comparison, the gemdict release build (hook, overlay, core, no UI yet) idles at 2.2 MB private, 12.7 MB working set, 0 CPU, 1.9 MB exe.
