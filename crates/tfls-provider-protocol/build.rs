// Compile the vendored tfplugin proto files with tonic-prost-build (tonic 0.14
// moved prost codegen into its own crate). Both tfplugin5 and tfplugin6 are
// compiled and fully supported.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["proto/tfplugin5.proto", "proto/tfplugin6.proto"],
            &["proto"],
        )?;
    println!("cargo:rerun-if-changed=proto/tfplugin5.proto");
    println!("cargo:rerun-if-changed=proto/tfplugin6.proto");
    Ok(())
}
