//! Nightly, non-blocking benchmarks (plan §12.2), `#[ignore]`d:
//! `cargo test --release -p cinelog-it -- --ignored --test-threads=1 bench_`, with
//! `CINELOG_IT_BIN` pointing at a release build (otherwise a debug recorder is measured).
//!
//! - B1: `cat` of 141 MB through a headless `cinelog rec`, median of 5, within 25% of the same
//!   `cat` through util-linux `script` (a pty plus a log file, timed in the same run, so the
//!   budget holds on any machine: the dev machine measured +6%). `CINELOG_IT_B1_BASELINE_MS`
//!   replaces the reference with a fixed baseline (then within 10%).
//! - B2: time to the first prompt of an interactive zsh on a pty, with and without cinelog
//!   (the kill switch), 20 runs each; the median difference must be ≤ 35 ms, and so must the
//!   p95 difference unless `CINELOG_IT_B2_P95_BUDGET_MS` allows more (shared CI runners have
//!   a long tail of slow process starts).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cinelog_it::cast::find_casts;
use cinelog_it::{Cast, Need, Sandbox, need, procs};

const B1_BYTES: u64 = 141_000_000;
const B2_RUNS: usize = 20;
const B1_RUNS: usize = 5;
/// cinelog's median may exceed `script`'s by this factor (it also parses and escapes output).
const B1_REFERENCE_FACTOR: f64 = 1.25;
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
    need!(Need::Script, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let data = sb.work.join("data.b64");
    let generate = format!(
        "head -c {} /dev/urandom | base64 > {}",
        B1_BYTES / 4 * 3,
        data.display()
    );
    assert!(Command::new("sh").args(["-c", &generate]).status().unwrap().success());
    let bin = sb.bin.clone().unwrap();
    let cat = format!("cat {}", data.display());
    let typescript = sb.work.join("typescript");
    let fixed: Option<f64> = std::env::var("CINELOG_IT_B1_BASELINE_MS")
        .ok()
        .and_then(|s| s.parse().ok());
    let (mut runs, mut reference) = (Vec::new(), Vec::new());
    // Interleaved, so a slow phase of a shared runner hits both sides alike.
    for _ in 0..B1_RUNS {
        let mut rec = Command::new(&bin);
        rec.env_clear()
            .envs(sb.env())
            .args(["rec", "--", "/bin/sh", "-c"])
            .arg(&cat);
        runs.push(timed_run(rec));
        if fixed.is_none() {
            let mut script = Command::new("script");
            script.env_clear().envs(sb.env()).arg("-qec").arg(&cat).arg(&typescript);
            reference.push(timed_run(script));
        }
    }
    let _ = std::fs::remove_file(&typescript);
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
    let m = median(runs.clone());
    let (limit, what) = match fixed {
        Some(b) => (b * 1.10, format!("baseline {b:.0} ms + 10%")),
        None => {
            let r = median(reference.clone());
            (
                r * B1_REFERENCE_FACTOR,
                format!("script's median {r:.0} ms (runs {reference:.0?}) x {B1_REFERENCE_FACTOR}"),
            )
        }
    };
    eprintln!("B1: runs {runs:.0?} ms, median {m:.0} ms, limit {limit:.0} ms ({what})");
    assert!(m <= limit, "B1 median {m:.0} ms exceeds {what}");
}

/// Wall time of one B1 run in ms; output goes to /dev/null, as with `cat > /dev/null`.
fn timed_run(mut cmd: Command) -> f64 {
    let start = Instant::now();
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
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
    assert!(status.success(), "{status:?}");
    start.elapsed().as_secs_f64() * 1000.0
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
    let added = median(with) - median(without);
    let p95_budget: f64 = std::env::var("CINELOG_IT_B2_P95_BUDGET_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(B2_BUDGET_MS);
    eprintln!(
        "B2: p95 without {a:.1} ms, with {b:.1} ms, added {:.1} ms (median added {added:.1} ms)",
        b - a
    );
    assert!(
        added <= B2_BUDGET_MS,
        "cinelog adds {added:.1} ms median (budget {B2_BUDGET_MS} ms)"
    );
    assert!(
        b - a <= p95_budget,
        "cinelog adds {:.1} ms p95 (budget {p95_budget} ms)",
        b - a
    );
}

#[test]
fn percentiles() {
    assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
    assert_eq!(p95((1..=20).map(f64::from).collect()), 19.0);
    assert_eq!(p95(vec![5.0]), 5.0);
}
