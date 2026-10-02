# dictap 0.1.1 verification

Validated on Windows x64 on 2026-10-02.

## Scope

Pass-one correctness, failure handling, architecture and efficiency changes. Additional product features remain deferred except the requested tray copy action and launcher installation repair.

The correction pass addressed all ten findings from the implementation review:

- Capture reservation releases exactly once, including explicit completion and destructor paths.
- Retained-audio disposition is committed with the row, so restart recovery preserves SaveOnly audio after a failed rename.
- Clipboard fallback checks delivery permission, and input rechecks window identity, foreground and reachability.
- UI requests admit one pending action per kind, retain newer drafts, acknowledge clipboard writes, and retain key input on failed saves.
- Reply capacity is reserved before admission; stale and duplicate replies cannot evict accepted current replies. A pending-request timer drains results even if the posted wake fails.
- Abnormal Live closes remain provider failures. Live finalization waits through the bounded final window rather than treating unordered generation completion as transcription completion.
- Partial native input releases unbalanced injected key downs before restoring original modifiers.
- Silence cleanup records a durable deletion tombstone and reports surviving audio truthfully.
- Key changes adopt the validated secret only after successful persistence, without depending on a second credential read.
- Address attempts leave a handshake reserve and allow another address after a stalled connect.

The tray's **Copy last transcription** uses the latest nonempty saved transcript, skips empty failed entries, and reports success only after the clipboard write succeeds. The Start menu shortcut opens History and the installer registers the app in Windows Installed apps.

## Evidence

- `cargo fmt --check`: passed.
- `cargo test --offline -- --skip win::cred::tests::roundtrip --skip gemini::live::tests::live_and_batch_real`: 180 passed, zero failed, two ignored, two filtered.
- `cargo clippy --offline --all-targets -- -D warnings`: passed.
- `cargo build --offline --release`: passed; executable 2,302,464 bytes.
- Four previously failing review regressions now pass in the regular suite.
- Named ignored microphone smoke: opened the real default device, captured briefly, stopped, finalized WAV and released the reservation. Audio was not uploaded.
- Named ignored clipboard smoke: synthetic text/privacy formats round-tripped, a competing copy prevented restoration, and the original clipboard was restored.
- Named ignored provider smoke: only locally generated synthetic speech was sent to Gemini. Live and batch both returned the expected text including the final words "blue umbrella". Observed elapsed times were about 10.1 seconds for Live including playback/final wait and 6.2 seconds for batch; these are smoke observations, not performance benchmarks.
- Installed executable SHA256 matches the tested release. Windows registration reports version 0.1.1; the Start menu shortcut targets the installed executable with `--history`; Windows StartApps lists dictap. One installed process is running and responding.

## Practical limits

Normal Gemini Live output has no whole-stream transcription completion proof. It is delivered promptly, marked provisional in history, and its audio retained for retry. Real stream errors still enter the failure/fallback path. The bounded final wait reduces late-tail loss without claiming a completion guarantee.

Input count confirms an attempted paste, not acceptance by every application. Adverse scheduling, lock, focus, identity, clipboard and partial-input cases have deterministic coverage; no real lock/suspend or forced partial native injection was attempted. Real target-application paste and Raycast search UI were not independently exercised in this correction pass. Computer Use's Raycast launch approval timed out.

The previous installed executable was backed up before replacement. History, settings and the saved API key were not reset. No new runtime dependencies or database schema version were introduced.
