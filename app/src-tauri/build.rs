fn main() {
    // CI sets BACKER_BUILD to the workflow run number; local builds show "dev".
    println!("cargo:rerun-if-env-changed=BACKER_BUILD");
    let build = std::env::var("BACKER_BUILD").ok().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=BACKER_BUILD_NUMBER={}", build.trim());
    tauri_build::build()
}
