use std::{fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let assets_dir = Path::new("../../assets");
    emit_rerun_for_path(assets_dir);
}

fn emit_rerun_for_path(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());

    if path.is_dir() {
        let entries = fs::read_dir(path)
            .unwrap_or_else(|err| panic!("failed to read asset directory {}: {err}", path.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|err| {
                panic!("failed to read directory entry under {}: {err}", path.display())
            });
            emit_rerun_for_path(&entry.path());
        }
    }
}
