// Copyright 2024 Golem Cloud
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod cargo;
mod compilation;
mod make;
mod rust;
mod stub;
mod wit;

use crate::cargo::generate_cargo_toml;
use crate::compilation::compile;
use crate::rust::generate_stub_source;
use crate::stub::StubDefinition;
use crate::wit::{
    copy_wit_files, generate_stub_wit, get_stub_wit, verify_action, StubTypeGen, WitAction,
};
use anyhow::{anyhow, Context};
use clap::Parser;
use fs_extra::dir::CopyOptions;
use golem_wasm_ast::analysis::{AnalysedExport, AnalysisContext, AnalysisFailure};
use golem_wasm_ast::component::Component;
use golem_wasm_ast::IgnoreAllButMetadata;
use heck::ToSnakeCase;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;
use wasm_compose::config::Dependency;
use wit_parser::{PackageName, UnresolvedPackage};

#[derive(Parser, Debug)]
#[command(name = "wasm-rpc-stubgen", version)]
pub enum Command {
    /// Generate a Rust RPC stub crate for a WASM component
    Generate(GenerateArgs),
    /// Build an RPC stub for a WASM component
    Build(BuildArgs),
    /// Adds a generated stub as a dependency to another WASM component
    AddStubDependency(AddStubDependencyArgs),
    /// Compose a WASM component with a generated stub WASM
    Compose(ComposeArgs),
    /// Initializes a Golem-specific cargo-make configuration in a Cargo workspace for automatically
    /// generating stubs and composing results.
    InitializeWorkspace(InitializeWorkspaceArgs),
}

/// Generate a Rust RPC stub crate for a WASM component
///
/// The command creates a new Rust crate that is ready to be compiled with
#[derive(clap::Args, Debug)]
#[command(version, about, long_about = None)]
pub struct GenerateArgs {
    /// The root directory of the component's WIT definition to be called via RPC
    #[clap(short, long)]
    pub source_wit_root: PathBuf,
    /// The target path to generate a new stub crate to
    #[clap(short, long)]
    pub dest_crate_root: PathBuf,
    /// The world name to be used in the generated stub crate. If there is only a single world in the source root
    ///  package, no need to specify.
    #[clap(short, long)]
    pub world: Option<String>,
    /// The crate version of the generated stub crate
    #[clap(long, default_value = "0.0.1")]
    pub stub_crate_version: String,
    #[clap(flatten)]
    pub wasm_rpc_override: WasmRpcOverride,
    /// Always inline all the data types defined in the source WIT instead of copying and depending on
    /// it from the stub WIT. This is useful for example with ComponentizeJS currently where otherwise
    /// the original component's interface would be added as an import to the final WASM.
    #[clap(long, default_value_t = false)]
    pub always_inline_types: bool,
}

#[derive(clap::Args, Debug, Clone)]
#[group(required = false, multiple = false)]
#[derive(Default)]
pub struct WasmRpcOverride {
    /// The path to the `wasm-rpc` crate to be used in the generated stub crate. If not specified, the latest version of `wasm-rpc` will be used. It needs to be an **absolute path**.
    #[clap(long, group = "override")]
    pub wasm_rpc_path_override: Option<String>,
    /// The version of the `wasm-rpc` crate to be used in the generated stub crate. If not specified, the latest version of `wasm-rpc` will be used.
    #[clap(long, group = "override")]
    pub wasm_rpc_version_override: Option<String>,
}

/// Build an RPC stub for a WASM component
///
/// The resulting WASM component implements the **stub interface** corresponding to the source interface, found in the
/// target directory's
/// `wit/_stub.wit` file. This WASM component is to be composed together with another component that calls the original
/// interface via WASM RPC.
#[derive(clap::Args, Debug)]
#[command(version, about, long_about = None)]
pub struct BuildArgs {
    /// The root directory of the component's WIT definition to be called via RPC
    #[clap(short, long)]
    pub source_wit_root: PathBuf,
    /// The name of the stub WASM file to be generated
    #[clap(long)]
    pub dest_wasm: PathBuf,
    /// The directory name where the generated WIT files should be placed
    #[clap(long)]
    pub dest_wit_root: PathBuf,
    /// The world name to be used in the generated stub crate. If there is only a single world in the source root
    ///   package, no need to specify.
    #[clap(short, long)]
    pub world: Option<String>,
    /// The crate version of the generated stub crate
    #[clap(long, default_value = "0.0.1")]
    pub stub_crate_version: String,
    #[clap(flatten)]
    pub wasm_rpc_override: WasmRpcOverride,
    /// Always inline all the data types defined in the source WIT instead of copying and depending on
    /// it from the stub WIT. This is useful for example with ComponentizeJS currently where otherwise
    /// the original component's interface would be added as an import to the final WASM.
    #[clap(long, default_value_t = false)]
    pub always_inline_types: bool,
}

