fn main() {
    println!("cargo:rerun-if-env-changed=XKEEN_UI_VERSION");
    let version = std::env::var("XKEEN_UI_VERSION")
        .unwrap_or_else(|_| format!("v{}", std::env::var("CARGO_PKG_VERSION").unwrap()));
    assert!(version.starts_with('v') && !version.contains(['\n', '\r']), "Invalid release version");
    println!("cargo:rustc-env=XKEEN_UI_VERSION={version}");
    let root = if std::env::var_os("CARGO_FEATURE_LOCAL_DEV").is_some() {
        std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .parent()
            .unwrap()
            .join(".local/opt")
            .to_str()
            .unwrap()
            .to_string()
    } else {
        "/opt".to_string()
    };
    println!("cargo:rustc-env=XKEEN_OPT_ROOT={root}");
    println!("cargo:rustc-env=BUILD_TARGET={}", std::env::var("TARGET").unwrap());
}
