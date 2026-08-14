fn main() {
    #[cfg(windows)]
    {
        embed_manifest::embed_manifest(
            embed_manifest::new_manifest("ARC.Live")
                .requested_execution_level(embed_manifest::manifest::ExecutionLevel::AsInvoker),
        )
        .expect("failed to embed Administrator manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