/// Adds a generated stub as a dependency to another WASM component
///
/// The command merges a generated RPC stub as a WIT dependency into another component's WIT root.
#[derive(clap::Args, Debug)]
#[command(version, about, long_about = None)]
pub struct AddStubDependencyArgs {
    /// The WIT root generated by either `generate` or `build` command
    #[clap(short, long)]
    pub stub_wit_root: PathBuf,
    /// The WIT root of the component where the stub should be added as a dependency
    #[clap(short, long)]
    pub dest_wit_root: PathBuf,
    /// This command would not do anything if it detects that it would change an existing WIT file's contents at
    /// the destination. With this flag, it can be forced to overwrite those files.
    #[clap(short, long)]
    pub overwrite: bool,
    /// Enables updating the Cargo.toml file in the parent directory of `dest-wit-root` with the copied
    /// dependencies.
    #[clap(short, long)]
    pub update_cargo_toml: bool,
}

/// Compose a WASM component with a generated stub WASM
///
/// The command composes a caller component's WASM (which uses the generated stub to call a remote worker) with the
/// generated stub WASM, writing out a composed WASM which no longer depends on the stub interface, ready to use.
#[derive(clap::Args, Debug)]
#[command(version, about, long_about = None)]
pub struct ComposeArgs {
    /// The WASM file of the caller component
    #[clap(long)]
    pub source_wasm: PathBuf,
    /// The WASM file of the generated stub. Multiple stubs can be listed.
    #[clap(long, required = true)]
    pub stub_wasm: Vec<PathBuf>,
    /// The name of the composed WASM file to be generated
    #[clap(long)]
    pub dest_wasm: PathBuf,
}

/// Initializes a Golem-specific cargo-make configuration in a Cargo workspace for automatically
/// generating stubs and composing results.
#[derive(clap::Args, Debug)]
#[command(version, about, long_about = None)]
pub struct InitializeWorkspaceArgs {
    /// List of subprojects to be called via RPC
    #[clap(long, required = true)]
    pub targets: Vec<String>,
    /// List of subprojects using the generated stubs for calling remote workers
    #[clap(long, required = true)]
    pub callers: Vec<String>,
    #[clap(flatten)]
    pub wasm_rpc_override: WasmRpcOverride,
}

pub fn generate(args: GenerateArgs) -> anyhow::Result<()> {
    let stub_def = StubDefinition::new(
        &args.source_wit_root,
        &args.dest_crate_root,
        &args.world,
        &args.stub_crate_version,
        &args.wasm_rpc_override,
        args.always_inline_types
    )
    .context("Failed to gather information for the stub generator. Make sure source_wit_root has a valid WIT file.")?;

    let type_gen_strategy = if args.always_inline_types {
        StubTypeGen::InlineRootTypes
    } else {
        StubTypeGen::ImportRootTypes
    };

    generate_stub_wit(&stub_def, type_gen_strategy)
        .context("Failed to generate the stub wit file")?;
    copy_wit_files(&stub_def).context("Failed to copy the dependent wit files")?;
    stub_def
        .verify_target_wits()
        .context("Failed to resolve the result WIT root")?;
    generate_cargo_toml(&stub_def).context("Failed to generate the Cargo.toml file")?;
    generate_stub_source(&stub_def).context("Failed to generate the stub Rust source")?;
    Ok(())
}

