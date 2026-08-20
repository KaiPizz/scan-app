fn main() {
    println!("cargo:rerun-if-changed=assets/rust-logo.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/rust-logo.ico")
            .set("FileDescription", "Skaner dokumentów")
            .set("ProductName", "Skaner dokumentów")
            .set("OriginalFilename", "skaner-dokumentow.exe");
        match resource.compile() {
            Ok(()) => {}
            // A non-Windows host cross-checking the code has no usable rc; the
            // icon/version info only matters for real Windows builds.
            Err(error) if std::env::var("HOST").is_ok_and(|host| !host.contains("windows")) => {
                println!("cargo:warning=pominięto zasoby Windows w cross-check: {error}");
            }
            Err(error) => panic!("kompilacja zasobów Windows: {error}"),
        }
    }
}
