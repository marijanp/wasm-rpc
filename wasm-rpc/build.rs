use cargo_metadata::MetadataCommand;
use std::io::Result;
use std::process::Command;
use std::process::Stdio;

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
    let output = Command::new("echo")
        .arg("$CARGO")
        .output()  // Run the command and capture the output
        .expect("Failed to execute command");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("{}", stdout);
    let metadata = MetadataCommand::new()
        .manifest_path("./Cargo.toml")
        .verbose(true)
        .other_options(vec!["--offline".to_string()])
        .exec()
        .unwrap();
    let package = metadata.packages.iter().find(|p| p.name == name).unwrap();
    package.manifest_path.parent().unwrap().to_string()
}