pub async fn build(args: BuildArgs) -> anyhow::Result<()> {
    let target_root = TempDir::new()?;
    let canonical_target_root = target_root.path().canonicalize()?;

    let stub_def = StubDefinition::new(
        &args.source_wit_root,
        &canonical_target_root,
        &args.world,
        &args.stub_crate_version,
        &args.wasm_rpc_override,
        args.always_inline_types,
    )
    .context("Failed to gather information for the stub generator")?;

    let type_gen_strategy = if args.always_inline_types {
        StubTypeGen::InlineRootTypes
    } else {
        StubTypeGen::ImportRootTypes
    };

    generate_stub_wit(&stub_def, type_gen_strategy)
        .context("Failed to generate the stub wit file")?;
    copy_wit_files(&stub_def).context("Failed to copy the dependent wit files")?;
    stub_def
        .verify_target_wits()
        .context("Failed to resolve the result WIT root")?;
    generate_cargo_toml(&stub_def).context("Failed to generate the Cargo.toml file")?;
    generate_stub_source(&stub_def).context("Failed to generate the stub Rust source")?;

    compile(&canonical_target_root)
        .await
        .context("Failed to compile the generated stub")?;

    let wasm_path = canonical_target_root
        .join("target")
        .join("wasm32-wasi")
        .join("release")
        .join(format!(
            "{}.wasm",
            stub_def.target_crate_name()?.to_snake_case()
        ));
    if let Some(parent) = args.dest_wasm.parent() {
        fs::create_dir_all(parent)
            .context("Failed to create parent directory of the target WASM file")?;
    }
    fs::copy(wasm_path, &args.dest_wasm)
        .context("Failed to copy the WASM file to the destination")?;

    fs::create_dir_all(&args.dest_wit_root)
        .context("Failed to create the target WIT root directory")?;

    fs_extra::dir::copy(
        canonical_target_root.join("wit"),
        &args.dest_wit_root,
        &CopyOptions::new().content_only(true).overwrite(true),
    )
    .context("Failed to copy the generated WIT files to the destination")?;

    Ok(())
}

