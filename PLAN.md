# gemdict — implementation plan (v3, 2026-09-26)

`HANDOFF.md` defines what to build and the Gemini protocol. This plan defines how. v3 folds in two line-level reviews of v2 (Codex, Grok) and the earlier round of four critiques. The reviews are in the session scratchpad (`plan-critique-*.md`, `critique-*.md`).

Toolchain (installed): Rust 1.98.1 MSVC (`D:\dev\toolchains\{rustup,cargo}`), VS 2022 Build Tools C++ (`D:\dev\toolchains\VSBuildTools`).

## 0. Principles

1. **Never lose dictated audio or text.** Audio is spooled to a valid WAV continuously. The history row is committed before any paste. A leftover spool file at startup becomes a failed, retryable row.
2. **Idle means idle.** No socket, no audio stream, no timers, no repaint while idle. Idle threads block in `GetMessage` or `recv`.
3. **No async runtime.** Plain threads, blocking or non-blocking std I/O. No tokio or futures.
4. **Small and explicit.** One module per concern, plain structs and enums, one `Event` enum as the cross-thread vocabulary. No traits without two implementations, no generics without a second caller, no macros of our own.
5. **All `unsafe` lives in `src/win/`.** Each call is wrapped in a small safe function with a `// SAFETY:` note.
6. **Verify, then build.** Step 0 compiles tiny probes for every crate or Win32 assumption before real code depends on it.
7. **Measure.** Size, memory and wakeups are checked in release builds at steps 1, 4 and 8.

## 1. Budgets and gates

| Metric | Target | How measured |
|---|---|---|
| Release exe | < 10 MB (expect ~3–6) | file size (`strip = true`) |
| Idle **private bytes** (`PrivateMemorySize64`), never opened UI | < 30 MB | PowerShell, 10 min after start |
| Idle private bytes after one dictation and one history open→hide | < 30 MB | same |
| Idle CPU / wakeups | 0.00 % and ~0 context switches/s for 10 min | Process Explorer, thread view |
| Stop → paste, normal case | < 1 s | log timestamps |

**UI gate (step 1):** eframe (glow) with a hidden viewport must (a) show/hide ≥ 3 times from the tray, (b) not wake while hidden, (c) meet the idle memory target after show→hide. If any fails, the history/settings window is plain Win32 on the same thread as the tray and IPC window. **Only one UI implementation ever exists.** The losing prototype is deleted.

## 2. Architecture

### Threads

```
main (UI)   eframe/winit loop + tray-icon + history/settings window. Own read-only SQLite connection.
            Sends UiCmd to core. Woken by core via egui::Context::request_repaint().
hook        Message-only window "gemdict.ipc" + WH_KEYBOARD_LL + GetMessage loop.
            Hook callback: pure chord logic → on emit PostMessage(ipc, WM_APP_TOGGLE); nothing else.
            The window proc (outside the hook callback) sends Event::Toggle / Event::Power(..) / Event::ShowHistory to core.
            Also receives WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, and the second instance's "show" message.
core        Owns AppState, the write DB connection, settings, key. Blocks on core_rx.recv_timeout(next deadline).
            Does paste (on its own thread, short and serial).
capture     (per dictation) owns the cpal stream. cpal callback → mono f32 chunks → sync_channel(512).
            The capture thread drains that channel continuously: resample → append to WAV spool → push i16 frames
            to the Live queue. It never touches the network, so connection time can't starve capture.
live        (per dictation) connects, sends queued audio, reads transcripts, runs batch fallback when needed.
```

Channels are `std::sync::mpsc`. Every per-dictation event carries a `sid: u64` (monotonic session id); core drops events whose `sid` isn't current.

### Events (`src/event.rs`)

