use cargo_metadata::MetadataCommand;
use std::io::Result;

fn main() -> Result<()> {
    let wasm_ast_root = find_package_root("golem-wasm-ast");

    let mut config = prost_build::Config::new();
    config.extern_path(".wasm.ast", "::golem_wasm_ast::analysis::protobuf");
    config.type_attribute(".", "#[cfg(feature = \"protobuf\")]");
    config.type_attribute(
        ".",
        "#[cfg_attr(feature=\"bincode\", derive(bincode::Encode, bincode::Decode))]",
    );
    config.compile_protos(
        &[
            "proto/wasm/rpc/val.proto",
            "proto/wasm/rpc/witvalue.proto",
            "proto/wasm/rpc/type_annotated_value.proto",
        ],
        &[&format!("{wasm_ast_root}/proto"), &"proto".to_string()],
    )?;
    Ok(())
}

fn find_package_root(name: &str) -> String {
    let metadata = MetadataCommand::new()
        .manifest_path(std::env!("CARGO_MANIFEST_DIR").to_owned() + "/Cargo.toml")
        .verbose(true)
        .exec()
        .unwrap();
    let package = metadata.packages.iter().find(|p| p.name == name).expect(name);
    package.manifest_path.parent().unwrap().to_string()
}
