use anyhow::{anyhow, bail};
use cargo_toml::{
    Dependency, DependencyDetail, DepsSet, Edition, Inheritable, LtoSetting, Manifest, Profile,
    Profiles, StripSetting,
};
use id_arena::Id;
use indexmap::IndexSet;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::fs;
use std::path::Path;
use toml::Value;
use wit_parser::*;

// https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md

fn visit<'a>(
    pkg: &'a UnresolvedPackage,
    deps: &'a BTreeMap<PackageName, UnresolvedPackage>,
    order: &mut IndexSet<PackageName>,
    visiting: &mut HashSet<&'a PackageName>,
) -> anyhow::Result<()> {
    if order.contains(&pkg.name) {
        return Ok(());
    }
    for (dep, _) in pkg.foreign_deps.iter() {
        if !visiting.insert(dep) {
            bail!("package depends on itself");
        }
        let dep = deps
            .get(dep)
            .ok_or_else(|| anyhow!("failed to find package `{dep}` in `deps` directory"))?;
        visit(dep, deps, order, visiting)?;
        assert!(visiting.remove(&dep.name));
    }
    assert!(order.insert(pkg.name.clone()));
    Ok(())
}

// Copied and modified from `wit-parser` crate
fn get_unresolved_packages(
    root_path: &Path,
) -> anyhow::Result<(UnresolvedPackage, Vec<UnresolvedPackage>)> {
    let root = UnresolvedPackage::parse_dir(root_path).unwrap();

    let mut deps = BTreeMap::new();
    let deps_path = root_path.join(Path::new("deps"));
    for dep_entry in fs::read_dir(deps_path).unwrap() {
        let dep_entry = dep_entry.unwrap();
        let dep = UnresolvedPackage::parse_path(&dep_entry.path()).unwrap();
        for src in dep.source_files() {
            println!("dep {dep_entry:?} source: {src:?}");
        }
        deps.insert(dep.name.clone(), dep);
    }

    // Perform a simple topological sort which will bail out on cycles
    // and otherwise determine the order that packages must be added to
    // this `Resolve`.
    let mut order = IndexSet::new();
    let mut visiting = HashSet::new();
    for pkg in deps.values().chain([&root]) {
        visit(&pkg, &deps, &mut order, &mut visiting)?;
    }

    let mut ordered_deps = Vec::new();
    for name in order {
        if let Some(pkg) = deps.remove(&name) {
            ordered_deps.push(pkg);
        }
    }

    Ok((root, ordered_deps))
}

fn main() {
    // TODO: inputs
    let root_path = Path::new("wasm-rpc-stubgen/example");
    let dest_root = Path::new("tmp/stubgen_out");
    let selected_world = Some("api");
    let stub_crate_version = "0.0.1".to_string();
    // ^^^

    let (root, deps) = get_unresolved_packages(root_path).unwrap();
    let root_package = root.name.clone();

    let mut resolve = Resolve::new();
    for unresolved in deps.iter().cloned() {
        resolve.push(unresolved).unwrap();
    }
    let root_id = resolve.push(root.clone()).unwrap();

    dump(&resolve);

    let world = resolve.select_world(root_id, selected_world).unwrap();
    let selected_world = resolve.worlds.get(world).unwrap().name.clone();

    let dest_wit_root = dest_root.join(Path::new("wit"));
    fs::create_dir_all(dest_root).unwrap();

    let stub_world_name = format!("wasm-rpc-stub-{}", selected_world);

    generate_stub_wit(
        &resolve,
        root_package.clone(),
        world,
        stub_world_name.clone(),
        &dest_wit_root.join(Path::new("_stub.wit")),
    )
    .unwrap();

    let mut all = deps.clone();
    all.push(root);
    for unresolved in all {
        println!("copying {:?}", unresolved.name);
        for source in unresolved.source_files() {
            let relative = source.strip_prefix(root_path).unwrap();
            let dest = dest_wit_root.join(relative);
            println!("Copying {source:?} to {dest:?}");
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(&source, &dest).unwrap();
        }
    }
    let wasm_rpc_wit = include_str!("../../wasm-rpc/wit/wasm-rpc.wit");
    let wasm_rpc_root = dest_wit_root.join(Path::new("deps/wasm-rpc"));
    fs::create_dir_all(&wasm_rpc_root).unwrap();
    fs::write(wasm_rpc_root.join(Path::new("wasm-rpc.wit")), wasm_rpc_wit).unwrap();

    println!("----");

    let mut final_resolve = Resolve::new();
    final_resolve.push_dir(&dest_wit_root).unwrap();
    dump(&final_resolve);

    println!("generating cargo.toml");
    generate_cargo_toml(
        &root_path,
        &dest_root.join("Cargo.toml"),
        selected_world,
        stub_crate_version,
        format!("{}:{}", root_package.namespace, root_package.name),
        stub_world_name,
        &deps,
    )
    .unwrap();
}

