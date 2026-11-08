//! Chooses the SPA directory embedded by `assets.rs`: `$CINELOG_WEB_DIST` if set, else
//! `web/dist` when it holds a build, else a stub page in `OUT_DIR`, so a plain
//! `cargo build` works without pnpm (§3.1). Exposes the choice as `CINELOG_WEB_DIST`
//! (absolute path) and, for the stub, `CINELOG_WEB_STUB=1`.

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
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let dist = manifest.join("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let chosen = match env::var_os("CINELOG_WEB_DIST").map(PathBuf::from) {
        Some(dir) if is_build(&dir) => Some(dir),
        Some(dir) => panic!("CINELOG_WEB_DIST={} has no index.html", dir.display()),
        None => is_build(&dist).then_some(dist),
    };
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

fn is_build(dir: &Path) -> bool {
    dir.join("index.html").is_file()
}
