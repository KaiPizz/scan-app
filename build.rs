fn main() {
    println!("cargo:rerun-if-changed=assets/rust-logo.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/rust-logo.ico")
            .set("FileDescription", "Skaner dokumentów")
            .set("ProductName", "Skaner dokumentów")
            .set("OriginalFilename", "skaner-dokumentow.exe");
        resource.compile().expect("kompilacja zasobów Windows");
    }
}
