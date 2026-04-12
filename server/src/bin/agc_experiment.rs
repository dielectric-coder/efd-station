// AGC vs IQ sample amplitude experiment.
//
// Captures IQ samples at each AGC threshold in AGC_VALUES, computes mean power
// E[I² + Q²] and peak |I + jQ|, and reports the dB ratio between the two
// settings — to determine whether the FDM-DUO's hardware AGC affects the IQ
// stream (it does not — the IQ tap is upstream of AGC).
//
// Prerequisites: the server and any other IQ/CAT consumers must be stopped
// (exclusive USB + CAT access). For a repeatable reading, disconnect the
// antenna or use a 50 Ω dummy load so the measurement is of the receiver's
// own noise floor.

use std::time::{Duration, Instant};

use efd_cat::{discover_serial_device, SerialPort};
use efd_iq::device::{convert_samples, FdmDuo, DEFAULT_SAMPLE_RATE, USB_BUFFER_SIZE};

const AGC_VALUES: [u8; 2] = [0, 10];
const CAPTURE_SECS: u64 = 10;
const SETTLE_MS: u64 = 500;
// USB buffers to discard after AGC change — clears stale FIFO data.
const DRAIN_READS: usize = 40;

#[derive(Default)]
struct Stats {
    n: u64,
    sum_i: f64,
    sum_q: f64,
    sum_sq: f64,
    peak_sq: f32,
}

impl Stats {
    fn accumulate(&mut self, samples: &[[f32; 2]]) {
        for &[i, q] in samples {
            let (ii, qq) = (i as f64, q as f64);
            self.sum_i += ii;
            self.sum_q += qq;
            self.sum_sq += ii * ii + qq * qq;
            let mag_sq = i * i + q * q;
            if mag_sq > self.peak_sq {
                self.peak_sq = mag_sq;
            }
        }
        self.n += samples.len() as u64;
    }

    fn n_f64(&self) -> f64 {
        self.n.max(1) as f64
    }
    fn mean_i(&self) -> f64 { self.sum_i / self.n_f64() }
    fn mean_q(&self) -> f64 { self.sum_q / self.n_f64() }
    fn mean_power(&self) -> f64 { self.sum_sq / self.n_f64() }
    fn peak(&self) -> f32 { self.peak_sq.sqrt() }
}

fn capture(dev: &FdmDuo, buf: &mut [u8], secs: u64) -> Stats {
    let mut stats = Stats::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if let Ok(n) = dev.bulk_read(buf) {
            if n > 0 {
                let samples = convert_samples(&buf[..n]);
                stats.accumulate(&samples);
            }
        }
    }
    stats
}

fn drain(dev: &FdmDuo, buf: &mut [u8], reads: usize) {
    for _ in 0..reads {
        let _ = dev.bulk_read(buf);
    }
}

fn print_row(agc: u8, s: &Stats) {
    println!(
        "AGC={agc:<2}  N={:>9}  E[I]={:+.3e}  E[Q]={:+.3e}  mean_power={:.6e}  RMS={:.6e}  peak={:.4}",
        s.n,
        s.mean_i(),
        s.mean_q(),
        s.mean_power(),
        s.mean_power().sqrt(),
        s.peak(),
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

    let mut buf = vec![0u8; USB_BUFFER_SIZE];
    drain(&dev, &mut buf, DRAIN_READS);

    println!("# AGC vs IQ amplitude");
    println!(
        "# capture={CAPTURE_SECS}s  sample_rate={DEFAULT_SAMPLE_RATE} Hz  agc_values={AGC_VALUES:?}"
    );
    println!();

    let mut results: Vec<(u8, Stats)> = Vec::with_capacity(AGC_VALUES.len());
    for &agc in &AGC_VALUES {
        let cmd = format!("TH{agc:02};");
        let resp = cat.command(&cmd)?;
        println!("# sent {cmd:<6} resp={resp:?}");
        std::thread::sleep(Duration::from_millis(SETTLE_MS));
        drain(&dev, &mut buf, DRAIN_READS);

        let s = capture(&dev, &mut buf, CAPTURE_SECS);
        print_row(agc, &s);
        results.push((agc, s));
    }

    let (a0, s0) = &results[0];
    let (a1, s1) = &results[1];
    let p0 = s0.mean_power();
    let p1 = s1.mean_power();
    if p0 > 0.0 && p1 > 0.0 {
        let power_db = 10.0 * (p1 / p0).log10();
        let peak_db = 20.0 * (s1.peak() / s0.peak()).log10();
        println!();
        println!("power(AGC={a1}) / power(AGC={a0}) = {power_db:+.2} dB");
        println!("peak(AGC={a1})  / peak(AGC={a0})  = {peak_db:+.2} dB");
    }

    dev.stop_streaming();
    Ok(())
}
