//! 把图标和版本信息编进 exe 资源。
//!
//! 只用 MSVC 工具链自带的 rc.exe，不引入 winres 之类的依赖。
//! 找不到 rc.exe 时安静跳过——程序照样能编，只是 exe 没图标。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=resources/app.rc");
    println!("cargo:rerun-if-changed=assets/smartpaste.ico");
    println!("cargo:rerun-if-changed=resources/app.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let res = out_dir.join("app.res");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe 未找到，跳过图标资源");
        return;
    };

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let status = Command::new(&rc)
        .current_dir(&manifest)
        .arg("/nologo")
        .arg(format!("/fo{}", res.display()))
        .arg("resources/app.rc")
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        _ => println!("cargo:warning=rc.exe 编译资源失败，exe 将没有图标"),
    }
}

/// 扫 Windows Kits 目录，取版本号最大的 x64 rc.exe。
fn find_rc() -> Option<PathBuf> {
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut best: Option<(Vec<u32>, PathBuf)> = None;

    for entry in std::fs::read_dir(kits).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let parts: Vec<u32> = name.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() < 3 {
            continue;
        }
        for arch in ["x64", "arm64"] {
            let candidate = path.join(arch).join("rc.exe");
            if !candidate.is_file() {
                continue;
            }
            // x64 优先
            let mut key = parts.clone();
            key.push(if arch == "x64" { 2 } else { 1 });
            if best.as_ref().is_none_or(|(bk, _)| key > *bk) {
                best = Some((key, candidate));
            }
        }
    }

    best.map(|(_, p)| p)
}
