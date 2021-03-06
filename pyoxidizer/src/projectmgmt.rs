// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Manage PyOxidizer projects.

use anyhow::{anyhow, Result};
use slog::warn;
use std::fs::create_dir_all;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process;

use super::environment::{canonicalize_path, MINIMUM_RUST_VERSION};
use crate::app_packaging::config::{eval_starlark_config_file, find_pyoxidizer_config_file_env};
use crate::app_packaging::repackage::{package_project, process_config, run_from_build};
use crate::app_packaging::state::BuildContext;
use crate::project_layout::{find_pyoxidizer_files, initialize_project};
use crate::py_packaging::config::RawAllocator;
use crate::py_packaging::distribution::{analyze_python_distribution_tar_zst, python_exe_path};

/// Attempt to resolve the default Rust target for a build.
pub fn default_target() -> Result<String> {
    // TODO derive these more intelligently.
    if cfg!(target_os = "linux") {
        Ok("x86_64-unknown-linux-gnu".to_string())
    } else if cfg!(target_os = "windows") {
        Ok("x86_64-pc-windows-msvc".to_string())
    } else if cfg!(target_os = "macos") {
        Ok("x86_64-apple-darwin".to_string())
    } else {
        Err(anyhow!("unable to resolve target"))
    }
}

fn dependency_current(
    logger: &slog::Logger,
    path: &Path,
    built_time: std::time::SystemTime,
) -> bool {
    match path.metadata() {
        Ok(md) => match md.modified() {
            Ok(t) => {
                if t > built_time {
                    warn!(
                        logger,
                        "building artifacts because {} changed",
                        path.display()
                    );
                    false
                } else {
                    true
                }
            }
            Err(_) => {
                warn!(logger, "error resolving mtime of {}", path.display());
                false
            }
        },
        Err(_) => {
            warn!(logger, "error resolving metadata of {}", path.display());
            false
        }
    }
}

/// Determines whether PyOxidizer artifacts are current.
fn artifacts_current(logger: &slog::Logger, config_path: &Path, artifacts_path: &Path) -> bool {
    let metadata_path = artifacts_path.join("cargo_metadata.txt");

    if !metadata_path.exists() {
        warn!(logger, "no existing PyOxidizer artifacts found");
        return false;
    }

    // We assume the mtime of the metadata file is the built time. If we
    // encounter any modified times newer than that file, we're not up to date.
    let built_time = match metadata_path.metadata() {
        Ok(md) => match md.modified() {
            Ok(t) => t,
            Err(_) => {
                warn!(
                    logger,
                    "error determining mtime of {}",
                    metadata_path.display()
                );
                return false;
            }
        },
        Err(_) => {
            warn!(
                logger,
                "error resolving metadata of {}",
                metadata_path.display()
            );
            return false;
        }
    };

    let metadata_data = match std::fs::read_to_string(&metadata_path) {
        Ok(data) => data,
        Err(_) => {
            warn!(logger, "error reading {}", metadata_path.display());
            return false;
        }
    };

    for line in metadata_data.split('\n') {
        if line.starts_with("cargo:rerun-if-changed=") {
            let path = PathBuf::from(&line[23..line.len()]);

            if !dependency_current(logger, &path, built_time) {
                return false;
            }
        }
    }

    let current_exe = std::env::current_exe().expect("unable to determine current exe");
    if !dependency_current(logger, &current_exe, built_time) {
        return false;
    }

    if !dependency_current(logger, config_path, built_time) {
        return false;
    }

    // TODO detect config file change.
    true
}

/// Build PyOxidizer artifacts for a project.
fn build_pyoxidizer_artifacts(logger: &slog::Logger, context: &mut BuildContext) -> Result<()> {
    let pyoxidizer_artifacts_path = &context.pyoxidizer_artifacts_path;

    create_dir_all(&pyoxidizer_artifacts_path)?;

    let pyoxidizer_artifacts_path = canonicalize_path(pyoxidizer_artifacts_path)?;

    if !artifacts_current(logger, &context.config_path, &pyoxidizer_artifacts_path) {
        process_config(logger, context, "0");
    }

    Ok(())
}

