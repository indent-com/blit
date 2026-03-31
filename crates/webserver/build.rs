use std::io::Write;
use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let src = Path::new(&manifest).join("../../js/web-app/dist/index.html");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dst = Path::new(&out_dir).join("index.html.br");

    println!("cargo::rerun-if-changed={}", src.display());

    let raw = std::fs::read(&src).unwrap_or_else(|e| {
        panic!("cannot read {}: {e}", src.display());
    });

    let mut compressed = Vec::new();
    let mut encoder = brotli::CompressorWriter::new(&mut compressed, 4096, 11, 22);
    encoder.write_all(&raw).unwrap();
    drop(encoder);

    std::fs::write(&dst, &compressed).unwrap();
}
