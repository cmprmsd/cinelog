//! Chooses the SPA directory embedded by `assets.rs`: `$CINELOG_WEB_DIST` if set, else
//! `web/dist` when it holds a build, else a stub page in `OUT_DIR`, so a plain
//! `cargo build` works without pnpm (§3.1). Exposes the choice as `CINELOG_WEB_DIST`
//! (absolute path) and, for the stub, `CINELOG_WEB_STUB=1`.
//!
//! A relative `CINELOG_WEB_DIST` is relative to the workspace root (cargo runs build
//! scripts in the package directory, not where `cargo` was started), so
//! `CINELOG_WEB_DIST=web/dist cargo build` works from the repository root.

use std::path::{Path, PathBuf};
use std::{env, fs};

const STUB: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>cinelog</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem}
code{background:#8882;padding:0 .3em;border-radius:3px}</style>
</head>
<body>
<h1>cinelog</h1>
<p>This binary was built without the web UI. The API under <code>/api</code> works.</p>
<p>To build the UI: <code>cd web &amp;&amp; pnpm install --frozen-lockfile &amp;&amp; pnpm build</code>,
then rebuild cinelog.</p>
</body>
</html>
"#;

fn main() {
    println!("cargo:rerun-if-env-changed=CINELOG_WEB_DIST");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let workspace = manifest.join("../..");
    let dist = workspace.join("web/dist");

    let chosen = match env::var_os("CINELOG_WEB_DIST").map(|d| workspace.join(d)) {
        Some(dir) if is_build(&dir) => Some(dir),
        Some(dir) => panic!(
            "CINELOG_WEB_DIST: {} has no index.html (a relative path is taken from the workspace root)",
            dir.display()
        ),
        None => is_build(&dist).then_some(dist.clone()),
    };
    watch(chosen.as_deref().unwrap_or(&dist));
    let dir = match chosen {
        Some(dir) => dir.canonicalize().expect("web dist directory is readable"),
        None => {
            let stub = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo")).join("web-stub");
            fs::create_dir_all(&stub).expect("create the stub directory");
            fs::write(stub.join("index.html"), STUB).expect("write the stub page");
            println!("cargo:rustc-env=CINELOG_WEB_STUB=1");
            stub
        }
    };
    println!("cargo:rustc-env=CINELOG_WEB_DIST={}", dir.display());
}

/// Reruns when `dir` changes. Cargo treats a missing path as always stale (a rebuild on
/// every build), so a missing dist is watched through its parent (`web/`, which changes
/// when the dist appears); without either only `build.rs` and the env are watched.
fn watch(dir: &Path) {
    if let Some(d) = [Some(dir), dir.parent()].into_iter().flatten().find(|d| d.is_dir()) {
        println!("cargo:rerun-if-changed={}", d.display());
    }
}

fn is_build(dir: &Path) -> bool {
    dir.join("index.html").is_file()
}
