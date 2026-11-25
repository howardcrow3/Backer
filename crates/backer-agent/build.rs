// The Mac App Store edition (feature `mas`, macOS only) uses a small Objective-C layer for bookmarks, the login item and notifications.
fn main() {
    println!("cargo:rerun-if-changed=native/mas.m");
    if std::env::var_os("CARGO_FEATURE_MAS").is_some() && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new().file("native/mas.m").flag("-fobjc-arc").flag("-fmodules").compile("backer_native");
        for f in ["Foundation", "ServiceManagement", "UserNotifications"] {
            println!("cargo:rustc-link-lib=framework={f}");
        }
    }
}