#[derive(Serialize)]
struct MetadataRoot {
    component: Option<ComponentMetadata>,
}

impl Default for MetadataRoot {
    fn default() -> Self {
        MetadataRoot { component: None }
    }
}

#[derive(Serialize)]
struct ComponentMetadata {
    package: String,
    target: ComponentTarget,
}

#[derive(Serialize)]
struct ComponentTarget {
    world: String,
    path: String,
    dependencies: HashMap<String, WitDependency>,
}

#[derive(Serialize)]
struct WitDependency {
    path: String,
}

fn generate_cargo_toml(
    root_path: &Path,
    target: &Path,
    name: String,
    version: String,
    package: String,
    stub_world_name: String,
    deps: &[UnresolvedPackage],
) -> anyhow::Result<()> {
    let mut manifest = Manifest::default();

    let mut wit_dependencies = HashMap::new();
    for dep in deps {
        let mut dirs = HashSet::new();
        for source in dep.source_files() {
            let relative = source.strip_prefix(root_path)?;
            let dir = relative
                .parent()
                .ok_or(anyhow!("Package source {source:?} has no parent directory"))?;
            dirs.insert(dir);
        }

        if dirs.len() != 1 {
            bail!("Package {} has multiple source directories", dep.name);
        }

        wit_dependencies.insert("golem:rpc".to_string(), WitDependency { path: "wit/deps/wasm-rpc".to_string() });
        wit_dependencies.insert(
            format!("{}:{}", dep.name.namespace, dep.name.name),
            WitDependency {
                path: format!("wit/{}", dirs.iter().next().unwrap().to_str().unwrap().to_string()),
            },
        );
    }

    let metadata = MetadataRoot {
        component: Some(ComponentMetadata {
            package: package.clone(),
            target: ComponentTarget {
                world: stub_world_name.clone(),
                path: "wit".to_string(),
                dependencies: wit_dependencies,
            },
        }),
    };

    let mut package = cargo_toml::Package::new(name, version);
    package.edition = Inheritable::Set(Edition::E2021);
    package.metadata = Some(metadata);
    manifest.package = Some(package);

    let lib = cargo_toml::Product {
        path: Some("src/lib.rs".to_string()),
        crate_type: vec!["cdylib".to_string()],
        ..Default::default()
    };
    manifest.lib = Some(lib);

    manifest.profile = Profiles {
        release: Some(Profile {
            lto: Some(LtoSetting::Fat),
            opt_level: Some(Value::String("s".to_string())),
            debug: None,
            split_debuginfo: None,
            rpath: None,
            debug_assertions: None,
            codegen_units: None,
            panic: None,
            incremental: None,
            overflow_checks: None,
            strip: Some(StripSetting::Symbols),
            package: BTreeMap::new(),
            build_override: None,
            inherits: None,
        }),
        ..Default::default()
    };

    let dep_wit_bindgen = Dependency::Detailed(Box::new(DependencyDetail {
        version: Some("0.17.0".to_string()),
        default_features: false,
        features: vec!["realloc".to_string()],
        ..Default::default()
    }));

    // TODO: configurable
    let dep_golem_wasm_rpc = Dependency::Detailed(Box::new(DependencyDetail {
        // version: Some("0.17.0".to_string()),
        path: Some("../../wasm-rpc".to_string()),
        default_features: false,
        features: vec!["stub".to_string()],
        ..Default::default()
    }));

    let mut deps = DepsSet::new();
    deps.insert("wit-bindgen".to_string(), dep_wit_bindgen);
    deps.insert("golem-wasm-rpc".to_string(), dep_golem_wasm_rpc);
    manifest.dependencies = deps;

    let cargo_toml = toml::to_string(&manifest)?;
    fs::write(target, cargo_toml)?;
    Ok(())
}

struct InterfaceStub {
    pub name: String,
    pub functions: Vec<FunctionStub>,
    pub imports: Vec<InterfaceStubImport>,
}

#[derive(Hash, PartialEq, Eq)]
struct InterfaceStubImport {
    pub name: String,
    pub path: String,
}

struct FunctionStub {
    pub name: String,
    pub params: Vec<FunctionParamStub>,
    pub results: FunctionResultStub,
}

