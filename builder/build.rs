//! Embed the application icon and an asInvoker/DPI-aware manifest when a Windows resource compiler exists.
//! Lives in builder/ (see `build = "builder/build.rs"` in Cargo.toml). Cargo runs it from the package root.
use std::cmp::Reverse;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(tool: impl AsRef<OsStr>, args: &[String]) -> bool {
    Command::new(tool).args(args).status().map(|s| s.success()).unwrap_or(false)
}

/// rc.exe is part of the Windows SDK and is normally not on PATH outside a developer prompt.
/// Look for the newest installed SDK so a plain terminal / build.bat works too.
fn find_sdk_rc() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(v) = env::var_os(var) { roots.push(PathBuf::from(v).join("Windows Kits").join("10").join("bin")); }
    }
    for bin in roots {
        let mut versions: Vec<(Vec<u32>, PathBuf)> = Vec::new();
        if let Ok(entries) = fs::read_dir(&bin) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("10.") || !entry.path().is_dir() { continue; }
                let key: Vec<u32> = name.split('.').map(|p| p.parse().unwrap_or(0)).collect();
                versions.push((key, entry.path()));
            }
        }
        versions.sort_by_key(|(key, _)| Reverse(key.clone()));
        for (_, dir) in versions {
            for arch in ["x64", "x86"] {
                let rc = dir.join(arch).join("rc.exe");
                if rc.is_file() { return Some(rc); }
            }
        }
    }
    None
}

/// `canonicalize` yields a `\\?\` path on Windows, which resource compilers may reject.
fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") { Some(rest) => PathBuf::from(rest), None => path }
}

fn main() {
    println!("cargo:rerun-if-changed=builder/icon.ico");
    println!("cargo:rerun-if-changed=builder/build.rs");
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" { return; }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let manifest = out.join("LiveWallpaper.manifest");
    let icon_src = Path::new("builder").join("icon.ico");
    if !icon_src.is_file() { println!("cargo:warning=builder/icon.ico is missing; the executable will use the default icon"); }
    let icon = plain(icon_src.canonicalize().unwrap_or(icon_src));
    let rc = out.join("LiveWallpaper.rc");
    let res = out.join("LiveWallpaper.res");
    let obj = out.join("LiveWallpaper.o");
    let manifest_text = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
    <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
  </requestedPrivileges></security></trustInfo>
  <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls"
    version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true</dpiAware>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">System</dpiAwareness>
  </windowsSettings></application>
</assembly>
"#;
    if fs::write(&manifest, manifest_text).is_err() { println!("cargo:warning=Could not write app manifest; using OS defaults"); return; }
    let quote = |p: &Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let rc_text = format!("1 24 \"{}\"\n101 ICON \"{}\"\n", quote(&manifest), quote(&icon));
    if fs::write(&rc, rc_text).is_err() { println!("cargo:warning=Could not write resource script"); return; }

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let compiled = if target_env == "msvc" {
        let args = vec!["/nologo".to_string(), format!("/fo{}", res.display()), rc.display().to_string()];
        run("rc.exe", &args) || run("rc", &args) || find_sdk_rc().map_or(false, |tool| run(tool, &args))
    } else {
        let args = vec!["-i".to_string(), rc.display().to_string(), "-o".to_string(), obj.display().to_string()];
        run("windres", &args) || run("llvm-windres", &args)
    };
    if compiled {
        if target_env == "msvc" { println!("cargo:rustc-link-arg-bins={}", res.display()); }
        else { println!("cargo:rustc-link-arg-bins={}", obj.display()); }
    } else {
        println!("cargo:warning=No Windows resource compiler found (install the Windows SDK); the executable will use the default icon/manifest");
    }
}
