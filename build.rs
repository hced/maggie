use std::fs::File;
use std::io::Write;
use std::path::Path;

use gl_generator::{Api, Fallbacks, GlobalGenerator, Profile, Registry};

fn main() {
    // Maggie's committed build baseline is x86-64-v3 (AVX2). If this build
    // machine's CPU can't run AVX2, warn loudly at build time: a v3 binary
    // would otherwise crash with SIGILL the moment it starts. On machines
    // that do support AVX2 this is a silent no-op.
    if !std::arch::is_x86_feature_detected!("avx2") {
        println!(
            "cargo:warning=this CPU does not support AVX2 (x86-64-v3), Maggie's committed \
             build baseline — the release binary would crash with SIGILL. Build with \
             `just build-generic` or RUSTFLAGS=\"-C target-cpu=x86-64\" instead."
        );
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("gles2.rs");

    let registry = Registry::new(
        Api::Gles2,
        (2, 0),
        Profile::Core,
        Fallbacks::All,
        ["GL_OES_vertex_array_object"],
    );

    let mut buf = Vec::new();
    registry.write_bindings(GlobalGenerator, &mut buf).unwrap();

    let source = String::from_utf8(buf).unwrap().replace(" -> ()", "");
    File::create(&dest)
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
}
