fn main() {
    println!("cargo:rerun-if-changed=packaging/frameback.ico");
    // A Windows executable's icon is a resource compiled into it.
    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_icon("packaging/frameback.ico")
        .compile()
        .expect("embedding the Windows icon");
}
