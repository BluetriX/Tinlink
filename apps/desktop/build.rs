use std::path::Path;

/// Web client code used unchanged by this UI: E2EE (docs/e2ee.md) and the
/// mention parser that must match it.
const SHARED: &[&str] = &["e2ee.js", "keyring.js", "logic.js"];

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
