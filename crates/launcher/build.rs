fn main() {
    // When cross-compiling for musl from a glibc host (Nix build),
    // the linker can't find musl's libc.a automatically.  The Nix
    // derivation sets MUSL_LIBC_DIR to the musl sysroot lib path.
    if let Ok(dir) = std::env::var("MUSL_LIBC_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }
}