```rust
pub enum Event {
    Toggle,                          // hotkey or tray
    ShowHistory,                     // tray click or second instance
    Power(PowerEvent),               // Suspend | Resume | Lock | Unlock
    Ui(UiCmd),
    Capture { sid: u64, ev: CaptureEvent },   // Opened { rate, channels } | Failed(String) | Ended { dropped: u32, reason: Option<String> }
    Live { sid: u64, ev: LiveEvent },         // Text { text: String, provisional: bool } | Failed(GeminiError)
    Quit,
}
pub enum UiCmd { Copy(i64), Retry(i64), Delete(i64), SaveSettings(Settings), SetApiKey(String), ImportOpenWhispr, TestKey }
```

### Core state machine (`src/core.rs`)

```
Idle      ─Toggle─▶ Starting   new sid; spawn capture; start sound; overlay "Starting…"; deadline = now+3 s
Starting  ─Capture::Opened─▶ Recording   spawn live (shares the Live queue); overlay "Listening"
Starting  ─Toggle─▶ Idle       cancel: stop flag set, audio discarded (nothing was said yet)
Starting  ─deadline / Capture::Failed─▶ Idle   notify "Microphone didn't respond"; see §6.2 stuck rule
Recording ─Toggle─▶ Finishing  stop flag; stop sound; capture HWND = GetForegroundWindow(); overlay "Transcribing…"
Recording ─Capture::Ended(reason)─▶ Finishing   (unplug/error) same as stop, reason noted
Finishing ─Live::Text─▶ commit row(ok) ─▶ paste ─▶ update row.paste ─▶ Idle
Finishing ─Live::Failed─▶ commit row(failed, audio kept) ─▶ notify ─▶ Idle
Finishing ─Toggle─▶ stays queued in the channel; handled after returning to Idle (starts a new dictation)
any       ─Quit─▶ if Recording: stop and finish (bounded by Live's own deadlines), then exit
any       ─Power(Suspend|Lock)─▶ if Recording: stop and finish
```

Deadlines are absolute `Instant`s. The core loop computes `recv_timeout(min(deadlines) - now)` after every event, so unrelated events (overlay hide) never reset a deadline. Empty result text (nothing said) = no row, overlay "Nothing heard", spool deleted.

`duration_ms` = time from `Capture::Opened` to stop.

## 3. Crate layout and line budgets

```
Cargo.toml, build.rs (embed manifest: asInvoker, PerMonitorV2 DPI; icon)
assets/   idle.ico, rec.ico, start.wav, stop.wav
src/
  main.rs         ~80   single-instance mutex, logger, spawn threads, run UI
  event.rs        ~50   Event, UiCmd, CaptureEvent, LiveEvent, PowerEvent
  error.rs        ~60   one AppError enum (Mic, Gemini(GeminiError), Db, Io, Clipboard, Key) + user_message()
  core.rs        ~280   state machine, deadlines, row writes, notifications
  hotkey.rs      ~160   Chord spec parse/format + pure ChordMachine (unit-tested)
  capture.rs     ~170   cpal open, stuck-open rule, callback, drain loop, resampler use
  audio.rs       ~140   Resampler (3:1 with remainder; linear otherwise), f32→i16, WavSpool (own 44-byte header, patch sizes on flush)
  gemini/
    mod.rs        ~60   GeminiConfig, GeminiError, error mapping, key scrubbing
    live.rs      ~240   connect, non-blocking loop, turn tracking, stop rule, fallback trigger
    batch.rs      ~90   ureq POST /v1beta/interactions
    protocol.rs  ~120   build setup/audio/end JSON; parse server messages into small structs (unit-tested with fixtures)
  store.rs       ~230   schema, user_version migrations, insert/update/search/delete, recovery of leftover spool
  import.rs      ~150   OpenWhispr history + dictionary + key import (read-only)
  paste.rs       ~120   target checks, focus restore, clipboard save/restore, SendInput sequence
  settings.rs     ~80   Settings + JSON load/save (corrupt file → defaults + backup)
  notify.rs       ~60   overlay text/state; tray icon state
  ui/
    mod.rs        ~70   tray menu + window show/hide
    history.rs   ~200   search, virtual list, copy/retry/delete
    settings.rs  ~160   hotkey capture, language, dictionary, autostart, key box, import button
  win/
    mod.rs        ~20
    ipc.rs       ~120   message-only window, WM_APP_*, power/session notifications, second-instance message
    hook.rs      ~120   SetWindowsHookEx install/reinstall, callback, VK_E8 injection, GetAsyncKeyState sync
    input.rs     ~100   modifier release/restore (EXTENDEDKEY for right Ctrl/Alt and Win), Ctrl+V / Ctrl+Shift+V
    clipboard.rs ~120   open with retries; get/set CF_UNICODETEXT; get/set CF_HDROP; sequence number
    window.rs    ~100   foreground HWND, class name, owner PID, elevated check, focus restore
    cred.rs       ~80   CredReadW / CredWriteW / CredDeleteW / CredEnumerateW
    aesgcm.rs     ~60   BCrypt AES-256-GCM decrypt (for OpenWhispr key import only)
    overlay.rs   ~140   layered no-activate click-through GDI window
    autostart.rs  ~40   HKCU Run value set/delete
    sound.rs      ~20   PlaySoundW(SND_MEMORY|SND_ASYNC)
```

