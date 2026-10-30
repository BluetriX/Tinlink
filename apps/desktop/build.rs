use std::path::Path;

/// The web client's E2EE code, used unchanged by this UI (docs/e2ee.md).
const SHARED: &[&str] = &["e2ee.js", "keyring.js"];

fn main() {
    let web = Path::new("../web");
    let out = Path::new("ui/shared");
    std::fs::create_dir_all(out).expect("create ui/shared");
    for name in SHARED {
        let src = web.join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        std::fs::copy(&src, out.join(name)).expect("copy shared web code");
    }
    tauri_build::build();
}
