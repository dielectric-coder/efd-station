//! Headless WebSocket test client for efd-station.
//!
//! Connects to the server, decodes messages, prints stats.
//! Usage: cargo run --example ws_test -- ws://pi-ip:8080/ws

use std::time::{Duration, Instant};

use efd_proto::{ClientMsg, CatCommand, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8080/ws".to_string());

    println!("connecting to {url}...");

    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS connect failed");

    println!("connected!");

    let (mut sink, mut stream) = ws.split();

    // Send a test CatCommand to verify upstream
    {
        let msg = ClientMsg::CatCommand(CatCommand {
            raw: "IF;".to_string(),
        });
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&msg, cfg).unwrap();
        sink.send(Message::Binary(bytes.into())).await.unwrap();
        println!("sent test command: IF;");
    }

    let start = Instant::now();
    let run_duration = Duration::from_secs(10);

    let mut fft_count: u64 = 0;
    let mut fft_bins_total: u64 = 0;
    let mut audio_count: u64 = 0;
    let mut state_count: u64 = 0;
    let mut last_state: Option<efd_proto::RadioState> = None;
    let mut error_count: u64 = 0;
    let mut decode_errors: u64 = 0;

    let cfg = bincode::config::standard();

    println!("receiving for {}s...\n", run_duration.as_secs());

    loop {
        if start.elapsed() >= run_duration {
            break;
        }

        let timeout = run_duration.saturating_sub(start.elapsed());
        let frame = tokio::select! {
            frame = stream.next() => frame,
            _ = tokio::time::sleep(timeout) => break,
        };

        let Some(frame) = frame else { break };

        let data = match frame {
            Ok(Message::Binary(data)) => data,
            Ok(Message::Close(_)) => {
                println!("server closed connection");
                break;
            }
            Ok(_) => continue,
            Err(e) => {
                println!("WS error: {e}");
                break;
            }
        };

        let msg: ServerMsg = match bincode::decode_from_slice(&data, cfg) {
            Ok((msg, _)) => msg,
            Err(e) => {
                decode_errors += 1;
                if decode_errors <= 3 {
                    println!("decode error: {e}");
                }
                continue;
            }
        };

        match msg {
            ServerMsg::FftBins(bins) => {
                fft_count += 1;
                fft_bins_total += bins.bins.len() as u64;
                if fft_count == 1 {
                    let peak = bins.bins.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                    let floor = bins.bins.iter().cloned().fold(f32::INFINITY, f32::min);
                    println!(
                        "first FftBins: {} bins, center={} Hz, span={} Hz, peak={:.1} dB, floor={:.1} dB",
                        bins.bins.len(),
                        bins.center_freq_hz,
                        bins.span_hz,
                        peak,
                        floor,
                    );
                }
            }
            ServerMsg::RadioState(state) => {
                state_count += 1;
                if state_count == 1 || state_count % 10 == 0 {
                    println!(
                        "RadioState: VFO {:?} freq={} Hz mode={:?} bw={} s={:.0} dB tx={}",
                        state.vfo,
                        state.freq_hz,
                        state.mode,
                        state.filter_bw,
                        state.s_meter_db,
                        state.tx,
                    );
                }
                last_state = Some(state);
            }
            ServerMsg::Audio(chunk) => {
                audio_count += 1;
                if audio_count == 1 {
                    println!(
                        "first AudioChunk: {} bytes opus, seq={}",
                        chunk.opus_data.len(),
                        chunk.seq,
                    );
                }
            }
            ServerMsg::Error(err) => {
                error_count += 1;
                println!("server error: [{}] {}", err.code, err.message);
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();

    println!("\n--- Summary ({:.1}s) ---", elapsed);
    println!("FftBins:    {} frames ({:.1}/s), {} bins/frame",
        fft_count,
        fft_count as f64 / elapsed,
        if fft_count > 0 { fft_bins_total / fft_count } else { 0 },
    );
    println!("RadioState: {} updates ({:.1}/s)", state_count, state_count as f64 / elapsed);
    println!("Audio:      {} chunks ({:.1}/s)", audio_count, audio_count as f64 / elapsed);
    if error_count > 0 {
        println!("Errors:     {}", error_count);
    }
    if decode_errors > 0 {
        println!("Decode errs: {}", decode_errors);
    }
    if let Some(s) = last_state {
        println!("Last state: {:?} {} Hz {:?} bw={} s={:.0} dB",
            s.vfo, s.freq_hz, s.mode, s.filter_bw, s.s_meter_db);
    }
}
