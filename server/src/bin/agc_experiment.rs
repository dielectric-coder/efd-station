// AGC vs IQ sample amplitude experiment.
//
// Measures whether the FDM-DUO hardware AGC threshold (TH command) affects the
// amplitude of captured IQ samples. Captures CAPTURE_SECS of IQ at each AGC
// value in AGC_VALUES, computes mean power E[I^2 + Q^2] and RMS, and prints
// the power ratio in dB between the two settings.
//
// Prerequisites: the server and any other IQ/CAT consumers must be stopped
// (exclusive access to the USB IQ endpoint and the CAT serial port).
// Tune the radio to a steady carrier before running.

use std::time::{Duration, Instant};

use efd_cat::{discover_serial_device, SerialPort};
use efd_iq::device::{convert_samples, FdmDuo, DEFAULT_SAMPLE_RATE, USB_BUFFER_SIZE};

const AGC_VALUES: &[u8] = &[0, 10];
const CAPTURE_SECS: u64 = 10;
const SETTLE_MS: u64 = 500;

#[derive(Default)]
struct Stats {
    n: u64,
    sum_i: f64,
    sum_q: f64,
    sum_sq: f64,
    peak: f32,
}

impl Stats {
    fn accumulate(&mut self, samples: &[[f32; 2]]) {
        for &[i, q] in samples {
            let (ii, qq) = (i as f64, q as f64);
            self.sum_i += ii;
            self.sum_q += qq;
            self.sum_sq += ii * ii + qq * qq;
            let m = i.abs().max(q.abs());
            if m > self.peak {
                self.peak = m;
            }
        }
        self.n += samples.len() as u64;
    }

    fn mean_power(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_sq / self.n as f64
        }
    }
}

fn capture(dev: &FdmDuo, secs: u64) -> Stats {
    let mut buf = vec![0u8; USB_BUFFER_SIZE];
    let mut stats = Stats::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        match dev.bulk_read(&mut buf) {
            Ok(n) if n > 0 => {
                let samples = convert_samples(&buf[..n]);
                stats.accumulate(&samples);
            }
            _ => continue,
        }
    }
    stats
}

fn drain(dev: &FdmDuo, reads: usize) {
    let mut buf = vec![0u8; USB_BUFFER_SIZE];
    for _ in 0..reads {
        let _ = dev.bulk_read(&mut buf);
    }
}

fn print_row(agc: u8, s: &Stats) {
    let n = s.n.max(1) as f64;
    let mean_power = s.mean_power();
    let rms = mean_power.sqrt();
    println!(
        "AGC={agc:<2}  N={:>9}  E[I]={:+.3e}  E[Q]={:+.3e}  mean_power={:.6e}  RMS={:.6e}  peak={:.4}",
        s.n,
        s.sum_i / n,
        s.sum_q / n,
        mean_power,
        rms,
        s.peak,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cat_device =
        discover_serial_device()?.ok_or("FDM-DUO CAT serial port not found")?;
    let cat = SerialPort::open(&cat_device)?;

    let dev = FdmDuo::open()?;
    dev.start_streaming()?;

    // Initial settle: discard early buffers that may contain stale FIFO data.
    drain(&dev, 40);

    println!("# AGC vs IQ amplitude");
    println!(
        "# capture={}s  sample_rate={} Hz  agc_values={:?}",
        CAPTURE_SECS, DEFAULT_SAMPLE_RATE, AGC_VALUES
    );
    println!();

    let mut results = Vec::with_capacity(AGC_VALUES.len());
    for &agc in AGC_VALUES {
        let cmd = format!("TH{agc:02};");
        let resp = cat.command(&cmd)?;
        println!("# sent {cmd:<6} resp={resp:?}");
        std::thread::sleep(Duration::from_millis(SETTLE_MS));
        drain(&dev, 40);

        let s = capture(&dev, CAPTURE_SECS);
        print_row(agc, &s);
        results.push((agc, s));
    }

    if let (Some((a0, s0)), Some((a1, s1))) = (results.first(), results.get(1)) {
        let p0 = s0.mean_power();
        let p1 = s1.mean_power();
        if p0 > 0.0 && p1 > 0.0 {
            let ratio = p1 / p0;
            let db = 10.0 * ratio.log10();
            println!();
            println!(
                "power(AGC={a1}) / power(AGC={a0}) = {ratio:.4}  ({db:+.2} dB)"
            );
            let rms_db = 20.0 * (s1.mean_power().sqrt() / s0.mean_power().sqrt()).log10();
            println!("RMS delta = {rms_db:+.2} dB");
        }
    }

    dev.stop_streaming();
    Ok(())
}