pub fn add_stub_dependency(args: AddStubDependencyArgs) -> anyhow::Result<()> {
    // The destination's WIT's package details
    let destination_wit_root = UnresolvedPackage::parse_dir(&args.dest_wit_root)?;

    // Dependencies of stub as directories
    let source_deps = wit::get_dep_dirs(&args.stub_wit_root)?;

    let main_wit = args.stub_wit_root.join("_stub.wit");
    let parsed = UnresolvedPackage::parse_file(&main_wit)?;

    let destination_package_name = destination_wit_root.name.clone();
    let stub_target_package_name = PackageName {
        name: parsed
            .name
            .name
            .strip_suffix("-stub")
            .expect("Unexpected stub package name")
            .to_string(),
        ..parsed.name.clone()
    };
    if destination_package_name == stub_target_package_name {
        return Err(anyhow!(
            "Both the caller and the target components are using the same package name ({destination_package_name}), which is not supported."
        ));
    }

    let world_name = internal::find_world_name(parsed)?;
    let mut actions = Vec::new();

    // If stub generated world points to the destination world (meaning the destination still owns the world for which the stub is generated),
    // we re-generation of stub with inlined types and copy the inlined stub to the destination
    if internal::dest_owns_stub_world(&world_name, &destination_wit_root) {
        let stub_root = &args
            .stub_wit_root
            .parent()
            .ok_or(anyhow!("Failed to get parent of stub wit root"))?;

        // We re-generate stub instead of copying it and inline types
        let stub_definition = StubDefinition::new(
            &args.dest_wit_root,
            stub_root,
            &Some(world_name),
            "0.0.1", // Version is unused when it comes to re-generating stub at this stage.
            &WasmRpcOverride::default(), // wasm-rpc path is unused when it comes to re-generating stub during dependency addition
            true,
        )?;

        // We filter the dependencies of stub that's already existing in dest_wit_root
        let filtered_source_deps = source_deps
            .into_iter()
            .filter(|dep| internal::find_if_same_package(dep, &destination_wit_root).unwrap())
            .collect::<Vec<_>>();

        // New stub string
        let new_stub = get_stub_wit(&stub_definition, StubTypeGen::InlineRootTypes)
            .context("Failed to regenerate inlined stub")?;

        let main_wit_package_name = wit::get_package_name(&main_wit)?;

        for source_dir in filtered_source_deps {
            actions.push(WitAction::CopyDepDir { source_dir })
        }

        actions.push(WitAction::WriteWit {
            source_wit: new_stub,
            dir_name: format!(
                "{}_{}",
                main_wit_package_name.namespace, main_wit_package_name.name
            ),
            file_name: "_stub.wit".to_string(),
        });
    } else {
        let main_wit_package_name = wit::get_package_name(&main_wit)?;

        for source_dir in source_deps {
            let parsed = UnresolvedPackage::parse_dir(&source_dir)?;

            if internal::is_invalid_dependency(&destination_wit_root, &parsed) {
                println!("Skipping the copy of cyclic dependencies {}", parsed.name);
            } else {
                let entries = fs::read_dir(&source_dir)?;
                for entry in entries {
                    let dependency_wit_path = entry?.path();
                    let source_wit = internal::replace_self_imports_from_dependencies(
                        &dependency_wit_path,
                        &destination_wit_root,
                    )?;

                    let dependency_file_name = internal::get_file_name(&dependency_wit_path)?;
                    let dependency_directory_name = internal::get_file_name(&source_dir)?;

                    actions.push(WitAction::WriteWit {
                        source_wit,
                        dir_name: dependency_directory_name,
                        file_name: dependency_file_name,
                    });
                }
            }
        }

        actions.push(WitAction::CopyDepWit {
            source_wit: main_wit,
            dir_name: format!(
                "{}_{}",
                main_wit_package_name.namespace, main_wit_package_name.name
            ),
        });
    }

    let mut proceed = true;
    for action in &actions {
        if !verify_action(action, &args.dest_wit_root, args.overwrite)? {
            eprintln!("Cannot {action} because the destination already exists with a different content. Use --overwrite to force.");
            proceed = false;
        }
    }

    if proceed {
        for action in &actions {
            action.perform(&args.dest_wit_root)?;
        }
    }

    if let Some(target_parent) = args.dest_wit_root.parent() {
        let target_cargo_toml = target_parent.join("Cargo.toml");
        if target_cargo_toml.exists() && target_cargo_toml.is_file() {
            if !args.update_cargo_toml {
                eprintln!("Warning: the newly copied dependencies have to be added to {}. Use the --update-cargo-toml flag to update it automatically.", target_cargo_toml.to_string_lossy());
            } else {
                cargo::is_cargo_component_toml(&target_cargo_toml).context(format!(
                    "The file {target_cargo_toml:?} is not a valid cargo-component project"
                ))?;
                let mut names = Vec::new();
                for action in actions {
                    names.push(action.get_dep_dir_name()?);
                }
                cargo::add_dependencies_to_cargo_toml(&target_cargo_toml, &names)?;
            }
        } else if args.update_cargo_toml {
            return Err(anyhow!(
                "Cannot update {:?} file because it does not exist or is not a file",
                target_cargo_toml
            ));
        }
    } else if args.update_cargo_toml {
        return Err(anyhow!("Cannot update the Cargo.toml file because parent directory of the destination WIT root does not exist."));
    }

    Ok(())
}

pub fn compose(args: ComposeArgs) -> anyhow::Result<()> {
    let mut config = wasm_compose::config::Config::default();

    for stub_wasm in &args.stub_wasm {
        let stub_bytes = fs::read(stub_wasm)?;
        let stub_component = Component::<IgnoreAllButMetadata>::from_bytes(&stub_bytes)
            .map_err(|err| anyhow!(err))?;

        let state = AnalysisContext::new(stub_component);
        let stub_exports = state.get_top_level_exports().map_err(|err| {
            let AnalysisFailure { reason } = err;
            anyhow!(reason)
        })?;

        for export in stub_exports {
            if let AnalysedExport::Instance(instance) = export {
                config.dependencies.insert(
                    instance.name.clone(),
                    Dependency {
                        path: stub_wasm.clone(),
                    },
                );
            }
        }
    }

    let composer = wasm_compose::composer::ComponentComposer::new(&args.source_wasm, &config);
    let result = composer.compose()?;
    println!("Writing composed component to {:?}", args.dest_wasm);
    fs::write(&args.dest_wasm, result).context("Failed to write the composed component")?;
    Ok(())
}

