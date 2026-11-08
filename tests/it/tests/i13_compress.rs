//! I13: compression of aged casts (`cinelog compress --now`, plan §6.6). Search results,
//! `show`, the indexed content and every playback slice are unchanged; the `.cast.zst`
//! consists of independently decodable frames cut at newlines that reproduce the original
//! byte for byte; fresh casts and legacy casts are never touched. (Export is an M4 feature.)

#[path = "../../fixtures/legacy-gen/legacy_gen.rs"]
mod legacy_gen;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use cinelog_it::index::{IndexDb, STORAGE_COLUMNS};
use cinelog_it::synth::{CorpusSpec, Rng, SynthCast, corpus};
use cinelog_it::util::{now_us, set_times, sha256_file};
use cinelog_it::{Need, Sandbox, SandboxOptions, Serve, need, scaled, wait_until};

const DAY_US: u64 = 86_400_000_000;

/// size, mtime (s, ns), atime (s, ns), inode and sha256 of a file.
type Stamp = (u64, i64, i64, i64, i64, u64, String);

fn stamp(p: &Path) -> Stamp {
    let m = fs::symlink_metadata(p).unwrap();
    (
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.atime(),
        m.atime_nsec(),
        m.ino(),
        sha256_file(p).unwrap(),
    )
}

fn listing(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.insert(p);
        }
    }
    out
}

/// Legacy casts (old plugin layout, flat in `logs/`), aged 40 days, atime included, so a read
/// without `O_NOATIME` would show up.
fn write_legacy(logs: &Path) -> Vec<PathBuf> {
    let old = now_us() - 40 * DAY_US;
    let mut files = Vec::new();
    for (name, l) in legacy_gen::corpus()
        .into_iter()
        .filter(|(n, _)| ["plain.cast", "p10k.cast"].contains(n))
    {
        let cast = l.write(logs, name).unwrap();
        let log = logs.join(format!("{name}.commands.log"));
        for p in [cast, log] {
            set_times(&p, old, old).unwrap();
            files.push(p);
        }
    }
    files
}

/// `(entry, kind, typed, snippet)` of every hit of `cinelog search --json -a <term>`.
fn search(sb: &Sandbox, term: &str) -> Vec<String> {
    sb.search_json(&["-a", term])
        .unwrap()
        .iter()
        .map(|h| format!("{} {} {} {}", h["entry"], h["kind"], h["typed"], h["snippet"]))
        .collect()
}

fn show(sb: &Sandbox, entry: &str) -> String {
    sb.cinelog(&["show", entry]).unwrap().expect_ok().unwrap().stdout
}

struct Frame {
    c_offset: usize,
    c_len: usize,
    u_offset: usize,
    u_len: usize,
}

fn frames(db: &IndexDb, uuid: &str) -> Vec<Frame> {
    db.rows(
        "SELECT f.c_offset, f.c_len, f.u_offset, f.u_len FROM zst_frames f JOIN sessions s ON s.id = f.session_id \
         WHERE s.uuid = ?1 AND f.part = 1 ORDER BY f.seq",
        [uuid],
    )
    .unwrap()
    .into_iter()
    .map(|r| {
        let n = |i: usize| r[i].parse::<usize>().unwrap();
        Frame {
            c_offset: n(0),
            c_len: n(1),
            u_offset: n(2),
            u_len: n(3),
        }
    })
    .collect()
}

