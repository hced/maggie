use std::io::Write;

fn main() {
    // Only generate GL bindings when the Wayland feature is enabled.
    if std::env::var("CARGO_FEATURE_WAYLAND").is_err() {
        return;
    }

    if !std::arch::is_x86_feature_detected!("avx2") {
        println!(
            "cargo:warning=this CPU does not support AVX2 (x86-64-v3), Maggie's committed \
             build baseline — the release binary would crash with SIGILL. Build with \
             `just build-generic` or RUSTFLAGS=\"-C target-cpu=x86-64\" instead."
        );
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = std::path::Path::new(&out_dir).join("gles2.rs");
    let source = generate_gles2();
    std::fs::File::create(&dest)
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
}

fn generate_gles2() -> String {
    #[cfg(feature = "wayland")]
    {
        use gl_generator::{Api, Fallbacks, GlobalGenerator, Profile, Registry};
        let registry = Registry::new(
            Api::Gles2,
            (2, 0),
            Profile::Core,
            Fallbacks::All,
            ["GL_OES_vertex_array_object"],
        );
        let mut buf = Vec::new();
        registry.write_bindings(GlobalGenerator, &mut buf).unwrap();
        String::from_utf8(buf).unwrap().replace(" -> ()", "")
    }
    #[cfg(not(feature = "wayland"))]
    {
        unreachable!("gl_generator not available without wayland feature")
    }
}