/// Build an oxidized Rust application at the specified project path.
pub fn build_project(logger: &slog::Logger, context: &mut BuildContext) -> Result<()> {
    if let Ok(rust_version) = rustc_version::version() {
        if rust_version.lt(&MINIMUM_RUST_VERSION) {
            return Err(anyhow!(
                "PyOxidizer requires Rust {}; version {} found",
                *MINIMUM_RUST_VERSION,
                rust_version,
            ));
        }
    } else {
        return Err(anyhow!(
            "unable to determine Rust version; is Rust installed?"
        ));
    }

    // Our build process is to first generate artifacts from the PyOxidizer
    // configuration within this process then call out to `cargo build`. We do
    // this because it is easier to emit output from this process than to have
    // it proxied via cargo.
    build_pyoxidizer_artifacts(logger, context)?;

    let mut args = Vec::new();
    args.push("build");

    args.push("--target");
    args.push(&context.target_triple);

    // We use an explicit target directory so we can be sure we write our
    // artifacts to the same directory that cargo is using (unless the config
    // file overwrites the artifacts directory, of course).
    let target_dir = context.target_base_path.display().to_string();
    args.push("--target-dir");
    args.push(&target_dir);

    args.push("--bin");
    args.push(&context.config.build_config.application_name);

    if context.release {
        args.push("--release");
    }

    if context.config.embedded_python_config.raw_allocator == RawAllocator::Jemalloc {
        args.push("--features");
        args.push("jemalloc");
    }

    let mut envs = Vec::new();
    envs.push((
        "PYOXIDIZER_ARTIFACT_DIR",
        context.pyoxidizer_artifacts_path.display().to_string(),
    ));
    envs.push(("PYOXIDIZER_REUSE_ARTIFACTS", "1".to_string()));

    // Set PYTHON_SYS_EXECUTABLE so python3-sys uses our distribution's Python to
    // configure itself.
    let python_exe_path = python_exe_path(&context.python_distribution_path)?;
    envs.push((
        "PYTHON_SYS_EXECUTABLE",
        python_exe_path.display().to_string(),
    ));

    // static-nobundle link kind requires nightly Rust compiler until
    // https://github.com/rust-lang/rust/issues/37403 is resolved.
    if cfg!(windows) {
        envs.push(("RUSTC_BOOTSTRAP", "1".to_string()));
    }

    let status = process::Command::new("cargo")
        .args(args)
        .current_dir(&context.project_path)
        .envs(envs)
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("cargo build failed"))
    }
}

pub fn resolve_build_context(
    logger: &slog::Logger,
    project_path: &str,
    config_path: Option<&str>,
    target: Option<&str>,
    release: bool,
    force_artifacts_path: Option<&Path>,
    verbose: bool,
) -> Result<BuildContext> {
    let path = canonicalize_path(&PathBuf::from(project_path))?;

    if find_pyoxidizer_files(&path).is_empty() {
        return Err(anyhow!("no PyOxidizer files in specified path"));
    }

    let target = match target {
        Some(v) => v.to_string(),
        None => default_target()?,
    };

    let config_path = match config_path {
        Some(p) => PathBuf::from(p),
        None => match find_pyoxidizer_config_file_env(logger, &path) {
            Some(p) => p,
            None => return Err(anyhow!("unable to find PyOxidizer config file")),
        },
    };

    let res = eval_starlark_config_file(logger, &config_path, &target)?;

    BuildContext::new(
        &path,
        res.config,
        None,
        &target,
        release,
        force_artifacts_path,
        verbose,
    )
}

fn run_project(
    logger: &slog::Logger,
    context: &mut BuildContext,
    extra_args: &[&str],
) -> Result<()> {
    // We call our build wrapper and invoke the binary directly. This allows
    // build output to be printed.
    build_project(logger, context)?;

    package_project(logger, context)?;

    let status = process::Command::new(&context.app_exe_path)
        .current_dir(&context.project_path)
        .args(extra_args)
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("cargo run failed"))
    }
}

/// Build a PyOxidizer enabled project.
///
/// This is a glorified wrapper around `cargo build`. Our goal is to get the
/// output from repackaging to give the user something for debugging.
pub fn build(
    logger: &slog::Logger,
    project_path: &str,
    target: Option<&str>,
    release: bool,
    verbose: bool,
) -> Result<()> {
    let mut context =
        resolve_build_context(logger, project_path, None, target, release, None, verbose)?;
    build_project(logger, &mut context)?;
    package_project(logger, &mut context)?;

    warn!(
        logger,
        "executable path: {}",
        context.app_exe_path.display()
    );

    Ok(())
}

pub fn build_artifacts(
    logger: &slog::Logger,
    project_path: &Path,
    dest_path: &Path,
    target: Option<&str>,
    release: bool,
    verbose: bool,
) -> Result<()> {
    let mut context = resolve_build_context(
        logger,
        project_path.to_str().unwrap(),
        None,
        target,
        release,
        Some(dest_path),
        verbose,
    )?;

    build_pyoxidizer_artifacts(logger, &mut context)?;

    Ok(())
}

pub fn run(
    logger: &slog::Logger,
    project_path: &str,
    target: Option<&str>,
    release: bool,
    extra_args: &[&str],
    verbose: bool,
) -> Result<()> {
    let mut context =
        resolve_build_context(logger, project_path, None, target, release, None, verbose)?;

    run_project(logger, &mut context, extra_args)
}

