use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let lock_path = manifest.join("Cargo.lock");
    let lock = fs::read_to_string(&lock_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", lock_path.display()));
    let version = package_version(&lock, "rolldown")
        .unwrap_or_else(|| panic!("Cargo.lock does not contain the rolldown package"));
    println!("cargo:rustc-env=BPERF_ROLLDOWN_VERSION={version}");
}

fn package_version<'a>(lock: &'a str, expected_name: &str) -> Option<&'a str> {
    lock.split("[[package]]").skip(1).find_map(|package| {
        let mut name = None;
        let mut version = None;
        for line in package.lines().map(str::trim) {
            if let Some(value) = quoted_value(line, "name") {
                name = Some(value);
            } else if let Some(value) = quoted_value(line, "version") {
                version = Some(value);
            }
        }
        (name == Some(expected_name)).then_some(version).flatten()
    })
}

fn quoted_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?
        .strip_prefix(" = \"")?
        .strip_suffix('"')
}
