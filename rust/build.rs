//! Embed the page's optional parts, when they have been built:
//!
//! - the engine compiled to WebAssembly (scripts/build-wasm.sh), without
//!   which the page converts on the server;
//! - the Perspective table viewer (`npm run perspective` in rust/web),
//!   without which the page offers no viewer.

use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=web/wasm");
    println!("cargo:rerun-if-changed=web/perspective");
    let mut code = String::new();

    let js = fs::canonicalize("web/wasm/sap_bin_wasm.js");
    let wasm = fs::canonicalize("web/wasm/sap_bin_wasm_bg.wasm");
    match (js, wasm) {
        (Ok(js), Ok(wasm)) => code.push_str(&format!(
            "pub const JS: &str = include_str!({js:?});\npub const WASM: &[u8] = include_bytes!({wasm:?});\n"
        )),
        _ => code.push_str("pub const JS: &str = \"\";\npub const WASM: &[u8] = &[];\n"),
    }

    let mut files = Vec::new();
    collect(Path::new("web/perspective"), &mut files);
    files.sort();
    code.push_str("/// (path under assets/perspective/, content type, bytes)\n");
    code.push_str("pub const PERSPECTIVE: &[(&str, &str, &[u8])] = &[\n");
    for file in files {
        let relative = file
            .strip_prefix("web/perspective")
            .expect("under web/perspective")
            .to_string_lossy()
            .replace('\\', "/");
        let kind = match file.extension().and_then(|e| e.to_str()) {
            Some("js") => "text/javascript; charset=utf-8",
            Some("wasm") => "application/wasm",
            Some("css") => "text/css; charset=utf-8",
            Some("md") => "text/markdown; charset=utf-8",
            _ => "application/octet-stream",
        };
        let path = fs::canonicalize(&file).expect("canonical path");
        code.push_str(&format!(
            "    ({relative:?}, {kind:?}, include_bytes!({path:?})),\n"
        ));
    }
    code.push_str("];\n");

    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("web_parts.rs");
    fs::write(out, code).expect("write web_parts.rs");
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
