use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIFTGATE_SKIP_WEB_BUILD");
    println!("cargo:rerun-if-env-changed=LIFTGATE_REBUILD_WEB");

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo always sets CARGO_MANIFEST_DIR"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo always sets OUT_DIR"));
    let web_dir = manifest_dir.join("..").join("..").join("web");
    let dist_dir = web_dir.join("dist");
    let has_web_sources = web_dir.join("package.json").is_file();

    if has_web_sources {
        println!(
            "cargo:rerun-if-changed={}",
            web_dir.join("package.json").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            web_dir.join("index.html").display()
        );
        println!("cargo:rerun-if-changed={}", web_dir.join("src").display());
    }

    let skip_build = env::var("LIFTGATE_SKIP_WEB_BUILD")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let force_rebuild = env::var("LIFTGATE_REBUILD_WEB")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let is_release = env::var("PROFILE").as_deref() == Ok("release");

    if !skip_build
        && is_release
        && has_web_sources
        && (force_rebuild || is_dist_missing_or_stale(&web_dir, &dist_dir))
    {
        try_build_web_bundle(&web_dir);
    }

    let generated_rs = out_dir.join("embedded_ui.rs");
    if dist_dir.join("index.html").is_file() {
        println!("cargo:rerun-if-changed={}", dist_dir.display());
        let stage_dir = out_dir.join("embedded_ui_stage");
        let _ = fs::remove_dir_all(&stage_dir);
        fs::create_dir_all(&stage_dir)
            .expect("OUT_DIR must be writable when staging embedded web assets");

        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        collect_files(&dist_dir, &dist_dir, &mut entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut code = String::from("pub const EMBEDDED_UI_ASSETS: &[(&str, &[u8])] = &[\n");
        for (idx, (rel_slash, src_path)) in entries.iter().enumerate() {
            println!("cargo:rerun-if-changed={}", src_path.display());
            let staged_file = stage_dir.join(format!("asset_{idx}.bin"));
            fs::copy(src_path, &staged_file).expect("copying dist asset into OUT_DIR must succeed");
            let staged_slash = staged_file.to_string_lossy().replace('\\', "/");
            let _ = writeln!(
                code,
                "    ({rel_slash:?}, include_bytes!({staged_slash:?})),"
            );
        }
        code.push_str("];\n");
        fs::write(&generated_rs, code).expect("writing embedded_ui.rs in OUT_DIR must succeed");
    } else {
        fs::write(
            &generated_rs,
            "pub const EMBEDDED_UI_ASSETS: &[(&str, &[u8])] = &[];\n",
        )
        .expect("writing empty embedded_ui.rs in OUT_DIR must succeed");
    }
}

fn collect_files(root: &Path, current: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read_dir) = fs::read_dir(current) else {
        return;
    };
    for item in read_dir.flatten() {
        let path = item.path();
        let Ok(meta) = item.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_files(root, &path, out);
        } else if meta.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let parts: Vec<String> = rel
                .components()
                .filter_map(|c| c.as_os_str().to_str().map(str::to_owned))
                .collect();
            if !parts.is_empty() {
                out.push((parts.join("/"), path));
            }
        }
    }
}

fn is_dist_missing_or_stale(web_dir: &Path, dist_dir: &Path) -> bool {
    let dist_index = dist_dir.join("index.html");
    let Ok(dist_mtime) = fs::metadata(&dist_index).and_then(|m| m.modified()) else {
        return true;
    };
    let mut newest_src = None;
    for candidate in [
        web_dir.join("package.json"),
        web_dir.join("index.html"),
        web_dir.join("src"),
    ] {
        update_latest_mtime(&candidate, &mut newest_src);
    }
    newest_src.is_some_and(|src_time| src_time > dist_mtime)
}

fn update_latest_mtime(path: &Path, newest: &mut Option<std::time::SystemTime>) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            update_latest_mtime(&entry.path(), newest);
        }
    } else if let Ok(modified) = meta.modified()
        && newest.is_none_or(|prev| modified > prev)
    {
        *newest = Some(modified);
    }
}

fn try_build_web_bundle(web_dir: &Path) {
    let Some(bun) = resolve_bun_binary() else {
        return;
    };
    if !web_dir.join("node_modules").is_dir()
        && !run_bun_command(&bun, web_dir, &["install", "--frozen-lockfile"])
    {
        println!(
            "cargo:warning=bun install failed while building embedded web UI; continuing without embedded bundle"
        );
        return;
    }
    if !run_bun_command(&bun, web_dir, &["run", "build"]) {
        println!(
            "cargo:warning=bun run build failed while building embedded web UI; continuing without embedded bundle"
        );
    }
}

fn run_bun_command(bun: &Path, web_dir: &Path, args: &[&str]) -> bool {
    Command::new(bun)
        .args(args)
        .current_dir(web_dir)
        .status()
        .is_ok_and(|s| s.success())
}

fn resolve_bun_binary() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["bun.exe", "bun.cmd", "bun"]
    } else {
        &["bun"]
    };
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            for name in names {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"));
    if let Some(h) = home {
        let base = PathBuf::from(h).join(".bun").join("bin");
        for name in names {
            let cand = base.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
