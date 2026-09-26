# gemdict — handoff

Build a tiny Windows dictation app to replace OpenWhispr for Ayush. It does one thing: press a hotkey, speak, press again, and the text from Google's Gemini transcription model is pasted where the cursor is. Everything is transcribed on Google's side, so there is no local model.

Written 2026-09-26 by a Claude Code session in `D:\hobby\random`. Everything below was checked against the installed app and its source on that date.

## Why

OpenWhispr (Electron, MIT, https://github.com/OpenWhispr/openwhispr, installed v4.0.129) is 782 MB installed, keeps 355 MB of local models in `~\.cache\openwhispr`, uses about 0.8 GB of RAM, and ships whisper.cpp, llama.cpp, sherpa-onnx, Qdrant, yt-dlp and meeting helpers. Ayush uses none of that. The goal is a light, reliable single binary.

## Current OpenWhispr setup (copy this behavior)

From `%APPDATA%\open-whispr\Local Storage` and `transcriptions.db`:

| Setting | Value |
|---|---|
| Activation | **tap / toggle** (press to start, press to stop), not hold |
| Hotkey | **Ctrl+Win** (`Control+Super`) |
| Provider / model | Gemini, BYOK, **`gemini-3.5-transcribe-live`** (streaming) |
| Language | `en-GB` |
| Cleanup / formatting | off; don't build it |
| Custom dictionary | 28 words (table `custom_dictionary`, column `word`, where `deleted_at IS NULL`) |
| History | 718 rows in `transcriptions`; 605 came from `gemini-streaming` |

**No prompt is sent.** OpenWhispr sends only audio plus optional language and vocabulary config. There's no system prompt or instructions.

## Gemini API facts (from `reference/openwhispr/src/helpers/`)

**Streaming, which is what he uses** (`geminiLiveStreaming.js`):
- WebSocket: `wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key=API_KEY`
- On open, send only the setup message, then wait for `{"setupComplete":{}}`, because audio sent before that is dropped. Buffer mic audio while connecting (OpenWhispr buffers up to 3 s):
  ```json
  {"setup":{"model":"models/gemini-3.5-transcribe-live",
            "generationConfig":{"responseModalities":["TEXT"]},
            "inputAudioTranscription":{"languageCodes":["en-GB"],"customVocabulary":["..."]}}}
  ```
  Cap vocabulary at 100 terms. Only send `languageCodes` when a language is explicitly set.
- Audio frames: `{"realtimeInput":{"audio":{"data":"<base64>","mimeType":"audio/pcm;rate=16000"}}}`, using 16 kHz mono 16-bit little-endian PCM.
- Server messages: `serverContent.interimInputTranscription.text` is a partial that gets revised, so replace it rather than appending. `serverContent.inputTranscription.text` is a final segment, so append it. `serverContent.generationComplete` marks the end of a turn. There is no `turnComplete`.
- The server closes turns on silence by itself, so one dictation can produce several final segments. Join them with spaces.
- On stop, send `{"realtimeInput":{"audioStreamEnd":true}}` and wait up to 3 s for the last final (it usually arrives in about 0.5 s), then close. Send a keepalive every 15 s. Allow 15 s for the connection to open.

**Batch fallback** (`geminiTranscription.js`): POST `https://generativelanguage.googleapis.com/v1beta/interactions` with header `x-goog-api-key`, body `{"model":"gemini-3.5-transcribe","input":[{"type":"audio","data":"<b64>","mime_type":"audio/wav"}],"generation_config":{"transcription_config":{"language_codes":["en-GB"],"custom_vocabulary":[...]}}}`. The text is in `output_text`, with a fallback of joining `steps[].content[].text`. Treat any `status` other than `completed` as an error. A bad key returns 400 `API_KEY_INVALID`, and a rate limit returns 429.

Streaming failures are the main reason to keep batch: if the socket fails, send the recorded audio to batch instead.

## Requirements

1. **Toggle hotkey Ctrl+Win**, configurable. It must work globally, including over elevated windows where possible.
2. Capture the default mic at 16 kHz mono PCM. Show a small unobtrusive "recording" indicator (tray icon state and/or a tiny overlay) and play a start/stop sound.
3. Stream to Gemini Live. On stop, paste the final text into the focused window: save the clipboard, set the text, send Ctrl+V, then restore the clipboard.
4. **History is required.** Store every transcription (text, time, duration, model, status or error) in SQLite. Opening the app from the tray shows a searchable list with **one-click copy**, because Ayush often dictates without a text box focused and copies the text from history afterwards. Also keep the last audio file of a failed transcription so it can be retried.
5. Import the existing OpenWhispr history (`transcriptions` table) and custom dictionary once, read-only.
6. **API key**: store it in Windows Credential Manager, entered once through a settings box. Offer a one-time import from OpenWhispr's `%APPDATA%\open-whispr\secure-keys` / `.env`, but never print, log or commit the key. Never put it in the repo.
7. Small settings: hotkey, language, dictionary words, and start with Windows.
8. Clear error toasts for a missing or invalid key, a rate limit and no network. A failed transcription must still land in history with its audio.
9. Target roughly <10 MB binary, <30 MB RAM idle, and zero CPU when idle.

Not needed: local models, cleanup/LLM rewriting, notes, meetings, calendar, agents, cloud sync or accounts.

## Suggested stack

**Rust** is the recommendation. Tauri would also be acceptable if the history UI is much easier as HTML; it uses the system WebView2, so it's still far lighter than Electron. Likely crates: `global-hotkey` (or a raw `RegisterHotKey`/low-level keyboard hook, because Ctrl+Win is a modifier-only chord that `RegisterHotKey` can't bind; see OpenWhispr's `windows-key-listener` for its approach), `cpal` plus resampling to 16 kHz, `tokio-tungstenite`, `reqwest`, `rusqlite` (bundled), `tray-icon`, `arboard`, `enigo` or `SendInput` for Ctrl+V, `keyring` for Credential Manager. Pick the history UI deliberately: a small native window (egui, or Slint) versus Tauri.

Decide this with Ayush at the start of the new thread, give a recommendation, then build.

## Reference

- `reference/openwhispr/` is a shallow clone of upstream (commit `8d29423`, 2026-09-26) and is gitignored. Useful files: `src/helpers/geminiLiveStreaming.js`, `geminiTranscription.js`, `audioManager.js`, and the Windows helpers (key listener, fast paste) under `resources/` / `native/`.
- Live OpenWhispr data: `%APPDATA%\open-whispr\` (`transcriptions.db`, `audio\` with 718 files / 294 MB). Open these **read-only** and don't modify them. Keep OpenWhispr installed until gemdict has matched it for a few days.

## Machine notes

- Windows 11, Acer laptop, 15.7 GB RAM that is often tight.
- The Audiocular D07 USB DAC mic can enumerate stuck and freeze dictation apps for about 60 s; the fix is to replug it. Handle a hung or failed mic open with a timeout and a toast instead of freezing.
- The folder isn't pushed anywhere yet. Ask Ayush before creating a GitHub repo (his account is `strix52`, private by default).
