use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/style");
    println!("cargo:rerun-if-changed=frontend/Trunk.toml");
    println!("cargo:rerun-if-changed=frontend/Cargo.toml");
    println!("cargo:rerun-if-changed=frontend/Cargo.lock");
    println!("cargo:rerun-if-changed=frontend/public");
    println!("cargo:rerun-if-changed=frontend-dist");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR");
    let frontend_dir = Path::new(&manifest_dir).join("frontend");
    let prebuilt_frontend_dist_dir = Path::new(&manifest_dir).join("frontend-dist");
    let cargo_target_dir = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(&manifest_dir).join("target"));
    let frontend_dist_dir = cargo_target_dir.join("frontend-dist");
    let frontend_build_target_dir = cargo_target_dir.join("frontend-build");
    println!(
        "cargo:rustc-env=WAKEZILLA_FRONTEND_DIST={}",
        frontend_dist_dir.display()
    );

    let is_release = env::var("PROFILE").as_deref() == Ok("release");
    let frontend_index = frontend_dist_dir.join("index.html");
    let must_build = is_release || !frontend_index.is_file();

    // Keep debug builds fast when frontend assets already exist.
    if !must_build {
        return;
    }

    if !frontend_dir.is_dir() {
        if prebuilt_frontend_dist_dir.join("index.html").is_file() {
            copy_dir_all(&prebuilt_frontend_dist_dir, &frontend_dist_dir);
            return;
        }

        panic!(
            "frontend assets not found: expected either `{}` or `{}`",
            frontend_dir.display(),
            prebuilt_frontend_dist_dir.join("index.html").display()
        );
    }

    ensure_tool_or_install("trunk", &["install", "trunk", "--locked"]);
    ensure_tool_or_install("wasm-bindgen", &["install", "--locked", "wasm-bindgen-cli"]);

    if !toolchain_installed("nightly") {
        run_command(
            "rustup",
            &["toolchain", "install", "nightly", "--allow-downgrade"],
            None,
            &[],
        );
    }
    if !target_installed("nightly", "wasm32-unknown-unknown") {
        run_command(
            "rustup",
            &[
                "target",
                "add",
                "wasm32-unknown-unknown",
                "--toolchain",
                "nightly",
            ],
            None,
            &[],
        );
    }

    let mut trunk_args = vec!["build".to_string()];
    if is_release {
        trunk_args.push("--release".to_string());
    }
    trunk_args.push("--dist".to_string());
    trunk_args.push(frontend_dist_dir.display().to_string());
    let trunk_args_refs: Vec<&str> = trunk_args.iter().map(String::as_str).collect();

    run_command(
        "trunk",
        &trunk_args_refs,
        Some("frontend"),
        &[
            ("NO_COLOR", "true"),
            (
                "CARGO_TARGET_DIR",
                frontend_build_target_dir
                    .to_str()
                    .expect("valid target path"),
            ),
        ],
    );
}

fn copy_dir_all(src: &Path, dst: &Path) {
    if dst.exists() {
        fs::remove_dir_all(dst).unwrap_or_else(|err| {
            panic!(
                "failed to clean frontend dist directory `{}`: {err}",
                dst.display()
            )
        });
    }

    fs::create_dir_all(dst).unwrap_or_else(|err| {
        panic!(
            "failed to create frontend dist directory `{}`: {err}",
            dst.display()
        )
    });

    for entry in fs::read_dir(src).unwrap_or_else(|err| {
        panic!(
            "failed to read frontend dist directory `{}`: {err}",
            src.display()
        )
    }) {
        let entry = entry.expect("failed to read frontend dist entry");
        let file_type = entry.file_type().unwrap_or_else(|err| {
            panic!(
                "failed to read file type for frontend asset `{}`: {err}",
                entry.path().display()
            )
        });
        let from = entry.path();
        let to = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_all(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap_or_else(|err| {
                panic!(
                    "failed to copy frontend asset `{}` to `{}`: {err}",
                    from.display(),
                    to.display()
                )
            });
        }
    }
}

fn ensure_tool_or_install(tool: &str, cargo_install_args: &[&str]) {
    if which::which(tool).is_ok() {
        return;
    }

    let args_text = cargo_install_args.join(" ");
    println!("cargo:warning=`{tool}` not found. Installing with: cargo {args_text}");
    run_command("cargo", cargo_install_args, None, &[]);

    if which::which(tool).is_err() {
        panic!("`{tool}` is still not available after installation");
    }
}

fn run_command(program: &str, args: &[&str], current_dir: Option<&str>, envs: &[(&str, &str)]) {
    let mut command = Command::new(program);
    command.args(args);

    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    for (key, value) in envs {
        command.env(key, value);
    }

    let printable_cmd = format!("{program} {}", args.join(" "));
    println!("cargo:warning=running {printable_cmd}");

    let status = command
        .status()
        .unwrap_or_else(|err| panic!("failed to run `{printable_cmd}`: {err}"));

    if !status.success() {
        panic!("command failed: `{printable_cmd}` (status: {status})");
    }
}

fn toolchain_installed(name_prefix: &str) -> bool {
    let output = Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run `rustup toolchain list`: {err}"));

    if !output.status.success() {
        panic!(
            "`rustup toolchain list` failed with status {}",
            output.status
        );
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|toolchain| toolchain.starts_with(name_prefix))
}

fn target_installed(toolchain: &str, target: &str) -> bool {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed", "--toolchain", toolchain])
        .output()
        .unwrap_or_else(|err| panic!("failed to run `rustup target list --installed`: {err}"));

    if !output.status.success() {
        panic!(
            "`rustup target list --installed --toolchain {toolchain}` failed with status {}",
            output.status
        );
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|installed_target| installed_target == target)
}