/// The frame table covers the file without gaps, every frame decodes on its own to the
/// matching original bytes and ends at a newline, and every playback segment's slice read
/// through its frame equals the original from that offset.
fn check_frames(db: &IndexDb, uuid: &str, zst: &[u8], original: &[u8]) -> usize {
    let fr = frames(db, uuid);
    assert!(!fr.is_empty(), "{uuid}: no frame table");
    let (mut c, mut u) = (0, 0);
    for f in &fr {
        assert_eq!((f.c_offset, f.u_offset), (c, u), "{uuid}: contiguous frames");
        let plain = zstd::decode_all(&zst[f.c_offset..f.c_offset + f.c_len]).unwrap();
        assert_eq!(plain.len(), f.u_len);
        assert!(
            plain == original[f.u_offset..f.u_offset + f.u_len],
            "{uuid}: frame at {}",
            f.u_offset
        );
        assert_eq!(plain.last(), Some(&b'\n'), "{uuid}: frames are cut at newlines");
        c += f.c_len;
        u += f.u_len;
    }
    assert_eq!(
        (c, u),
        (zst.len(), original.len()),
        "{uuid}: the frames cover both files"
    );

    let offsets = db
        .rows(
            "SELECT g.byte_offset FROM segments g JOIN sessions s ON s.id = g.session_id \
             WHERE s.uuid = ?1 AND g.part = 1 ORDER BY g.seq",
            [uuid],
        )
        .unwrap();
    assert!(!offsets.is_empty(), "{uuid}: playback segments");
    for o in offsets {
        let off: usize = o[0].parse().unwrap();
        let f = fr
            .iter()
            .find(|f| (f.u_offset..f.u_offset + f.u_len).contains(&off))
            .unwrap_or_else(|| panic!("{uuid}: segment offset {off} outside every frame"));
        let plain = zstd::decode_all(&zst[f.c_offset..f.c_offset + f.c_len]).unwrap();
        assert!(
            plain[off - f.u_offset..] == original[off..f.u_offset + f.u_len],
            "{uuid}: playback slice at {off}"
        );
        assert!(
            off == 0 || original[off - 1] == b'\n',
            "{uuid}: segment {off} starts a line"
        );
    }
    fr.len()
}

fn session_storage(db: &IndexDb, uuid: &str) -> (String, String, String) {
    let r = db
        .strings(
            "SELECT s.cast_path, s.compressed, p.compressed FROM sessions s \
             JOIN cast_parts p ON p.session_id = s.id AND p.part = 1 WHERE s.uuid = ?1",
            [uuid],
        )
        .unwrap();
    let r = &r[0];
    (r[0].clone().unwrap(), r[1].clone().unwrap(), r[2].clone().unwrap())
}

fn zst_of(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.zst", p.display()))
}

fn tmp_leftovers(logs: &Path) -> Vec<PathBuf> {
    listing(logs)
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".tmp"))
        .collect()
}

/// `compress_after_days`: not the default (14), so the "fresh" cast below would be a
/// candidate under the default and is not one only because the config is honoured.
const AFTER_DAYS: u64 = 25;

