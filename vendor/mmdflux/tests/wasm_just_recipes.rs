use std::fs;
use std::path::PathBuf;

#[test]
fn justfile_exposes_wasm_recipes() {
    let justfile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Justfile");
    let contents = fs::read_to_string(&justfile_path).unwrap_or_else(|error| {
        panic!(
            "failed to read Justfile at {}: {error}",
            justfile_path.display()
        )
    });

    assert!(
        contents
            .lines()
            .any(|line| line.trim_start().starts_with("wasm-build:")),
        "Justfile must include `wasm-build` recipe"
    );
    assert!(
        contents
            .lines()
            .any(|line| line.trim_start().starts_with("wasm-test:")),
        "Justfile must include `wasm-test` recipe"
    );
}
