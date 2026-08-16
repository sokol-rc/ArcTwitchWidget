fn main() {
    #[cfg(windows)]
    {
        embed_manifest::embed_manifest(
            embed_manifest::new_manifest("ARC.Live")
                .requested_execution_level(embed_manifest::manifest::ExecutionLevel::AsInvoker),
        )
        .expect("failed to embed Administrator manifest");

        // The icon resource needs the Windows SDK resource compiler. A machine
        // without it still gets a working build, just an iconless executable.
        let manifest_directory =
            std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets the manifest directory");
        let icon = std::path::Path::new(&manifest_directory).join("../../assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(&icon.to_string_lossy());
        if let Err(error) = resource.compile() {
            println!("cargo:warning=icon resource was not embedded: {error}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
}
