//! RED R3D support: when `FILMCRAFT_R3D_SDK` names an unpacked R3D SDK (RED's free download; its
//! headers and static library may not be redistributed, so they are never part of FilmCraft),
//! compile the bridge in `src/r3d/shim.cpp` against it, link RED's static library and enable
//! `cfg(filmcraft_r3d)`. Without it, nothing is compiled and R3D imports explain what is missing.

use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(filmcraft_r3d)");
    println!("cargo:rerun-if-env-changed=FILMCRAFT_R3D_SDK");
    println!("cargo:rerun-if-changed=src/r3d/shim.cpp");
    let Some(sdk) = std::env::var_os("FILMCRAFT_R3D_SDK").filter(|s| !s.is_empty()).map(PathBuf::from) else { return };
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let crt_static = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default().split(',').any(|f| f == "crt-static");
    let lib_dir = sdk.join("Lib");
    let (dir, lib) = match (os.as_str(), arch.as_str()) {
        ("linux", "x86_64") => ("linux64", "R3DSDKPIC-cpp11".to_string()),
        ("windows", "x86_64") => ("win64", format!("R3DSDK-2017{}", if crt_static { "MT" } else { "MD" })),
        ("windows", "aarch64") => ("winarm64", format!("R3DSDK-2022{}", if crt_static { "MT" } else { "MD" })),
        ("macos", _) => ("mac64", "R3DSDK-libcpp".to_string()),
        _ => {
            println!("cargo:warning=FILMCRAFT_R3D_SDK is set but the R3D SDK has no library for {os}/{arch}: R3D support left out");
            return;
        }
    };
    let include = sdk.join("Include");
    if !include.join("R3DSDK.h").is_file() {
        println!("cargo:warning=FILMCRAFT_R3D_SDK={} has no Include/R3DSDK.h: R3D support left out", sdk.display());
        return;
    }
    let mut b = cc::Build::new();
    b.cpp(true).file("src/r3d/shim.cpp").include(&include).warnings(false);
    if os == "windows" {
        b.flag("/EHsc");
    } else {
        b.flag("-std=c++11");
    }
    b.compile("filmcraft_r3d_shim");
    println!("cargo:rustc-link-search=native={}", lib_dir.join(dir).display());
    println!("cargo:rustc-link-lib=static={lib}");
    match os.as_str() {
        "linux" => {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            println!("cargo:rustc-link-lib=dylib=dl");
            println!("cargo:rustc-link-lib=dylib=uuid");
            println!("cargo:rustc-link-lib=dylib=pthread");
        }
        "macos" => {
            println!("cargo:rustc-link-lib=dylib=c++");
            println!("cargo:rustc-link-lib=framework=Metal");
        }
        _ => {}
    }
    println!("cargo:rustc-cfg=filmcraft_r3d");
}
