mod codegen;
mod ir;
mod model;
mod naming;
mod schema;
mod spec;

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use serde::Deserialize;

#[derive(Parser)]
#[command(about = "Generate a Rust reqwest client module from an OpenAPI 3.1 document")]
struct Cli {
    /// Generator config.
    #[clap(short, long, default_value = "openapirator.toml")]
    config: PathBuf,

    /// Path to OpenAPI document (overrides config value).
    #[clap(short, long)]
    input: Option<PathBuf>,
    // TODO: dry run
}

#[derive(Deserialize)]
struct RusftfmtConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "RusftfmtConfig::default_edition")]
    rust_edition: String,
}
impl RusftfmtConfig {
    fn default_edition() -> String {
        "2024".into()
    }
}

#[derive(Deserialize)]
struct GeneratorConfig {
    #[serde(default)]
    input: Option<PathBuf>,
    output: PathBuf,
    #[serde(default)]
    dependencies_toml_path: Option<PathBuf>,
}
impl GeneratorConfig {
    fn dependencies_toml_path(&self) -> Cow<'_, PathBuf> {
        if let Some(path) = self.dependencies_toml_path.as_ref() {
            Cow::Borrowed(path)
        } else {
            Cow::Owned(self.output.clone().join("DEPENDENCIES.toml"))
        }
    }
}

#[derive(Deserialize)]
struct Config {
    generator: GeneratorConfig,
    rustfmt: RusftfmtConfig,
    #[serde(default)]
    suppress_warnings: bool,
}

fn default_true() -> bool {
    true
}

fn main() -> Result<()> {
    let args = Cli::parse();
    let config_text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("reading {}", args.config.display()))?;
    let Config {
        generator: gen_cfg,
        rustfmt: fmt_cfg,
        suppress_warnings,
    }: Config = toml::from_str(&config_text).with_context(|| {
        format!(
            "parsing {} (expected e.g. `output_module_path = \"src/api\"`)",
            args.config.display()
        )
    })?;

    env_logger::Builder::new()
        .filter_level(if suppress_warnings {
            log::LevelFilter::Warn
        } else {
            log::LevelFilter::Debug
        })
        .format_timestamp(None)
        .init();

    let input_path = args
        .input
        .as_ref()
        .or(gen_cfg.input.as_ref())
        .ok_or(anyhow!("No input file path was provided"))?;
    let input = std::fs::read_to_string(input_path)
        .with_context(|| format!("reading {}", input_path.display()))?;

    let doc: serde_json::Value = serde_json::from_str(&input)
        .with_context(|| format!("parsing {}", input_path.display()))?;

    let model = model::build(&doc)?;
    let files = codegen::generate(&model)?;

    std::fs::create_dir_all(&gen_cfg.output)
        .with_context(|| format!("creating {}", gen_cfg.output.display()))?;
    let mut written: Vec<PathBuf> = Vec::new();
    for (name, content) in &files {
        let path = gen_cfg.output.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }

    let deps_path = gen_cfg.dependencies_toml_path();
    if let Some(parent) = deps_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(deps_path.as_ref(), codegen::DEPENDENCIES_TOML)
        .with_context(|| format!("writing {}", deps_path.display()))?;

    if fmt_cfg.enabled {
        match Command::new("rustfmt")
            .arg("--edition")
            .arg(&fmt_cfg.rust_edition)
            .args(&written)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => log::warn!("rustfmt exited with {status}; output left unformatted"),
            Err(e) => log::warn!("rustfmt not run ({e}); output left unformatted"),
        }
    }

    let live_types = model.types.iter().filter(|t| t.is_live()).count();
    println!(
        "wrote {} files to {} ({} types, {} operations, {} tags)",
        files.len(),
        gen_cfg.output.display(),
        live_types,
        model.operations.len(),
        model.tags.len()
    );
    println!("wrote dependency list to {}", deps_path.display());
    Ok(())
}
