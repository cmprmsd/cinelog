//! Nightly, non-blocking benchmarks (plan §12.2), `#[ignore]`d:
//! `cargo test --release -p cinelog-it -- --ignored --test-threads=1 bench_`, with
//! `CINELOG_IT_BIN` pointing at a release build (otherwise a debug recorder is measured).
//!
//! - B1: `cat` of 141 MB through a headless `cinelog rec`, median of 3, within 10% of the
//!   baseline `CINELOG_IT_B1_BASELINE_MS` (default 990, the [rec] spike).
//! - B2: time to the first prompt of an interactive zsh on a pty, with and without cinelog
//!   (the kill switch), 20 runs each; the p95 difference must be ≤ 35 ms.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cinelog_it::cast::find_casts;
use cinelog_it::{Cast, Need, Sandbox, need, procs};

const B1_BYTES: u64 = 141_000_000;
const B2_RUNS: usize = 20;
const B2_BUDGET_MS: f64 = 35.0;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn p95(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let idx = ((v.len() as f64) * 0.95).ceil() as usize;
    v[idx.clamp(1, v.len()) - 1]
}

#[test]
#[ignore = "nightly benchmark (B1)"]
fn bench_b1_throughput() {
    need!(Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let data = sb.work.join("data.b64");
    let generate = format!(
        "head -c {} /dev/urandom | base64 > {}",
        B1_BYTES / 4 * 3,
        data.display()
    );
    assert!(Command::new("sh").args(["-c", &generate]).status().unwrap().success());
    let bin = sb.bin.clone().unwrap();
    let mut runs = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let mut child = Command::new(&bin)
            .env_clear()
            .envs(sb.env())
            .args(["rec", "--", "/bin/sh", "-c"])
            .arg(format!("cat {}", data.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st;
            }
            if start.elapsed() > Duration::from_secs(120) {
                let _ = child.kill();
                panic!("B1 run did not finish within 120 s");
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        runs.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(status.success(), "{status:?}");
    }
    for p in find_casts(&sb.logs) {
        let last = std::fs::read(&p).unwrap();
        let tail = String::from_utf8_lossy(&last[last.len().saturating_sub(200)..]).into_owned();
        assert!(
            tail.trim_end().ends_with("\"x\", \"0\"]"),
            "{} ends with x: {tail}",
            p.display()
        );
        std::fs::remove_file(p).unwrap();
    }
    let baseline: f64 = std::env::var("CINELOG_IT_B1_BASELINE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(990.0);
    let m = median(runs.clone());
    eprintln!("B1: runs {runs:.0?} ms, median {m:.0} ms, baseline {baseline:.0} ms");
    assert!(
        m <= baseline * 1.10,
        "B1 median {m:.0} ms exceeds baseline {baseline:.0} ms + 10%"
    );
}

/// Time from spawning `zsh -i` on a fresh pty (`script`) to the first prompt on it.
fn time_to_prompt(sb: &Sandbox, extra: &[(&str, &str)]) -> f64 {
    let mut child = Command::new("script")
        .env_clear()
        .envs(sb.env())
        .envs(extra.iter().copied())
        .args(["-qfec", &format!("{} -i", sb.zsh.display()), "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if cinelog_it::cast::memmem(&seen, b"clit%") {
                let _ = tx.send(());
                break;
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("a prompt within 10 s");
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let _ = child.kill();
    let _ = child.wait();
    procs::terminate(&sb.processes(), Duration::from_millis(500));
    ms
}

#[test]
#[ignore = "nightly benchmark (B2)"]
fn bench_b2_startup() {
    need!(Need::Zsh, Need::Script, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    // "Without cinelog" is the same .zshrc minus the `source` line: the plugin's own load and
    // parse time counts towards the budget (the CINELOG_DISABLE switch would still pay it).
    let baseline = sb.baseline_zdotdir().unwrap();
    let baseline = baseline.to_string_lossy();
    let plain: &[(&str, &str)] = &[("ZDOTDIR", &baseline)];
    // Warm the page cache for both paths first.
    time_to_prompt(&sb, plain);
    time_to_prompt(&sb, &[]);
    let (mut without, mut with) = (Vec::new(), Vec::new());
    for _ in 0..B2_RUNS {
        without.push(time_to_prompt(&sb, plain));
        with.push(time_to_prompt(&sb, &[]));
    }
    let recorded = find_casts(&sb.logs)
        .iter()
        .filter(|p| Cast::load(p).is_ok_and(|c| c.session().is_some()))
        .count();
    assert!(
        recorded > B2_RUNS,
        "the measured shells were recorded ({recorded} casts)"
    );
    let (a, b) = (p95(without.clone()), p95(with.clone()));
    eprintln!(
        "B2: p95 without {a:.1} ms, with {b:.1} ms, added {:.1} ms (median added {:.1} ms)",
        b - a,
        median(with) - median(without)
    );
    assert!(
        b - a <= B2_BUDGET_MS,
        "cinelog adds {:.1} ms p95 (budget {B2_BUDGET_MS} ms)",
        b - a
    );
}

#[test]
fn percentiles() {
    assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
    assert_eq!(p95((1..=20).map(f64::from).collect()), 19.0);
    assert_eq!(p95(vec![5.0]), 5.0);
}
