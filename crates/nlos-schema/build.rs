use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let proto = "../../schema/nlos/sabi/v1/envelope.proto";
    println!("cargo:rerun-if-changed={proto}");

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.compile_protos(&[proto], &["../../schema"])?;
    Ok(())
}