struct FunctionParamStub {
    pub name: String,
    pub typ: Type,
}

enum FunctionResultStub {
    Single(Type),
    Multi(Vec<FunctionParamStub>),
}

impl FunctionResultStub {
    pub fn is_empty(&self) -> bool {
        match self {
            FunctionResultStub::Single(_) => false,
            FunctionResultStub::Multi(params) => params.is_empty(),
        }
    }
}

trait TypeExtensions {
    fn wit_type_string(&self, resolve: &Resolve) -> anyhow::Result<String>;
}

impl TypeExtensions for Type {
    fn wit_type_string(&self, resolve: &Resolve) -> anyhow::Result<String> {
        match self {
            Type::Bool => Ok("bool".to_string()),
            Type::U8 => Ok("u8".to_string()),
            Type::U16 => Ok("u16".to_string()),
            Type::U32 => Ok("u32".to_string()),
            Type::U64 => Ok("u64".to_string()),
            Type::S8 => Ok("s8".to_string()),
            Type::S16 => Ok("s16".to_string()),
            Type::S32 => Ok("s32".to_string()),
            Type::S64 => Ok("s64".to_string()),
            Type::Float32 => Ok("f32".to_string()),
            Type::Float64 => Ok("f64".to_string()),
            Type::Char => Ok("char".to_string()),
            Type::String => Ok("string".to_string()),
            Type::Id(type_id) => {
                let typ = resolve
                    .types
                    .get(*type_id)
                    .ok_or(anyhow!("type not found"))?;
                let name = typ.name.clone().ok_or(anyhow!("type has no name"))?;
                Ok(name)
            }
        }
    }
}

fn collect_stub_imports<'a>(
    types: impl Iterator<Item = (&'a String, &'a TypeId)>,
    resolve: &Resolve,
) -> anyhow::Result<Vec<InterfaceStubImport>> {
    let mut imports = Vec::new();

    for (name, typ) in types {
        println!("type {:?} -> {:?}", name, typ);
        let typ = resolve.types.get(*typ).unwrap();
        println!("  {:?}", typ);
        match typ.owner {
            TypeOwner::World(world_id) => {
                let world = resolve.worlds.get(world_id).unwrap();
                println!("  from world {:?}", world.name);
            }
            TypeOwner::Interface(interface_id) => {
                let interface = resolve.interfaces.get(interface_id).unwrap();
                let package = interface.package.and_then(|id| resolve.packages.get(id));
                let interface_name = interface.name.clone().unwrap_or("unknown".to_string());
                let interface_path = package
                    .map(|p| p.name.interface_id(&interface_name))
                    .unwrap_or(interface_name);
                println!("  from interface {}", interface_path);
                imports.push(InterfaceStubImport {
                    name: name.clone(),
                    path: interface_path,
                });
            }
            TypeOwner::None => {
                println!("  no owner");
            }
        }
    }

    Ok(imports)
}

fn collect_stub_interfaces(resolve: &Resolve, world: &World) -> anyhow::Result<Vec<InterfaceStub>> {
    let top_level_types = world
        .exports
        .iter()
        .filter_map(|(name, item)| match item {
            WorldItem::Type(t) => Some((name.clone().unwrap_name(), t)),
            _ => None,
        })
        .collect::<Vec<_>>();

    let top_level_functions = world
        .exports
        .iter()
        .filter_map(|(_, item)| match item {
            WorldItem::Function(f) => Some(f),
            _ => None,
        })
        .collect::<Vec<_>>();

    let mut interfaces = Vec::new();
    for (name, item) in &world.exports {
        match item {
            WorldItem::Interface(id) => {
                let interface = resolve
                    .interfaces
                    .get(*id)
                    .ok_or(anyhow!("exported interface not found"))?;
                let name = interface.name.clone().unwrap_or(String::from(name.clone()));
                let functions = collect_stub_functions(interface.functions.values())?;
                let imports = collect_stub_imports(interface.types.iter(), resolve)?;
                interfaces.push(InterfaceStub {
                    name,
                    functions,
                    imports,
                });
            }
            _ => {}
        }
    }

    if !top_level_functions.is_empty() {
        interfaces.push(InterfaceStub {
            name: String::from(world.name.clone()),
            functions: collect_stub_functions(top_level_functions.into_iter())?,
            imports: collect_stub_imports(top_level_types.iter().map(|(k, v)| (k, *v)), resolve)?,
        });
    }

    Ok(interfaces)
}

