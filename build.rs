use std::fs::File;
use std::io::Write;
use std::path::Path;

use gl_generator::{Api, Fallbacks, GlobalGenerator, Profile, Registry};

fn main() {
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
    File::create(&dest).unwrap().write_all(source.as_bytes()).unwrap();
}