/// Initialize a new Rust project with PyOxidizer support.
///
/// `code` can specify custom Python code to run by default in the new
/// application.
///
/// `pip_install` can specify Python packages to `pip install` for the application.
pub fn init(project_path: &str, code: Option<&str>, pip_install: &[&str]) -> Result<()> {
    let path = Path::new(project_path);

    initialize_project(&path, code, pip_install)?;
    println!();
    println!(
        "A new Rust binary application has been created in {}",
        path.display()
    );
    println!();
    println!("This application can be built by doing the following:");
    println!();
    println!("  $ cd {}", path.display());
    println!("  $ pyoxidizer build");
    println!("  $ pyoxidizer run");
    println!();
    println!("The default configuration is to invoke a Python REPL. You can");
    println!("edit the various pyoxidizer.*.bzl config files or the main.rs ");
    println!("file to change behavior. The application will need to be rebuilt ");
    println!("for configuration changes to take effect.");

    Ok(())
}

pub fn python_distribution_extract(dist_path: &str, dest_path: &str) -> Result<()> {
    let mut fh = std::fs::File::open(Path::new(dist_path))?;
    let mut data = Vec::new();
    fh.read_to_end(&mut data)?;
    let cursor = Cursor::new(data);
    let dctx = zstd::stream::Decoder::new(cursor)?;
    let mut tf = tar::Archive::new(dctx);

    println!("extracting archive to {}", dest_path);
    tf.unpack(dest_path)?;

    Ok(())
}

pub fn python_distribution_info(dist_path: &str) -> Result<()> {
    let mut fh = std::fs::File::open(Path::new(dist_path))?;
    let mut data = Vec::new();
    fh.read_to_end(&mut data)?;

    let temp_dir = tempdir::TempDir::new("python-distribution")?;
    let temp_dir_path = temp_dir.path();

    let cursor = Cursor::new(data);
    let dist = analyze_python_distribution_tar_zst(cursor, temp_dir_path)?;

    println!("High-Level Metadata");
    println!("===================");
    println!();
    println!("Flavor:       {}", dist.flavor);
    println!("Version:      {}", dist.version);
    println!("OS:           {}", dist.os);
    println!("Architecture: {}", dist.arch);
    println!();

    println!("Extension Modules");
    println!("=================");
    for (name, ems) in dist.extension_modules {
        println!("{}", name);
        println!("{}", "-".repeat(name.len()));
        println!();

        for em in ems {
            println!("{}", em.variant);
            println!("{}", "^".repeat(em.variant.len()));
            println!();
            println!("Required: {}", em.required);
            println!("Built-in Default: {}", em.builtin_default);
            if let Some(licenses) = em.licenses {
                println!("Licenses: {}", licenses.join(", "));
            }
            if !em.links.is_empty() {
                println!(
                    "Links: {}",
                    em.links
                        .iter()
                        .map(|l| l.name.clone())
                        .collect::<Vec<String>>()
                        .join(", ")
                );
            }

            println!();
        }
    }

    println!("Python Modules");
    println!("==============");
    println!();
    for name in dist.py_modules.keys() {
        println!("{}", name);
    }
    println!();

    println!("Python Resources");
    println!("================");
    println!();

    for (package, resources) in dist.resources {
        for name in resources.keys() {
            println!("[{}].{}", package, name);
        }
    }

    Ok(())
}

pub fn python_distribution_licenses(path: &str) -> Result<()> {
    let mut fh = std::fs::File::open(Path::new(path))?;
    let mut data = Vec::new();
    fh.read_to_end(&mut data)?;

    let temp_dir = tempdir::TempDir::new("python-distribution")?;
    let temp_dir_path = temp_dir.path();

    let cursor = Cursor::new(data);
    let dist = analyze_python_distribution_tar_zst(cursor, temp_dir_path)?;

    println!(
        "Python Distribution Licenses: {}",
        match dist.licenses {
            Some(licenses) => itertools::join(licenses, ", "),
            None => "NO LICENSE FOUND".to_string(),
        }
    );
    println!();
    println!("Extension Libraries and License Requirements");
    println!("============================================");
    println!();

    for (name, variants) in &dist.extension_modules {
        for variant in variants {
            if variant.links.is_empty() {
                continue;
            }

            let name = if variant.variant == "default" {
                name.clone()
            } else {
                format!("{} ({})", name, variant.variant)
            };

            println!("{}", name);
            println!("{}", "-".repeat(name.len()));
            println!();

            for link in &variant.links {
                println!("Dependency: {}", &link.name);
                println!(
                    "Link Type: {}",
                    if link.system {
                        "system"
                    } else if link.framework {
                        "framework"
                    } else {
                        "library"
                    }
                );

                println!();
            }

            if variant.license_public_domain.is_some() && variant.license_public_domain.unwrap() {
                println!("Licenses: Public Domain");
            } else if let Some(ref licenses) = variant.licenses {
                println!("Licenses: {}", itertools::join(licenses, ", "));
                for license in licenses {
                    println!("License Info: https://spdx.org/licenses/{}.html", license);
                }
            } else {
                println!("Licenses: UNKNOWN");
            }

            println!();
        }
    }

    Ok(())
}

pub fn run_build_script(logger: &slog::Logger, build_script: &str) -> Result<()> {
    run_from_build(logger, build_script);

    Ok(())
}