Target ~3,300 lines including tests. Past ~4,000: stop and cut.

## 4. Dependencies

```toml
[dependencies]
eframe     = { version = "0.36", default-features = false, features = ["glow", "default_fonts"] }  # UI gate decides
tray-icon  = { version = "0.25", default-features = false }
cpal       = "0.18"
tungstenite = { version = "0.30", default-features = false, features = ["handshake", "native-tls"] }
native-tls = "0.2"                                   # schannel
ureq       = { version = "3", default-features = false, features = ["native-tls-no-default", "json"] }  # probe in step 0
rusqlite   = { version = "0.40", features = ["bundled"] }
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
base64     = "0.23"                                  # probe in step 0
log        = "0.4"
windows    = { version = "0.62", features = [ /* exact list, added as used */ ] }

[profile.release]
opt-level = "z"      # compared with "s" in step 8
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

Not used: tokio, reqwest, rustls/ring/aws-lc, **keyring** (4.x has no Windows-native feature; `win/cred.rs` is simpler), **hound** (header sizes are wrong until drop; own 44-byte header), arboard, enigo, rubato, winrt-notification, global-hotkey, anyhow, chrono, uuid.

Logging: `log` facade + ~30-line file logger → `%LOCALAPPDATA%\gemdict\gemdict.log`, rotated at 1 MB. The key and full WebSocket URL are never logged; URLs are logged without their query. Error bodies are scrubbed (key value and any `key=` query value removed) before reaching the log, DB or UI.

## 5. Data

### Paths
- `%APPDATA%\gemdict\settings.json`, `%APPDATA%\gemdict\gemdict.db`
- `%LOCALAPPDATA%\gemdict\spool\<sid>.wav` (in progress), `failed\<row id>.wav`, `gemdict.log`

### Schema (`PRAGMA user_version = 1`, `journal_mode=WAL`, `busy_timeout=2000`)

```sql
CREATE TABLE transcriptions (
  id          INTEGER PRIMARY KEY,
  created_ms  INTEGER NOT NULL,              -- unix ms UTC
  text        TEXT    NOT NULL DEFAULT '',
  duration_ms INTEGER,
  model       TEXT,
  status      TEXT    NOT NULL,              -- 'ok' | 'provisional' | 'failed'
  error       TEXT,                          -- scrubbed, human-readable
  paste       TEXT,                          -- 'attempted' | 'skipped:<reason>' | 'failed:<reason>' | NULL
  audio_path  TEXT,                          -- kept for failed/provisional rows
  source      TEXT    NOT NULL DEFAULT 'gemdict',
  source_id   INTEGER,
  UNIQUE(source, source_id)
);
CREATE INDEX transcriptions_created ON transcriptions(created_ms DESC);
CREATE TABLE dictionary (word TEXT PRIMARY KEY COLLATE NOCASE);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
```

- `provisional` = Live gave only interim text or batch failed after Live text arrived. The text is shown and pasted, and the audio is kept so Retry can give a clean result.
- Search: `text LIKE ?1 ESCAPE '\'` (user input escaped), newest first, `LIMIT 200`. No FTS.
- Retention of kept audio: max 20 files / 200 MB, oldest first; the row keeps its text/error with `audio_path = NULL`.
- Commit order for a failure: WAV moved to `failed\` → row inserted with that path in one transaction → only then is the spool entry considered handled. If the DB write fails, the WAV stays in `spool\` and startup recovery picks it up.
- Retry: batch on the kept WAV → update row in a transaction → delete the WAV only after commit.
- Startup recovery: each `spool\*.wav` → header repaired from file length → failed row "Recovered after crash".

### OpenWhispr import (read-only, idempotent; offered on first run and in Settings)

**History and dictionary**
- Open `%APPDATA%\open-whispr\transcriptions.db` in place with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI | SQLITE_OPEN_NO_MUTEX`, URI `file:…?mode=ro`, `busy_timeout=2000`. Normal WAL reading sees committed rows. No file copying. The source DB's files are never written.
- Probe columns with `pragma_table_info`. Select `id, text, timestamp, audio_duration_ms, model, status, error_message` (only those present), `WHERE deleted_at IS NULL` if that column exists.
- Timestamp parser accepts `YYYY-MM-DD[ T]HH:MM:SS[.fff][Z|±HH:MM]`; no zone = UTC. Rows that fail to parse are counted and reported ("Imported 716, skipped 2").
- `status = 'completed'` → `ok`; anything else → `failed`. `INSERT OR IGNORE` by `(source='openwhispr', source_id)`.
- Dictionary: `SELECT word FROM custom_dictionary` (+ `WHERE deleted_at IS NULL` if present). No audio is copied. A `meta` row records the import time.