pub fn initialize_workspace(
    args: InitializeWorkspaceArgs,
    stubgen_command: &str,
    stubgen_prefix: &[&str],
) -> anyhow::Result<()> {
    make::initialize_workspace(
        &args.targets,
        &args.callers,
        args.wasm_rpc_override,
        stubgen_command,
        stubgen_prefix,
    )
}

mod internal {
    use anyhow::anyhow;
    use regex::Regex;
    use std::fs;
    use std::path::{Path, PathBuf};
    use wit_parser::UnresolvedPackage;

    pub(crate) fn find_if_same_package(
        dep_dir: &Path,
        target_wit: &UnresolvedPackage,
    ) -> anyhow::Result<bool> {
        let dep_package_name = UnresolvedPackage::parse_dir(dep_dir)?.name;
        let dest_package = target_wit.name.clone();

        if dep_package_name != dest_package {
            Ok(true)
        } else {
            println!(
                "Skipping the copy of cyclic dependencies {}",
                dep_package_name
            );
            Ok(false)
        }
    }

    pub(crate) fn find_world_name(unresolved_package: UnresolvedPackage) -> anyhow::Result<String> {
        // In reality, there is only 1 interface in generated stub in 1 _stub.wit
        for (_, interface) in unresolved_package.interfaces {
            if let Some(name) = interface.name {
                if name.starts_with("stub-") {
                    let world_name = name.replace("stub-", "");
                    return Ok(world_name);
                }
            }
        }

        Err(anyhow!("Failed to find world name from the stub. The interface name in stub is expected to have the pattern stub-<world-name>"))
    }

    pub(crate) fn dest_owns_stub_world(
        stub_world_name: &str,
        destination_wit_root: &UnresolvedPackage,
    ) -> bool {
        destination_wit_root
            .worlds
            .iter()
            .map(|(_, world)| world.name.clone())
            .collect::<Vec<_>>()
            .contains(&stub_world_name.to_string())
    }

    // When copying the wit files of the target to the packages wit/deps in the source, we need to ensure
    // these dependencies are not the source itself, or it's stub version
    // For cases where adding the stub dependency to its own package is valid (i.e, in case of self-loop/ direct-cycle dependency)
    // this function is/should-never-be called because, in this case destination owns the stub world (the stub to be copied) already
    // and forms a different branch of logic.
    pub(crate) fn is_invalid_dependency(
        destination_wit_root: &UnresolvedPackage,
        dependency_package: &UnresolvedPackage,
    ) -> bool {
        let self_stub_name = format!("{}-stub", destination_wit_root.name.name);

        dependency_package.name == destination_wit_root.name
            || (dependency_package.name.namespace == destination_wit_root.name.namespace
                && dependency_package.name.name == self_stub_name)
    }

    // For those dependencies we add to the source, if they are importing from the skipped/invalid dependencies
    // we simply make sure to delete them
    pub(crate) fn replace_self_imports_from_dependencies(
        dependency_wit_path: &PathBuf,
        destination_wit_root: &UnresolvedPackage,
    ) -> anyhow::Result<String> {
        let read_data = fs::read_to_string(dependency_wit_path)?;
        let self_stub_package = format!(
            "{}:{}-stub",
            destination_wit_root.name.namespace, destination_wit_root.name.name
        );

        let re = Regex::new(
            format!(
                r"import\s+{}(/[^;]*)?;",
                regex::escape(self_stub_package.as_str())
            )
            .as_str(),
        )
        .unwrap();

        Ok(re.replace_all(&read_data, "").to_string())
    }

    pub fn get_file_name(path: &PathBuf) -> anyhow::Result<String> {
        let msg = format!(
            "Failed to get the file name of the dependency wit path {:?}",
            path
        );

        Ok(path
            .file_name()
            .ok_or(anyhow::Error::msg(msg.clone()))?
            .to_str()
            .ok_or(anyhow::Error::msg(msg))?
            .to_string())
    }
}
