mod codegen;
mod ir;
mod model;
mod naming;
mod schema;
mod spec;

use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::{borrow::Cow, path::Path};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG_PATH: &str = "openapirator.toml";

#[derive(Parser)]
#[command(
    about = "Generate a Rust reqwest client module from an OpenAPI 3.1 document",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate the client module described by the config file.
    Generate {
        /// Generator config.
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,

        /// Path to OpenAPI document (overrides config value).
        #[arg(short, long)]
        input: Option<PathBuf>,
    },
    /// Write a config file filled with default values.
    DumpConfig {
        /// Where to write the config.
        #[arg(default_value = DEFAULT_CONFIG_PATH)]
        path: PathBuf,

        /// Overwrite the file if it already exists.
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Deserialize, Serialize)]
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
impl Default for RusftfmtConfig {
    fn default() -> Self {
        RusftfmtConfig {
            enabled: true,
            rust_edition: Self::default_edition(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct GeneratorConfig {
    #[serde(default)]
    input: Option<PathBuf>,
    output: PathBuf,
    /// Where to write the `cargo add` script for the client's dependencies.
    /// Defaults to `<output>/DEPENDENCIES.sh`.
    #[serde(default, alias = "dependencies_toml_path")]
    dependencies_script_path: Option<PathBuf>,
    /// Merge structurally identical inline schemas into one Rust type. Off by default: every
    /// inline schema then gets its own type named after where it appears, so e.g. a response
    /// struct is never reused as the return type of an unrelated operation.
    #[serde(default)]
    dedup_types: bool,
    /// Derive `Default` for every struct whose fields all implement `Default`, not only for
    /// structs made of `Option` fields.
    #[serde(default)]
    derive_default_when_possible: bool,
}
impl GeneratorConfig {
    fn dependencies_script_path(&self) -> Cow<'_, Path> {
        if let Some(path) = self.dependencies_script_path.as_ref() {
            Cow::Borrowed(path)
        } else {
            Cow::Owned(self.output.join("DEPENDENCIES.sh"))
        }
    }
}
impl Default for GeneratorConfig {
    fn default() -> Self {
        GeneratorConfig {
            input: Some("openapi.json".into()),
            output: "src/api".into(),
            dependencies_script_path: None,
            dedup_types: false,
            derive_default_when_possible: false,
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
struct Config {
    generator: GeneratorConfig,
    #[serde(default)]
    rustfmt: RusftfmtConfig,
    #[serde(default)]
    suppress_warnings: bool,
}

/// Comment written above each key in `dump-config` output. Every serialized key must have an
/// entry here (checked by a test), so new config fields get documented.
const KEY_DOCS: &[(&str, &str)] = &[
    (
        "suppress_warnings",
        "Only print warnings about unsupported constructs, not debug output.",
    ),
    (
        "input",
        "OpenAPI 3.1 document (JSON). Can be overridden with `openapirator generate --input`.",
    ),
    (
        "output",
        "Directory that receives the generated module (mod.rs, types.rs, tags/).\n\
         The optional `dependencies_script_path` key overrides where the `cargo add` script\n\
         is written (default: \"<output>/DEPENDENCIES.sh\").",
    ),
    (
        "dedup_types",
        "Merge structurally identical inline schemas into one Rust type. Off by default: every\n\
         inline schema then gets its own type named after where it appears.",
    ),
    (
        "derive_default_when_possible",
        "Derive `Default` for every struct whose fields all implement `Default` (strings,\n\
         numbers, Vec, maps, Option, nested such structs), not only for all-optional structs.\n\
         Enums and file-upload fields never qualify.",
    ),
    (
        "enabled",
        "Run rustfmt on the generated files when it is available.",
    ),
    (
        "rust_edition",
        "Rust edition of the consuming crate, passed to `rustfmt --edition`.",
    ),
];

impl Config {
    /// The config file `dump-config` writes: `Config::default()` serialized, with the comments
    /// from [`KEY_DOCS`] inserted above each key. `None` fields are omitted by the serializer.
    fn dump_default() -> Result<String> {
        let body =
            toml::to_string_pretty(&Config::default()).context("serializing default config")?;
        let mut out = String::from(
            "# openapirator configuration. Paths are relative to the directory the tool runs in.\n\n",
        );
        for line in body.lines() {
            if let Some((key, _)) = line.split_once(" = ")
                && let Some((_, doc)) = KEY_DOCS.iter().find(|(k, _)| *k == key)
            {
                for d in doc.lines() {
                    out.push_str("# ");
                    out.push_str(d);
                    out.push('\n');
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        Ok(out)
    }
}

fn default_true() -> bool {
    true
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Generate { config, input } => generate(&config, input.as_deref()),
        Cmd::DumpConfig { path, force } => dump_config(&path, force),
    }
}

fn dump_config(path: &Path, force: bool) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    if force {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let mut file = match open.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            bail!(
                "{} already exists; pass --force to overwrite it",
                path.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    };
    std::io::Write::write_all(&mut file, Config::dump_default()?.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote default config to {}", path.display());
    Ok(())
}

fn generate(config_path: &Path, input_override: Option<&Path>) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path).with_context(|| {
        format!(
            "reading {} (create one with `openapirator dump-config`)",
            config_path.display()
        )
    })?;
    let Config {
        generator: gen_cfg,
        rustfmt: fmt_cfg,
        suppress_warnings,
    }: Config = toml::from_str(&config_text).with_context(|| {
        format!(
            "parsing {} (see `openapirator dump-config` for the expected layout)",
            config_path.display()
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

    let input_path = input_override
        .or(gen_cfg.input.as_deref())
        .ok_or(anyhow!("No input file path was provided"))?;
    let input = std::fs::read_to_string(input_path)
        .with_context(|| format!("reading {}", input_path.display()))?;

    let doc: serde_json::Value = serde_json::from_str(&input)
        .with_context(|| format!("parsing {}", input_path.display()))?;

    let model = model::build(&doc, gen_cfg.dedup_types)?;
    let files = codegen::generate(
        &model,
        &codegen::Options {
            derive_default_when_possible: gen_cfg.derive_default_when_possible,
        },
    )?;

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

    let deps_path = gen_cfg.dependencies_script_path();
    if let Some(parent) = deps_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(
        deps_path.as_ref(),
        codegen::dependencies_script().as_bytes(),
    )
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
    println!(
        "wrote {} (run it with `sh` in the consuming crate, or paste its commands)",
        deps_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn dumped_config_parses_back_to_the_defaults() {
        let text = Config::dump_default().unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        let d = Config::default();
        assert_eq!(parsed.suppress_warnings, d.suppress_warnings);
        assert_eq!(parsed.generator.input, d.generator.input);
        assert_eq!(parsed.generator.output, d.generator.output);
        assert_eq!(
            parsed.generator.dependencies_script_path,
            d.generator.dependencies_script_path
        );
        assert_eq!(parsed.generator.dedup_types, d.generator.dedup_types);
        assert_eq!(
            parsed.generator.derive_default_when_possible,
            d.generator.derive_default_when_possible
        );
        assert_eq!(parsed.rustfmt.enabled, d.rustfmt.enabled);
        assert_eq!(parsed.rustfmt.rust_edition, d.rustfmt.rust_edition);
    }

    #[test]
    fn every_dumped_key_is_documented_and_vice_versa() {
        let text = Config::dump_default().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let keys: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k))
            .collect();
        assert!(!keys.is_empty());
        for (i, line) in lines.iter().enumerate() {
            if line.contains(" = ") {
                assert!(lines[i - 1].starts_with("# "), "undocumented key: {line}");
            }
        }
        for (key, _) in KEY_DOCS {
            assert!(
                keys.contains(key),
                "KEY_DOCS entry `{key}` matches no serialized key"
            );
        }
    }
}
