//! End-to-end integration test for `efd_dsp::drm`.
//!
//! Gated with `#[ignore]` because it needs:
//! - the vendored `dream` binary (built via `third_party/dream/build.sh`)
//! - a running PipeWire session with `pactl`/`pacat`/`parec` available
//!
//! Run with:
//! ```
//! cargo test -p efd-dsp --test drm_integration -- --ignored --nocapture
//! ```
//!
//! What it exercises:
//! 1. `spawn_drm_bridge` creates the two null sinks and launches the
//!    dream/pacat/parec subprocesses.
//! 2. A known-good DRM recording (VoR FLAC) is pushed directly into the
//!    bridge's `drm_in` sink via `paplay`, simulating what the Rust
//!    IQ→pacat path would produce if fed a real DRM IQ stream.
//! 3. The bridge's `audio_tx` receives decoded DRM audio, which we
//!    verify is non-silent.
//!
//! The test intentionally bypasses the Rust IQ→pacat path because we
//! don't have a baseband-IQ DRM recording handy; the FLAC recordings
//! are mono IF. The `-c 6` channel selection used by `spawn_drm_bridge`
//! expects I/Q, so the decoded result when fed mono-IF-as-stereo-IQ is
//! not expected to match the reference decode exactly — but dream will
//! still attempt to decode and produce output, which is enough to
//! prove the subprocess + routing + capture chain is alive.

use std::path::PathBuf;
use std::time::Duration;

use efd_dsp::{spawn_drm_bridge, AudioBlock, DrmConfig};
use efd_iq::IqBlock;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn vendored_dream() -> PathBuf {
    repo_root().join("third_party/dream/build/install/bin/dream")
}

fn vor_sample() -> PathBuf {
    repo_root().join("third_party/dream/samples/VoiceOfRussia_ModeB_10kHz.flac")
}

async fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
}

#[tokio::test]
#[ignore]
async fn drm_bridge_decodes_vor_sample() {
    let dream = vendored_dream();
    if !dream.exists() {
        eprintln!(
            "skipping: vendored dream binary not found at {}. \
             Run `third_party/dream/build.sh` first.",
            dream.display()
        );
        return;
    }
    let sample = vor_sample();
    assert!(
        sample.exists(),
        "VoR sample missing at {}",
        sample.display()
    );
    for tool in ["pactl", "pacat", "parec", "paplay"] {
        assert!(
            tool_available(tool).await,
            "required tool `{tool}` not on PATH"
        );
    }

    // Unique sink names per test run so re-runs don't collide.
    let tag = std::process::id();
    let cfg = DrmConfig {
        dream_binary: dream.to_string_lossy().into(),
        input_sink: format!("efd_drm_test_in_{tag}"),
        output_sink: format!("efd_drm_test_out_{tag}"),
        iq_input_rate: 48_000,
        dream_rate: 48_000,
        ..Default::default()
    };

    // Unused IQ channel — we're pushing audio directly into the sink
    // below via paplay, bypassing the Rust IQ→pacat path.
    let (_iq_tx, iq_rx) = broadcast::channel::<std::sync::Arc<IqBlock>>(4);
    let (audio_tx, mut audio_rx) = mpsc::channel::<AudioBlock>(256);
    let cancel = CancellationToken::new();

    let bridge = spawn_drm_bridge(cfg.clone(), iq_rx, audio_tx, cancel.clone());

    // Give the bridge a moment to load sinks and start subprocesses.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Push the VoR FLAC into drm_in. paplay auto-converts to the sink's
    // rate/format; the fact it's mono vs our stereo sink is fine —
    // PipeWire broadcasts mono to both channels.
    let paplay = Command::new("paplay")
        .args([
            "--device",
            &cfg.input_sink,
            sample.to_string_lossy().as_ref(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("paplay");

    // Collect audio blocks for up to ~20 seconds.
    let collect = tokio::spawn(async move {
        let mut total_samples: usize = 0;
        let mut peak: f32 = 0.0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), audio_rx.recv()).await {
                Ok(Some(blk)) => {
                    total_samples += blk.samples.len();
                    for &s in &blk.samples {
                        let a = s.abs();
                        if a > peak {
                            peak = a;
                        }
                    }
                }
                Ok(None) => break, // channel closed
                Err(_) => break,   // timeout
            }
        }
        (total_samples, peak)
    });

    let _ = paplay.wait_with_output().await;
    // Let the bridge drain any tail audio.
    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel.cancel();
    let _ = bridge.await;
    let (total, peak) = collect.await.expect("collector");

    println!("DRM bridge: {total} samples collected, peak={peak:.3}");
    assert!(
        total > 48_000,
        "expected >1s of decoded audio, got {} samples",
        total
    );
    assert!(peak > 0.01, "expected non-silent audio, peak={peak}");
}