**API key** (verified against `src/helpers/secretCrypto.js`)
- OpenWhispr keeps a random 32-byte master key in Credential Manager (`@napi-rs/keyring`, service `OpenWhispr`, account `secrets-master-key`, stored base64) and each secret in `secure-keys\<NAME>.enc` as `IV(12) | tag(16) | AES-256-GCM ciphertext`.
- Import: `CredEnumerateW` filtered to generic credentials whose target contains `OpenWhispr` and whose user name is `secrets-master-key` (the exact target-name format of keyring-rs is probed, not assumed). Decode the blob (UTF-16LE, else UTF-8) → base64 → must be 32 bytes. Read `secure-keys\GEMINI_API_KEY.enc`, decrypt with `win/aesgcm.rs` (BCrypt, `BCRYPT_CHAIN_MODE_GCM`). Store in gemdict's own credential (`CredWriteW`, target `gemdict/gemini-api-key`, `CRED_PERSIST_LOCAL_MACHINE`). Then run the key test.
- The master key and plaintext are zeroed after use, never logged, never displayed. OpenWhispr's credential and files are only read.
- If the master credential is missing (older safeStorage mode) or decryption fails: check `.env` for `GEMINI_API_KEY=`; otherwise Settings says "Couldn't import from OpenWhispr — paste a key".

## 6. Components

### 6.1 Hotkey (`hotkey.rs` logic, `win/hook.rs` + `win/ipc.rs` plumbing)

Spec: required modifiers {Ctrl, Alt, Shift, Win} (left/right both count) + optional non-modifier key. Default Ctrl+Win. The current spec is packed into an `AtomicU64` that the callback loads; the core stores a new one on settings change. There's no lock and no core→hook message.