#[test]
fn i13_compress_now_keeps_search_slices_and_legacy_intact() {
    need!(Need::Index);
    let sb = Sandbox::with(SandboxOptions {
        extra: vec![format!("[retention]\ncompress_after_days = {AFTER_DAYS}")],
        ..Default::default()
    })
    .unwrap();
    // Three aged casts (the first about 14 MB, so it spans at least three 4 MiB frames) and
    // a younger one.
    let aged = corpus(
        &sb.logs,
        &CorpusSpec {
            seed: 13,
            casts: 3,
            commands: 100,
            max_lines: 280,
            newest_start_us: now_us() - 30 * DAY_US,
        },
    )
    .unwrap();
    let mut fresh = SynthCast::new(now_us() - (AFTER_DAYS - 5) * DAY_US, 120, 40);
    fresh.command("echo fresh-cast", "fresh-cast\n", 0, 10_000);
    let (fresh_path, _) = fresh.write(&sb.logs, 0).unwrap();
    let legacy = write_legacy(&sb.logs);
    let legacy_before: Vec<_> = legacy.iter().map(|p| stamp(p)).collect();
    let fresh_before = stamp(&fresh_path);
    let originals: Vec<(PathBuf, String, Vec<u8>)> = aged
        .iter()
        .map(|(p, c)| (p.clone(), c.uuid.clone(), fs::read(p).unwrap()))
        .collect();

    sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    let db_path = sb.index_db();
    let before = IndexDb::open(&db_path).unwrap().dump(STORAGE_COLUMNS).unwrap();
    let terms = [
        "end of c0k3",
        "c1k20",
        "frame 2 of",
        "naïve",
        "fresh-cast",
        "git status",
    ];
    let searches: Vec<Vec<String>> = terms.iter().map(|t| search(&sb, t)).collect();
    assert!(searches.iter().all(|s| !s.is_empty()), "{searches:?}");
    let entries: Vec<String> = aged
        .iter()
        .flat_map(|(_, c)| {
            [1, c.commands.len() as u64 / 2].map(|i| format!("{}:{}", c.uuid, c.commands[i as usize].seq))
        })
        .collect();
    let shows: Vec<String> = entries.iter().map(|e| show(&sb, e)).collect();

    // --dry-run lists exactly the aged cinelog casts.
    let dry = sb.cinelog(&["compress", "--dry-run"]).unwrap().expect_ok().unwrap();
    let listed: BTreeSet<PathBuf> = dry
        .stdout
        .lines()
        .filter_map(|l| l.split('\t').next().filter(|_| l.contains('\t')).map(PathBuf::from))
        .collect();
    let want: BTreeSet<PathBuf> = aged.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(listed, want, "{dry}");
    assert!(
        dry.stdout.contains("3 casts, ") && dry.stdout.contains(&format!("older than {AFTER_DAYS} days")),
        "{dry}"
    );
    assert!(aged.iter().all(|(p, _)| p.is_file()), "--dry-run compresses nothing");

    let run = sb.cinelog(&["compress", "--now"]).unwrap().expect_ok().unwrap();
    assert!(run.stdout.contains("compressed 3 of 3 casts"), "{run}");
    assert!(tmp_leftovers(&sb.logs).is_empty(), "{:?}", tmp_leftovers(&sb.logs));

    let db = IndexDb::open(&db_path).unwrap();
    let mut frame_counts = Vec::new();
    for (path, uuid, original) in &originals {
        assert!(!path.exists(), "{}: the original is removed", path.display());
        let zst = fs::read(zst_of(path)).unwrap();
        assert!(zst.len() * 4 < original.len(), "{}: compresses", path.display());
        assert!(
            zstd::decode_all(&zst[..]).unwrap() == *original,
            "{}: round trip",
            path.display()
        );
        let (cast_path, s_comp, p_comp) = session_storage(&db, uuid);
        assert_eq!(PathBuf::from(cast_path), zst_of(path));
        assert_eq!((s_comp.as_str(), p_comp.as_str()), ("1", "1"));
        frame_counts.push(check_frames(&db, uuid, &zst, original));
    }
    assert!(
        frame_counts[0] >= 3,
        "the large cast ({} bytes) spans at least three frames: {frame_counts:?}",
        originals[0].2.len()
    );
    // Only legacy casts must be read with O_NOATIME (§11), so the fresh cast's atime may move.
    let unchanged = |a: &Stamp, b: &Stamp| (a.0, a.1, a.2, a.5, &a.6) == (b.0, b.1, b.2, b.5, &b.6);
    assert!(
        unchanged(&stamp(&fresh_path), &fresh_before),
        "the fresh cast is untouched"
    );
    let legacy_after: Vec<_> = legacy.iter().map(|p| stamp(p)).collect();
    assert_eq!(
        legacy_after, legacy_before,
        "legacy casts are never compressed or touched"
    );

    let after = db.dump(STORAGE_COLUMNS).unwrap();
    if let Some(d) = after.diff(&before, 5) {
        panic!("compression changed indexed content:\n{d}");
    }
    let frames_compressed = db.dump(&[]).unwrap().tables["zst_frames"].clone();
    drop(db);
    for (t, s) in terms.iter().zip(&searches) {
        assert_eq!(&search(&sb, t), s, "search {t:?} after compression");
    }
    for (e, s) in entries.iter().zip(&shows) {
        // `show` names the cast, which is now the `.cast.zst`.
        let now = show(&sb, e);
        assert!(now.contains(".cast.zst · "), "{now}");
        assert_eq!(
            &now.replace(".cast.zst · ", ".cast · "),
            s,
            "show {e} after compression"
        );
    }

    // Nothing left to do; and a rebuild reads the compressed casts transparently.
    let again = sb.cinelog(&["compress", "--now"]).unwrap().expect_ok().unwrap();
    assert!(again.stdout.contains("compressed 0 of 0 casts"), "{again}");
    sb.cinelog(&["index", "--rebuild"]).unwrap().expect_ok().unwrap();
    let db = IndexDb::open(&db_path).unwrap();
    if let Some(d) = db.dump(STORAGE_COLUMNS).unwrap().diff(&before, 5) {
        panic!("index --rebuild over compressed casts differs:\n{d}");
    }
    for (path, uuid, original) in &originals {
        let (cast_path, s_comp, _) = session_storage(&db, uuid);
        assert_eq!((PathBuf::from(cast_path), s_comp.as_str()), (zst_of(path), "1"));
        check_frames(&db, uuid, &fs::read(zst_of(path)).unwrap(), original);
    }
    assert_eq!(
        db.dump(&[]).unwrap().tables["zst_frames"],
        frames_compressed,
        "the rebuilt frame table equals the compressor's"
    );
    for (t, s) in terms.iter().zip(&searches) {
        assert_eq!(&search(&sb, t), s, "search {t:?} after the rebuild");
    }
    let legacy_after: Vec<_> = legacy.iter().map(|p| stamp(p)).collect();
    assert_eq!(legacy_after, legacy_before, "legacy casts untouched by the rebuild");
}

