// Ensures `web/dist` exists at compile time so the RustEmbed derive in
// `middleware.rs` (folder = "../../web/dist") never fails a build that did
// not run the Vite step. Release builds run after `npm run build`
// (see .github/workflows/release.yml), so the real UI is embedded there;
// this placeholder only fires for bare `cargo build/test` runs.
fn main() {
    let dist = std::path::Path::new("../../web/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        std::fs::create_dir_all(dist)
            .and_then(|_| {
                std::fs::write(
                    &index,
                    "<!doctype html><title>agentgrid</title>\
                     <p>Web UI not bundled in this build. Run \
                     <code>npm ci && npm run build</code> in web/ and rebuild, \
                     or set AGENTGRID_WEB_ROOT to a built dist directory.</p>",
                )
            })
            .expect("create placeholder web/dist");
    }
    println!("cargo:rerun-if-changed=../../web/dist/index.html");
}