**Modifier-only chord, fires on release, only if clean:**
- In the callback: ignore `LLKHF_INJECTED`. Update the modifier state for the current key from the event. For every *other* modifier, resync from `GetAsyncKeyState` (OpenWhispr's `SyncModifierState`), so a missed key-up (Win+L, UAC) can't stick the chord.
- `armed` when all required modifiers are down and no non-modifier key-down has been seen since the first went down. A non-modifier key-down while armed → `dirty` (Ctrl+Win+←/→/D still work).
- When a Win key-up arrives while `armed && !dirty` and Win is part of the chord: inject `VK_E8` down+up (`SendInput`, marked by our own dwExtraInfo) **before** returning `CallNextHookEx`, so the Start menu doesn't open.
- When the last required modifier goes up: emit if `armed && !dirty`, then reset.
- Emit = `PostMessageW(ipc_hwnd, WM_APP_TOGGLE)`. The callback does nothing else and never allocates, locks or logs.

**Chord with a key (e.g. Ctrl+Shift+Space):** fire on that key's first key-down with modifiers matching; repeats ignored.

Hook health: reinstall on `Resume` and `Unlock` (from ipc window), and reset the chord state. The hook works while an elevated window is focused? It generally does *not* for keys delivered to higher-integrity windows (UIPI). This is tested in step 1 and documented in the README either way.

Unit tests (table-driven): clean chord, dirty chord (arrows, D), repeats, L/R variants, release order, missed key-up recovery, injected ignored, key chord.

### 6.2 Capture (`capture.rs`, `audio.rs`)

- `cpal` default host (WASAPI shared), `default_input_device()`, an **f32** config at the device's default rate and channel count (the probe checks the D07 and the laptop mic; if f32 isn't offered, convert from i16).
- `build_input_stream(config, data_cb, err_cb, Some(Duration::from_secs(3)))`.
- Data callback: average channels → `Vec<f32>` → `try_send` into `sync_channel(512)`. On full, bump `AtomicU32 dropped`. No blocking.
- Capture thread: after `play()` succeeds, check the stop/cancel flag. If core already gave up (stale `sid`), drop the stream and exit without `Opened`. Otherwise send `Opened` and loop: `recv_timeout(50 ms)` → resample → `WavSpool::write` → push i16 samples into the Live queue. On stop: drain what's left, finalize WAV, send `Ended { dropped }`, drop the stream.
- **Stuck-open rule:** at most one capture thread may be unresolved. If one timed out and hasn't exited, a new Toggle shows "Microphone still stuck — replug it" and doesn't spawn. When the stuck thread finally returns, it exits quietly and clears the flag (checked through an `Arc<AtomicBool>`). No timed backoff.
- No default mic → immediate `Failed("No microphone")`.

Resampler: exactly 48 000 → average each 3 samples, carrying a 0–2 sample remainder across chunks. Other rates → linear interpolation with a fractional phase carried across chunks. Unit tests: length, remainder carry, 1 kHz sine stays 1 kHz.

`WavSpool`: writes a 44-byte header with placeholder sizes, appends PCM, and every ~1 s (and at finalize) seeks back to patch the RIFF/data sizes. A crash leaves a WAV that's at most 1 s short; startup repair recomputes sizes from the file length anyway.

**Live queue:** `Mutex<VecDeque<i16>>` + `Condvar`, capped at **15 s** of audio (480 KB). It only fills while Live is connecting or slow. If the cap is hit, Live is abandoned for this dictation and batch uses the full WAV. Nothing is lost either way, because the WAV has everything.

### 6.3 Gemini Live (`gemini/live.rs`, `gemini/protocol.rs`)

Connect (one absolute 15 s deadline covering all of it):
1. Resolve `generativelanguage.googleapis.com:443` on a helper thread and wait for it ≤ remaining deadline (std `ToSocketAddrs` has no timeout).
2. `TcpStream::connect_timeout`, `set_nodelay(true)`.
3. `tungstenite::client_tls(request, stream)` with read/write timeouts = remaining deadline during the handshake.
4. Send setup: `{"setup":{"model":"models/<live model>","generationConfig":{"responseModalities":["TEXT"]},"inputAudioTranscription":{…}}}`. `inputAudioTranscription` is always present; `languageCodes` only if a language is set; `customVocabulary` only if non-empty, max 100.
5. Switch the inner `TcpStream` to non-blocking (`MaybeTlsStream::NativeTls(s) => s.get_ref().get_ref().set_nonblocking(true)`; exact path probed in step 0). Wait for `setupComplete` inside the loop below; don't send audio before it.

