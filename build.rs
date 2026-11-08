fn main() {
    slint_build::compile("ui/app.slint").expect("slint build");

    #[cfg(windows)]
    {
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let res_path = std::path::Path::new(&out_dir).join("cadence.res");
        let rc_path = std::path::Path::new("cadence.rc");
        if rc_path.exists() {
            let rc_exe = find_rc().unwrap_or_else(|| std::path::PathBuf::from("rc.exe"));
            let status = std::process::Command::new(&rc_exe)
                .args(["/fo", res_path.to_str().unwrap(), rc_path.to_str().unwrap()])
                .status();
            if let Ok(s) = status {
                if s.success() {
                    println!("cargo:rustc-link-arg={}", res_path.display());
                }
            }
        }
    }
}

#[cfg(windows)]
fn find_rc() -> Option<std::path::PathBuf> {
    if let Ok(output) = std::process::Command::new("where").arg("rc.exe").output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().next() {
                let p = std::path::PathBuf::from(line.trim());
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    let kits_dir = std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if kits_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(kits_dir) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("x64").join("rc.exe");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}
