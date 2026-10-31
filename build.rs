use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/app.ico");
    println!("cargo:rerun-if-changed=res/app.manifest");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app_res.o");
    let res_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("res");
    let windres = env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let ok = Command::new(&windres)
        .current_dir(&res_dir)
        .args(["app.rc", "-O", "coff", "-o"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning=`{windres}` not found or failed: building without the icon/manifest resources (set WINDRES to a windres binary to include them)");
    }
}
