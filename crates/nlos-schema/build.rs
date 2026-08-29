use std::error::Error;
use std::fmt::Write;

use prost_build::{Method, Service, ServiceGenerator};

struct LocalRpcServiceGenerator;

impl ServiceGenerator for LocalRpcServiceGenerator {
    fn generate(&mut self, service: Service, output: &mut String) {
        assert_eq!(service.package, "nlos.sabi.v1");
        assert_eq!(service.proto_name, "LocalRpcService");
        assert_eq!(service.methods.len(), 1);
        let method = &service.methods[0];
        assert_local_exchange(method);

        writeln!(
            output,
            r#"
/// Transport-neutral client surface generated from `LocalRpcService`.
pub mod local_rpc {{
    pub const FULL_NAME: &str = "nlos.sabi.v1.LocalRpcService";
    pub const EXCHANGE_NAME: &str = "nlos.sabi.v1.LocalRpcService/Exchange";

    pub trait Client {{
        type Error;

        fn exchange(
            &self,
            request: super::ExchangeRequest,
        ) -> impl core::future::Future<Output = Result<super::ExchangeResponse, Self::Error>> + Send;
    }}
}}
"#
        )
        .expect("writing generated Rust source to a String cannot fail");
    }
}

fn assert_local_exchange(method: &Method) {
    assert_eq!(method.proto_name, "Exchange");
    assert_eq!(method.input_proto_type, ".nlos.sabi.v1.ExchangeRequest");
    assert_eq!(method.output_proto_type, ".nlos.sabi.v1.ExchangeResponse");
    assert!(!method.client_streaming);
    assert!(!method.server_streaming);
}

fn main() -> Result<(), Box<dyn Error>> {
    let protos = [
        "../../schema/nlos/sabi/v1/envelope.proto",
        "../../schema/nlos/sabi/v1/service_directory.proto",
        "../../schema/nlos/sabi/v1/operation_control.proto",
        "../../schema/nlos/sabi/v1/system_control.proto",
        "../../schema/nlos/sabi/v1/takeover_control.proto",
        "../../schema/nlos/sabi/v1/wait_control.proto",
        "../../schema/nlos/sabi/v1/principal_handshake.proto",
    ];
    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.service_generator(Box::new(LocalRpcServiceGenerator));
    config.compile_protos(&protos, &["../../schema"])?;
    Ok(())
}
