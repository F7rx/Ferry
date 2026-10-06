//! Loopback benchmark: two engines in one process, real TLS sockets, real
//! files on disk. Reports throughput and peak process memory per scenario.
//!
//!   cargo run --release -p ferry-core --example bench -- [scenarios...]
//!
//! Scenarios: 1m 100m 1g 10g small (10,000 × 4 KiB). Default: 1m 100m 1g small.
//! Writes a Markdown table to stdout.

use ferry_core::events::EngineEvent;
use ferry_core::model::{Decision, Direction, Protocol, TransferState};
use ferry_core::{Engine, EngineConfig, SendItem, Settings, Target};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MB: u64 = 1_000_000;

struct Scenario {
    name: &'static str,
    files: u64,
    size: u64,
}

fn scenario(name: &str) -> Option<Scenario> {
    Some(match name {
        "1m" => Scenario { name: "1 MB", files: 1, size: MB },
        "100m" => Scenario { name: "100 MB", files: 1, size: 100 * MB },
        "1g" => Scenario { name: "1 GB", files: 1, size: 1000 * MB },
        "10g" => Scenario { name: "10 GB", files: 1, size: 10_000 * MB },
        "small" => Scenario { name: "10,000 × 4 KiB", files: 10_000, size: 4096 },
        _ => return None,
    })
}

fn fill(path: &Path, size: u64, seed: u64) {
    let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path).unwrap());
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    // Only as much pattern as the file needs (10,000 × 4 KiB must not cost
    // 10,000 × 1 MiB of generation).
    let mut buf = vec![0u8; (size as usize).clamp(8, 1 << 20).next_multiple_of(8)];
    let mut left = size;
    while left > 0 {
        for chunk in buf.chunks_exact_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes());
        }
        let n = left.min(buf.len() as u64) as usize;
        f.write_all(&buf[..n]).unwrap();
        left -= n as u64;
    }
}

#[cfg(windows)]
fn rss_bytes() -> u64 {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct Counters {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
    }
    let mut c: Counters = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<Counters>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    // Private bytes: what the process itself allocated (excludes the file cache).
    c.PagefileUsage as u64
}

#[cfg(not(windows))]
fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

async fn engine(alias: &str, save: Option<&Path>) -> Arc<Engine> {
    let mut s = Settings { alias: alias.into(), port: 0, history_enabled: false, ..Settings::default() };
    if let Some(dir) = save {
        s.save_dir = Some(dir.to_path_buf());
    }
    Engine::start(EngineConfig::ephemeral(s)).await.unwrap()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let names = if args.is_empty() { vec!["1m".into(), "100m".into(), "1g".into(), "small".into()] } else { args };
    let root = std::env::var("FERRY_BENCH_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("ferry-bench"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    println!("| Scenario | Files | Total | Time | Throughput | Peak private memory | Result |");
    println!("|---|---:|---:|---:|---:|---:|---|");
    for name in names {
        let Some(sc) = scenario(&name) else {
            eprintln!("unknown scenario {name}");
            continue;
        };
        let src = root.join(format!("src-{name}"));
        let dst = root.join(format!("dst-{name}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        let items: Vec<SendItem> = if sc.files == 1 {
            let p = src.join("data.bin");
            fill(&p, sc.size, 1);
            vec![SendItem::Path { path: p }]
        } else {
            let dir = src.join("many");
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..sc.files {
                fill(&dir.join(format!("f{i:05}.bin")), sc.size, i + 7);
            }
            vec![SendItem::Path { path: dir }]
        };

        let rx = engine("Bench RX", Some(&dst)).await;
        let tx = engine("Bench TX", None).await;
        {
            let rx2 = rx.clone();
            let mut ev = rx.subscribe();
            tokio::spawn(async move {
                while let Ok(e) = ev.recv().await {
                    if let EngineEvent::IncomingRequest { request } = e {
                        rx2.respond(&request.id, Decision::accept_all());
                    }
                }
            });
        }
        let base = rss_bytes();
        let peak = Arc::new(AtomicU64::new(base));
        let sampler = {
            let peak = peak.clone();
            tokio::spawn(async move {
                loop {
                    peak.fetch_max(rss_bytes(), Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
        };
        let target =
            Target::Address { host: "127.0.0.1".into(), port: rx.port(), protocol: Protocol::Https, fingerprint: Some(rx.fingerprint()) };
        let started = Instant::now();
        let ids = tx.send(vec![target], items).await.unwrap();
        let mut last_report = Instant::now();
        let result = loop {
            if let Some(t) = tx.transfers().into_iter().find(|t| t.id == ids[0] && t.state.is_final()) {
                break t;
            }
            if std::env::var("FERRY_BENCH_TRACE").is_ok() && last_report.elapsed() > Duration::from_secs(2) {
                last_report = Instant::now();
                for t in tx.transfers().into_iter().chain(rx.transfers()) {
                    eprintln!("[trace] {:?} {:?} {}/{} files, {} bytes", t.direction, t.state, t.files_done, t.file_count, t.bytes_done);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        // Wait for the receiver to finish committing too.
        loop {
            if rx.transfers().iter().any(|t| t.direction == Direction::Receive && t.state.is_final()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let elapsed = started.elapsed();
        sampler.abort();
        let total = sc.files * sc.size;
        let mbps = total as f64 / elapsed.as_secs_f64() / 1e6;
        println!(
            "| {} | {} | {} | {:.2} s | {:.0} MB/s | {} MB (+{} MB) | {} |",
            sc.name,
            sc.files,
            ferry_core::util::format_bytes(total),
            elapsed.as_secs_f64(),
            mbps,
            peak.load(Ordering::Relaxed) / 1_000_000,
            peak.load(Ordering::Relaxed).saturating_sub(base) / 1_000_000,
            if result.state == TransferState::Completed { "ok".to_string() } else { format!("{:?} {:?}", result.state, result.error) }
        );
        tx.shutdown().await;
        rx.shutdown().await;
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
