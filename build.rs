// Builds the decompression part of libdeflate 1.26 (third_party/libdeflate,
// MIT License, see COPYING there; unmodified upstream source) with the C
// compiler of the configured linker (llvm-mingw clang), into a static
// library that src/inflate.rs links.
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCES: [&str; 5] = ["lib/deflate_decompress.c", "lib/zlib_decompress.c", "lib/adler32.c", "lib/utils.c", "lib/x86/cpu_features.c"];

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party").join("libdeflate");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // Cargo passes the configured linker to build scripts; it is clang.
    let cc = PathBuf::from(std::env::var("RUSTC_LINKER").unwrap_or_else(|_| "clang".into()));
    let ar = cc.parent().map(|d| d.join("llvm-ar.exe")).filter(|p| p.exists()).unwrap_or_else(|| "llvm-ar".into());
    let mut objs = Vec::new();
    for src in SOURCES {
        let obj = out.join(src.replace('/', "_").replace(".c", ".o"));
        let st = Command::new(&cc)
            .args(["-O2", "-DNDEBUG", "-c"])
            .arg(root.join(src))
            .arg("-o")
            .arg(&obj)
            .status()
            .expect("C compiler not found");
        assert!(st.success(), "compiling {src} failed");
        objs.push(obj);
    }
    let lib = out.join("libdeflate.a");
    let _ = std::fs::remove_file(&lib);
    let st = Command::new(&ar).arg("crs").arg(&lib).args(&objs).status().expect("llvm-ar not found");
    assert!(st.success(), "llvm-ar failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rerun-if-changed=third_party/libdeflate");
    println!("cargo:rerun-if-changed=build.rs");
}