fn collect_stub_functions<'a>(
    functions: impl Iterator<Item = &'a Function>,
) -> anyhow::Result<Vec<FunctionStub>> {
    Ok(functions
        .filter(|f| f.kind == FunctionKind::Freestanding)
        .map(|f| {
            let mut params = Vec::new();
            for (name, typ) in &f.params {
                params.push(FunctionParamStub {
                    name: name.clone(),
                    typ: typ.clone(),
                });
            }

            let results = match &f.results {
                Results::Named(params) => {
                    let mut param_stubs = Vec::new();
                    for (name, typ) in params {
                        param_stubs.push(FunctionParamStub {
                            name: name.clone(),
                            typ: typ.clone(),
                        });
                    }
                    FunctionResultStub::Multi(param_stubs)
                }
                Results::Anon(single) => FunctionResultStub::Single(single.clone()),
            };

            FunctionStub {
                name: f.name.clone(),
                params,
                results,
            }
        })
        .collect())
}

fn generate_stub_wit(
    resolve: &Resolve,
    package_name: PackageName,
    world_id: Id<World>,
    target_world_name: String,
    target: &Path,
) -> anyhow::Result<()> {
    let world = resolve.worlds.get(world_id).unwrap();

    let mut out = String::new();

    writeln!(out, "package {};", package_name)?;
    writeln!(out, "")?;
    writeln!(out, "interface stub-{} {{", world.name)?;

    let interfaces = collect_stub_interfaces(resolve, world)?;
    let all_imports = interfaces
        .iter()
        .flat_map(|i| i.imports.iter())
        .collect::<IndexSet<_>>();

    writeln!(out, "  use golem:rpc/types@0.1.0.{{uri}};")?;
    for import in all_imports {
        writeln!(out, "  use {}.{{{}}};", import.path, import.name)?;
    }
    writeln!(out, "")?;

    for interface in interfaces {
        writeln!(out, "  resource {} {{", &interface.name)?;
        writeln!(out, "    constructor(location: uri);")?; // TODO: worker-uri
        for function in interface.functions {
            write!(out, "    {}: func(", function.name)?;
            for (idx, param) in function.params.iter().enumerate() {
                write!(
                    out,
                    "{}: {}",
                    param.name,
                    param.typ.wit_type_string(resolve)?
                )?;
                if idx < function.params.len() - 1 {
                    write!(out, ", ")?;
                }
            }
            write!(out, ")")?;
            if !function.results.is_empty() {
                write!(out, " -> ")?;
                match function.results {
                    FunctionResultStub::Single(typ) => {
                        write!(out, "{}", typ.wit_type_string(resolve)?)?;
                    }
                    FunctionResultStub::Multi(params) => {
                        write!(out, "(")?;
                        for (idx, param) in params.iter().enumerate() {
                            write!(
                                out,
                                "{}: {}",
                                param.name,
                                param.typ.wit_type_string(resolve)?
                            )?;
                            if idx < params.len() - 1 {
                                write!(out, ", ")?;
                            }
                        }
                        write!(out, ")")?;
                    }
                }
            }
            writeln!(out, ";")?;
        }
        writeln!(out, "  }}")?;
        writeln!(out, "")?;
    }

    writeln!(out, "}}")?;
    writeln!(out, "")?;

    writeln!(out, "world {} {{", target_world_name)?;
    writeln!(out, "  export stub-{};", world.name)?;
    writeln!(out, "}}")?;

    fs::write(target, out)?;
    Ok(())
}

fn dump(resolve: &Resolve) {
    for (id, world) in &resolve.worlds {
        println!("World {id:?}");
        for (key, item) in &world.exports {
            println!("  {key:?} -> {item:?}");

            match item {
                WorldItem::Interface(id) => {
                    if let Some(interface) = resolve.interfaces.get(*id) {
                        println!("    interface {:?}", &interface.name);

                        for (_, f) in &interface.functions {
                            println!("      function {:?}", f.name);
                            for (name, typ) in &f.params {
                                println!("        param {:?} -> {:?}", name, typ);
                            }
                            for typ in f.results.iter_types() {
                                println!("        result {:?}", typ);
                            }
                        }
                    }
                }
                WorldItem::Function(f) => {
                    println!("    function {:?}", f.name);
                    for (name, typ) in &f.params {
                        println!("      param {:?} -> {:?}", name, typ);
                    }
                    for typ in f.results.iter_types() {
                        println!("      result {:?}", typ);
                    }
                }
                WorldItem::Type(id) => {
                    let ty = resolve.types.get(*id);
                    println!("    type {:?}", ty.map(|t| &t.name));
                }
            }
        }
    }
}
