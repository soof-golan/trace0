fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(["proto/perfetto.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto/perfetto.proto");
    Ok(())
}
