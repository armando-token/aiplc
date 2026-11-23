use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    let platform_dir = match (target_os.as_str(), target_arch.as_str()) {
        ("macos", "aarch64") => "darwin-arm64",
        ("linux", "arm") => "linux-armv7",
        ("linux", "aarch64") => "linux-aarch64",
        other => panic!("Unsupported target for prebuilt WAMR/QuickJS: {other:?}"),
    };

    let lib_dir = root_dir.join("third_party/prebuilt").join(platform_dir);
    let wamr_inc = root_dir.join("third_party/wamr-2.4.6/core/iwasm/include");
    let qjs_inc = root_dir.join("third_party/quickjs-ng-v0.18.0");
    let shim_c = manifest_dir.join("csrc/wamr_qjs_shim.c");

    println!("cargo:rerun-if-changed={}", shim_c.display());
    println!("cargo:rerun-if-changed={}", lib_dir.display());

    cc::Build::new()
        .file(&shim_c)
        .include(&wamr_inc)
        .include(&qjs_inc)
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Wno-unused-parameter")
        .compile("wamr_qjs_shim");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=vmlib");
    println!("cargo:rustc-link-lib=static=quickjs");

    if target_os == "linux" {
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=pthread");
    }
}
