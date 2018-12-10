#![feature(slice_concat_ext, custom_test_frameworks)]
// Custom test runner, to avoid libtest being wrapped around compiletest which wraps libtest.
#![test_runner(test_runner)]

use std::slice::SliceConcatExt;
use std::path::{PathBuf, Path};
use std::env;

use compiletest_rs as compiletest;
use colored::*;

fn miri_path() -> PathBuf {
    if rustc_test_suite().is_some() {
        PathBuf::from(option_env!("MIRI_PATH").unwrap())
    } else {
        PathBuf::from(concat!("target/", env!("PROFILE"), "/miri"))
    }
}

fn rustc_test_suite() -> Option<PathBuf> {
    option_env!("RUSTC_TEST_SUITE").map(PathBuf::from)
}

fn rustc_lib_path() -> PathBuf {
    option_env!("RUSTC_LIB_PATH").unwrap().into()
}

fn mk_config(mode: &str) -> compiletest::common::ConfigWithTemp {
    let mut config = compiletest::Config::default().tempdir();
    config.mode = mode.parse().expect("Invalid mode");
    config.rustc_path = miri_path();
    if rustc_test_suite().is_some() {
        config.run_lib_path = rustc_lib_path();
        config.compile_lib_path = rustc_lib_path();
    }
    config.filter = env::args().nth(1);
    config
}

fn compile_fail(sysroot: &Path, path: &str, target: &str, host: &str, opt: bool) {
    let opt_str = if opt { " with optimizations" } else { "" };
    eprintln!("{}", format!(
        "## Running compile-fail tests in {} against miri for target {}{}",
        path,
        target,
        opt_str
    ).green().bold());

    let mut flags = Vec::new();
    flags.push(format!("--sysroot {}", sysroot.display()));
    flags.push("-Dwarnings -Dunused".to_owned()); // overwrite the -Aunused in compiletest-rs
    flags.push("--edition 2018".to_owned());
    if opt {
        // Optimizing too aggressivley makes UB detection harder, but test at least
        // the default value.
        // FIXME: Opt level 3 ICEs during stack trace generation.
        flags.push("-Zmir-opt-level=1".to_owned());
    }

    let mut config = mk_config("compile-fail");
    config.src_base = PathBuf::from(path);
    config.target = target.to_owned();
    config.host = host.to_owned();
    config.target_rustcflags = Some(flags.join(" "));
    compiletest::run_tests(&config);
}

fn miri_pass(sysroot: &Path, path: &str, target: &str, host: &str, opt: bool) {
    let opt_str = if opt { " with optimizations" } else { "" };
    eprintln!("{}", format!(
        "## Running run-pass tests in {} against miri for target {}{}",
        path,
        target,
        opt_str
    ).green().bold());

    let mut flags = Vec::new();
    flags.push(format!("--sysroot {}", sysroot.display()));
    flags.push("-Dwarnings -Dunused".to_owned()); // overwrite the -Aunused in compiletest-rs
    flags.push("--edition 2018".to_owned());
    if opt {
        // FIXME: We use opt level 1 because MIR inlining defeats the validation
        // whitelist.
        flags.push("-Zmir-opt-level=1".to_owned());
    }

    let mut config = mk_config("ui");
    config.src_base = PathBuf::from(path);
    config.target = target.to_owned();
    config.host = host.to_owned();
    config.target_rustcflags = Some(flags.join(" "));
    compiletest::run_tests(&config);
}

fn is_target_dir<P: Into<PathBuf>>(path: P) -> bool {
    let mut path = path.into();
    path.push("lib");
    path.metadata().map(|m| m.is_dir()).unwrap_or(false)
}

fn target_has_std<P: Into<PathBuf>>(path: P) -> bool {
    let mut path = path.into();
    path.push("lib");
    std::fs::read_dir(path)
        .expect("invalid target")
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .any(|file_name| file_name == "libstd.rlib")
}


fn for_all_targets<F: FnMut(String)>(sysroot: &Path, f: F) {
    let target_dir = sysroot.join("lib").join("rustlib");
    let mut targets = std::fs::read_dir(target_dir)
        .expect("invalid sysroot")
        .map(|entry| entry.unwrap())
        .filter(|entry| is_target_dir(entry.path()))
        .filter(|entry| target_has_std(entry.path()))
        .map(|entry| entry.file_name().into_string().unwrap())
        .peekable();

    if targets.peek().is_none() {
        panic!("No valid targets found");
    }

    targets.for_each(f);
}

fn get_sysroot() -> PathBuf {
    let sysroot = std::env::var("MIRI_SYSROOT").unwrap_or_else(|_| {
        let sysroot = std::process::Command::new("rustc")
            .arg("--print")
            .arg("sysroot")
            .output()
            .expect("rustc not found")
            .stdout;
        String::from_utf8(sysroot).expect("sysroot is not utf8")
    });
    PathBuf::from(sysroot.trim())
}

fn get_host() -> String {
    let rustc = rustc_test_suite().unwrap_or(PathBuf::from("rustc"));
    let host = std::process::Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("rustc not found for -vV")
        .stdout;
    let host = std::str::from_utf8(&host).expect("sysroot is not utf8");
    let host = host.split("\nhost: ").nth(1).expect(
        "no host: part in rustc -vV",
    );
    let host = host.split('\n').next().expect("no \n after host");
    String::from(host)
}

fn run_pass_miri(opt: bool) {
    let sysroot = get_sysroot();
    let host = get_host();

    for_all_targets(&sysroot, |target| {
        miri_pass(&sysroot, "tests/run-pass", &target, &host, opt);
    });
}

fn compile_fail_miri(opt: bool) {
    let sysroot = get_sysroot();
    let host = get_host();

    for_all_targets(&sysroot, |target| {
        compile_fail(&sysroot, "tests/compile-fail", &target, &host, opt);
    });
}

fn test_runner(_tests: &[&()]) {
    // We put everything into a single test to avoid the parallelism `cargo test`
    // introduces.  We still get parallelism within our tests because `compiletest`
    // uses `libtest` which runs jobs in parallel.

    run_pass_miri(false);
    run_pass_miri(true);

    compile_fail_miri(false);
    compile_fail_miri(true);
}