#[test]
fn i13_compress_now_asks_the_running_server_and_startup_repairs_leftovers() {
    need!(Need::Index);
    let sb = Sandbox::new().unwrap();
    let mut rng = Rng::new(131);
    let mut old = SynthCast::new(now_us() - 20 * DAY_US, 120, 40);
    for k in 0..20 {
        old.command(&format!("cat log-{k}"), &rng.lines(200, 100), 0, 100_000);
    }
    old.command("echo served-needle", "served-needle\n", 0, 1_000);
    let (path, old) = old.write(&sb.logs, 0).unwrap();
    let original = fs::read(&path).unwrap();
    let legacy = write_legacy(&sb.logs);
    let legacy_before: Vec<_> = legacy.iter().map(|p| stamp(p)).collect();

    let serve = Serve::start(&sb, "serve", &[]).unwrap();
    let uuid = old.uuid.clone();
    sb.wait_hit(&["served-needle"], scaled(60.0), |h| h["session_uuid"] == uuid.as_str())
        .unwrap_or_else(|e| panic!("{e:#}\n{}", serve.log_text()));
    let ask = sb.cinelog(&["compress", "--now"]).unwrap().expect_ok().unwrap();
    assert!(
        ask.stdout
            .contains(&format!("asked the running server (pid {})", serve.pid())),
        "{ask}"
    );
    wait_until("the server to compress the aged cast", scaled(60.0), || {
        (!path.exists() && zst_of(&path).is_file()).then_some(())
    })
    .unwrap_or_else(|e| panic!("{e:#}\n{}", serve.log_text()));
    assert!(zstd::decode_all(&fs::read(zst_of(&path)).unwrap()[..]).unwrap() == original);
    sb.wait_hit(&["-a", "served-needle"], scaled(10.0), |h| {
        h["session_uuid"] == uuid.as_str() && h["kind"] == "output"
    })
    .unwrap();
    assert!(serve.stop(scaled(15.0)).unwrap().success());

    // A crash between the rename to .cast.zst and the unlink leaves both files: the next
    // start verifies the zst and removes the redundant original.
    fs::write(&path, &original).unwrap();
    let zst_before = stamp(&zst_of(&path));
    let serve = Serve::start(&sb, "serve-2", &[]).unwrap();
    wait_until("startup to remove the redundant original", scaled(60.0), || {
        (!path.exists()).then_some(())
    })
    .unwrap_or_else(|e| panic!("{e:#}\n{}", serve.log_text()));
    assert_eq!(stamp(&zst_of(&path)).6, zst_before.6, "the zst is kept as it was");
    sb.wait_hit(&["served-needle"], scaled(10.0), |h| h["session_uuid"] == uuid.as_str())
        .unwrap();
    assert!(serve.stop(scaled(15.0)).unwrap().success());
    assert!(tmp_leftovers(&sb.logs).is_empty());
    let legacy_after: Vec<_> = legacy.iter().map(|p| stamp(p)).collect();
    assert_eq!(legacy_after, legacy_before, "legacy casts untouched by serve");
}
