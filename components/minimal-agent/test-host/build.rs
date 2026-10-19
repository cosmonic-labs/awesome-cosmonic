//! wasmtime's bindgen has no switch for WIT feature gates, and `run` is
//! `@unstable(feature = agent-run)` in cosmonic:agent@0.4.0. Copy the
//! component's WIT, deps included, with the gates removed into wit-host/
//! (gitignored). Run `wash wit fetch` in the component first, so ../wit/deps
//! exists.
use std::fs;
use std::path::Path;

fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let path = entry?.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let dest = to.join(name);
        if path.is_dir() {
            copy(&path, &dest)?;
        } else if path.extension().is_some_and(|e| e == "wit") {
            let ungated: String = fs::read_to_string(&path)?
                .lines()
                .filter(|l| !l.trim_start().starts_with("@unstable(feature = agent-run)"))
                .map(|l| format!("{l}\n"))
                .collect();
            fs::write(dest, ungated)?;
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let source = Path::new("../wit");
    println!("cargo:rerun-if-changed=../wit");
    if !source.join("deps").is_dir() {
        panic!("../wit/deps is missing: run `wash wit fetch` in components/minimal-agent first");
    }
    let dest = Path::new("wit-host");
    let _ = fs::remove_dir_all(dest);
    copy(source, dest)
}