Loop (sleeps on the queue `Condvar` with a 10 ms timeout, so it's idle between frames; runs only while dictating):
- If `setup_complete`: take up to 10 frames of 100 ms (3,200 bytes each) from the queue → `realtimeInput.audio` messages. A `WouldBlock` on write keeps the message pending and retries next pass (tungstenite buffers it; call `flush()`).
- `read()` until `WouldBlock`:
  - `Text` → `protocol::parse` returns a struct with any of: `setup_complete`, `interim`, `final_text`, `generation_complete`. Apply all present fields (not else-if): interim replaces `interim` and sets `turn_open = true`; final is trimmed and appended to `finals`, `interim` cleared; `generation_complete` sets `turn_open = false`.
  - `Ping` → tungstenite queues the Pong; call `flush()`. `Pong` → clear `awaiting_pong`.
  - `Close(1007 | 1008)` → `GeminiError::KeyInvalid` (no batch; it would fail the same way). Other close / I/O error → fallback.
- Keepalive: every 15 s send a Ping; if `awaiting_pong` is still set at the next tick, treat the socket as dead → fallback.
- Stop: send the remaining queue, then `{"realtimeInput":{"audioStreamEnd":true}}`. If `!turn_open`, finish immediately. Otherwise read until `turn_open == false` or 3 s from `audioStreamEnd`. Close the socket (best effort, no waiting).
- Result: `finals.join(" ")`. If empty but `interim` isn't → `provisional`.

Fallback (any failure before the result, other than KeyInvalid): wait for capture to finalize the WAV, then batch. If batch succeeds → `ok` with batch text. If batch fails and Live had text → `provisional` with Live text and audio kept. Otherwise → `failed` with audio kept.

### 6.4 Batch (`gemini/batch.rs`)
- One `ureq::Agent` built with native TLS explicitly and `timeout_global = 90 s` (covers DNS, connect, send, read).
- POST `…/v1beta/interactions`, header `x-goog-api-key`, body per HANDOFF with snake_case `language_codes` / `custom_vocabulary`, WAV base64 inline.
- Size cap: raw WAV ≤ 15 MB (~8 min at 16 kHz mono i16; base64 body ~20 MB). Over the cap → `failed: too long for fallback`, audio kept (Live text, if any, is stored as provisional).
- Parse: `status` present and not `completed` → error; `output_text`, else join `steps[].content[].text`.
- Errors: 400 with `API_KEY_INVALID`, 401, 403 → `KeyInvalid`; 429 → `RateLimited`; DNS/connect/TLS/timeout → `Offline`; other → `Other(scrubbed, ≤ 200 chars)`.

**Key test** (Settings "Test" and after import): batch with an embedded 1 s WAV of a spoken word (in `assets/`). Success = any response that isn't `KeyInvalid`, `Offline` or `RateLimited`; empty text still counts as success.

### 6.5 Paste (`paste.rs`, `win/*`)

Run after the row is committed:
1. `target` = the HWND captured at stop. Skip (and say "Saved to history — couldn't paste: <reason>") if it's null, destroyed (`IsWindow`), gemdict's own, or elevated. Elevated check: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` on its PID → if access is denied, treat as elevated; otherwise compare token integrity levels.
2. If the foreground isn't `target`: `AttachThreadInput` + `SetForegroundWindow(target)`, then re-check. If it failed → skip with `focus`.
3. Save the clipboard: open with 10 × 20 ms retries; save `CF_UNICODETEXT` and `CF_HDROP` if present (both HGLOBAL, well-defined). Anything else is not saved. If other formats were present, they're lost only if we later restore; see step 6.
4. Set `CF_UNICODETEXT` = text. Also set `ExcludeClipboardContentFromMonitorProcessing` and `CanIncludeInClipboardHistory = 0` so dictations don't fill Win+V history. Record `seq`.
5. Input: remember which modifiers are physically down (`GetAsyncKeyState`). Send key-ups for them (`KEYEVENTF_EXTENDEDKEY` for right Ctrl, right Alt, both Win). Then Ctrl+V, or Ctrl+Shift+V when the class is `CASCADIA_HOSTING_WINDOW_CLASS` or `ConsoleWindowClass`. Then key-downs for the modifiers that are *still* physically down (recheck), so a held chord keeps working. `SendInput` returning fewer events than sent → `failed:input-blocked`.
6. Wait 500 ms. If `GetClipboardSequenceNumber() == seq` and we saved something, restore it. If the original clipboard held only formats we don't save (images, rich text), leave the transcript there rather than wipe it.
7. `paste = 'attempted'` (Windows gives no proof of insertion).

### 6.6 Overlay and notices (`notify.rs`, `win/overlay.rs`)
- One layered, topmost, no-activate, click-through, tool window (GDI text and a coloured dot), bottom-centre of the monitor with the cursor. Created on first use on the UI thread, then shown/hidden. States: Listening (red), Transcribing (grey), Saved / Nothing heard / error text (4 s).
- Tray icon swaps idle/recording. No WinRT toasts and no `Shell_NotifyIcon` balloons (tray-icon doesn't expose its HWND). The overlay is the single notice surface. Errors are also visible in history rows.

### 6.7 UI (`ui/`)
- Tray menu: Open history · Start/stop dictation · Settings · Quit. Left-click opens history.
- History: search (150 ms debounce), virtual list (`ScrollArea::show_rows`), each row shows time, first ~2 lines, status chip, **Copy** (flash "Copied"), **Retry** (rows with audio), **Delete** (inline confirm). Double-click copies.
- Settings: hotkey capture (a small in-window key capture that reuses `hotkey::Chord` formatting), language (blank = auto, default `en-GB`), dictionary (one per line, counter, max 100), start with Windows, API key (write-only; shows set/not set) + Test, Import from OpenWhispr (shows counts and whether the key was imported). A note: "Paste doesn't work into apps running as administrator; the text is in history."
- Close = hide.

### 6.8 Single instance, autostart, power
- Named mutex `Local\gemdict-7c1e0d2a` (compile-time constant). A second launch finds the `gemdict.ipc` message-only window with `FindWindowExW(HWND_MESSAGE, …)` and posts `WM_APP_SHOW`, then exits.
- The app always starts in the tray. Autostart = HKCU `Run` value `gemdict` = quoted exe path; unchecking deletes it.
- Suspend/lock during recording → stop and finish normally. Resume/unlock → reinstall hook, reset chord.

## 7. Build steps (commit after each; each lists its acceptance)

**Step 0 — probes (throwaway, in `probes/`, not shipped).** Compile and run: eframe 0.36 glow hidden viewport + tray-icon show/hide; tungstenite 0.30 `client_tls` + non-blocking inner stream path; ureq 3 native-tls agent HTTPS GET; cpal 0.18 default input config on the laptop mic and the D07; `windows` 0.62 `CredEnumerateW` listing target names containing `OpenWhispr` (names only), BCrypt GCM round-trip on test data; base64 0.23 API. Record exact versions/features in `NOTES.md`.

**Step 1 — skeleton + UI gate.** Cargo project, release profile, single instance, ipc window, hook + chord machine (tests), tray, sounds, overlay, core loop logging Toggle, history window with dummy rows.
- Accept: Ctrl+Win toggles; Ctrl+Win+→ switches desktop without toggling; Start menu stays closed; lock/unlock doesn't stick the chord; hotkey with an elevated window focused (record result); second launch shows the first; show/hide ×3.
- Measure memory and wakeups; apply the UI gate; write numbers to `NOTES.md`.

**Step 2 — credentials, settings, key import.** `win/cred.rs`, `win/aesgcm.rs`, settings JSON, Settings UI with key box, OpenWhispr key import.
- Accept: key persists across restart and isn't in any file; import succeeds from OpenWhispr (key never printed); corrupt settings file → defaults + backup.

**Step 3 — capture + spool.** Capture thread, stuck-open rule, resampler (tests), WavSpool, debug tray item "Record 5 s".
- Accept: WAV plays cleanly; unplug D07 mid-recording → `Ended` and a valid WAV; simulated hang (debug flag sleeps in open) → notice, app usable, second Toggle refused until the thread exits; kill the process mid-recording → next start recovers a failed row.

**Step 4 — Gemini + store.** protocol.rs with fixtures (setup, interim/final/generationComplete in one message, close codes), Live loop, batch, error mapping, store + recovery.
- Accept: 10 real dictations incl. > 1 min with pauses; pause-then-stop pastes/saves < 1 s after stop; artificial 2 s connect delay doesn't clip the first word; invalid key → close 1008 → "Key invalid", failed row; network off → failed row with audio; kill the socket after a final with batch failing → provisional row with the Live text; retry fills failed rows; key absent from log (grep).
- Measure memory/wakeups after 3 dictations.

**Step 5 — paste.** §6.5.
- Accept: Notepad, VS Code, Chrome textarea, Windows Terminal (Ctrl+Shift+V), elevated PowerShell (skipped with notice), no text box (history has it), focus moved during transcription (restored to the right window), Explorer file copy survives, a copy made during the 500 ms isn't overwritten, a held key-chord still works afterwards.

**Step 6 — history UI.** Search, copy, retry, delete; 1,000 rows scroll smoothly.

**Step 7 — import + dictionary + autostart.** History import (twice → no duplicates; mixed timestamp formats; OpenWhispr running during import), dictionary editor, autostart toggle.

**Step 8 — hardening and budgets.** Sleep/resume mid-recording, quit mid-session, disk full (spool write error → failed row with what exists), rapid toggles, 30-minute idle soak, `opt-level` z vs s, final size/memory/wakeups, `README.md` (usage, data locations, limits, uninstall).

## 8. Coding conventions

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` before each commit.
- `unsafe` only in `win/`; each `unsafe` block has a `// SAFETY:` line. Crate root: `#![warn(unsafe_op_in_unsafe_fn)]`. A `deny(unsafe_code)` attribute on each non-`win` module isn't worth the friction; review enforces it.
- One `AppError` enum with `Display` + `user_message()`. No `unwrap()`/`expect()` outside tests and documented startup invariants.
- Pure logic (chord machine, resampler, WAV header, protocol parse/build, error mapping, timestamp parsing, LIKE escaping) is plain functions with unit tests. Win32/network code stays thin.
- No global mutable state beyond: the hook's chord state (hook thread only), the `AtomicU64` hotkey spec, and the ipc HWND in a `OnceLock`.
- Comments explain why. Use HANDOFF's terms (Live, batch, final, interim).
- Manual acceptance results go in `NOTES.md` per step.

## 9. Risks

| Risk | Where | Fallback |
|---|---|---|
| eframe hidden viewport wakes or uses too much memory | Step 0/1 | Win32 window, one implementation |
| keyring-rs credential target name differs from assumption | Step 0 | enumerate and match on user name `secrets-master-key` |
| Non-blocking schannel stream misbehaves | Step 0/4 | blocking stream + short `SO_RCVTIMEO` on a separate reader thread |
| D07 hangs inside WASAPI | Step 3 | stuck-open rule; `wasapi` crate only if cpal fails otherwise |
| Start menu still opens despite VK_E8 | Step 1 | swallow the Win key-up only for a clean chord |
| Focus restore blocked by Windows foreground rules | Step 5 | skip with notice; text is in history |
