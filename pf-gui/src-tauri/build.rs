//! Tauri build hook.
//!
//! The frontend is built by the Tauri CLI / npm into `../dist`. When only
//! `cargo build -p pf-gui` is run (or CI builds the workspace without Node),
//! that directory may not exist yet; `tauri::generate_context!` embeds the
//! frontend at compile time and would fail. We create a minimal placeholder so
//! the Rust crate always compiles, then the real bundle overwrites it.

fn main() {
    ensure_frontend_placeholder();
    tauri_build::build();
}

fn ensure_frontend_placeholder() {
    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let Some(project) = std::path::Path::new(&manifest).parent() else {
        return;
    };
    let dist = project.join("dist");
    let index = dist.join("index.html");
    if !index.exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(
            &index,
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>power-forensics</title></head>\
             <body style=\"font-family:sans-serif;background:#0e1116;color:#c8d1dc\">\
             <p>Frontend not built. Run <code>npm install &amp;&amp; npm run build</code> in <code>pf-gui</code>.</p>\
             </body></html>",
        );
    }
    println!("cargo:rerun-if-changed=../dist");
}
