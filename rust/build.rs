//! Embed the WebAssembly build of the engine, when it has been built
//! (scripts/build-wasm.sh). Without it the page converts on the server.

use std::path::Path;
use std::{env, fs};

fn main() {
    let dir = Path::new("web/wasm");
    println!("cargo:rerun-if-changed=web/wasm");
    let js = dir.join("sap_bin_wasm.js");
    let wasm = dir.join("sap_bin_wasm_bg.wasm");
    let code = match (fs::canonicalize(&js), fs::canonicalize(&wasm)) {
        (Ok(js), Ok(wasm)) => format!(
            "pub const JS: &str = include_str!({js:?});\npub const WASM: &[u8] = include_bytes!({wasm:?});\n"
        ),
        _ => "pub const JS: &str = \"\";\npub const WASM: &[u8] = &[];\n".to_owned(),
    };
    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("wasm.rs");
    fs::write(out, code).expect("write wasm.rs");
}
