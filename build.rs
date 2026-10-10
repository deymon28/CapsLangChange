fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon-active.ico");
    println!("cargo:rerun-if-changed=assets/icon-paused.ico");
    println!("cargo:rerun-if-changed=assets/icon-excluded.ico");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let major = env("CARGO_PKG_VERSION_MAJOR");
    let minor = env("CARGO_PKG_VERSION_MINOR");
    let patch = env("CARGO_PKG_VERSION_PATCH");
    let version = env("CARGO_PKG_VERSION");
    let macros = [
        format!("VER_MAJOR={major}"),
        format!("VER_MINOR={minor}"),
        format!("VER_PATCH={patch}"),
        format!("VER_STRING=\"{version}\""),
    ];
    embed_resource::compile("assets/app.rc", &macros).manifest_required().expect("failed to compile Windows resources");
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}
