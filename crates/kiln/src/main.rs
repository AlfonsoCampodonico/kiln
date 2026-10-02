//! The `kiln` CLI. Every string from an image is sanitised before printing (T8).
#![forbid(unsafe_code)]

mod bench;
mod sanitize;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kiln_image::{ConvertOptions, LocalRequest, Output, convert_local, import_image, load, resolve_name};
use kiln_oci::Platform;
use kiln_store::Store;
use serde_json::json;

use crate::sanitize::clean_line;

#[derive(Parser)]
#[command(name = "kiln", version, about = "Build microVM images from OCI images")]
struct Cli {
    /// Store directory (default: $KILN_HOME, else ~/.local/share/kiln).
    #[arg(long, global = true, value_name = "DIR")]
    store: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct ConvertArgs {
    /// Platform to convert (os/arch[/variant]); repeatable. Default: the host's.
    #[arg(long = "platform", value_name = "PLATFORM")]
    platforms: Vec<String>,
    /// Squash the bottom layers when there are more app layers than this.
    #[arg(long, default_value_t = 10)]
    max_layers: usize,
    /// Layers converted in parallel (default: available CPUs).
    #[arg(long)]
    jobs: Option<usize>,
}

impl ConvertArgs {
    fn options(&self) -> ConvertOptions {
        let mut o = ConvertOptions {
            max_layers: self.max_layers,
            ..Default::default()
        };
        if let Some(j) = self.jobs {
            o.jobs = j;
        }
        o
    }

    fn platforms(&self) -> Result<Vec<Platform>> {
        self.platforms
            .iter()
            .map(|p| Platform::parse(p).with_context(|| format!("invalid --platform {}", clean_line(p))))
            .collect()
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert a local OCI layout directory or `docker save` archive.
    Convert {
        path: PathBuf,
        /// Image to pick when the source holds several (its ref name).
        #[arg(long = "ref", value_name = "NAME")]
        source_ref: Option<String>,
        /// Name for the result (default: <file name>:latest).
        #[arg(long)]
        tag: Option<String>,
        #[command(flatten)]
        args: ConvertArgs,
        /// Print a JSON summary.
        #[arg(long)]
        json: bool,
    },
    /// Copy an image from another (possibly read-only) store, verifying every blob.
    Import {
        #[arg(long = "from-store", value_name = "DIR")]
        from_store: PathBuf,
        name: String,
        /// Local name (default: the same name).
        #[arg(long = "as", value_name = "NAME")]
        as_name: Option<String>,
    },
    /// Show an image's platforms, process and layers.
    Inspect {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List tagged images.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove blobs and cache entries no tag reaches.
    Gc,
    /// Measure convert performance on a local OCI layout (JSON on stdout).
    Bench {
        path: PathBuf,
        #[command(flatten)]
        args: ConvertArgs,
        /// Size of the synthetic changed top layer (for tests).
        #[arg(long, hide = true, default_value_t = bench::CHANGED_TOP_BYTES)]
        changed_top_bytes: usize,
    },
}

fn open_store(cli: &Cli) -> Result<Store> {
    let root = match &cli.store {
        Some(p) => p.clone(),
        None => Store::default_root()?,
    };
    Store::open(&root).with_context(|| format!("opening store {}", root.display()))
}

fn default_tag(path: &Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| kiln_store::check_ref_name(s).is_ok());
    match stem {
        Some(s) => Ok(format!("{s}:latest")),
        None => bail!(
            "cannot derive a name from {}; pass --tag",
            clean_line(&path.display().to_string())
        ),
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn convert_summary(out: &Output, tag: &str) -> serde_json::Value {
    json!({
        "tag": tag,
        "digest": out.digest.to_string(),
        "mediaType": out.media_type,
        "images": out.images.iter().map(|c| json!({
            "platform": c.platform.to_string(),
            "manifest": c.manifest_digest.to_string(),
            "squashed": c.squashed,
            "layers": c.layers.iter().map(|l| json!({
                "erofs": l.erofs.to_string(),
                "size": l.size,
                "cached": l.cached,
                "inherits": l.inherits,
                "sources": l.sources.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "warnings": l.warnings,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn run(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Convert {
            path,
            source_ref,
            tag,
            args,
            json,
        } => {
            let store = open_store(&cli)?;
            let tag = match tag {
                Some(t) => t.clone(),
                None => default_tag(path)?,
            };
            let platforms = args.platforms()?;
            let req = LocalRequest {
                source_ref: source_ref.as_deref(),
                platforms: &platforms,
                tag: Some(&tag),
            };
            let out = convert_local(&store, path, &req, &args.options())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&convert_summary(&out, &tag))?);
                return Ok(());
            }
            for c in &out.images {
                let cached = c.layers.iter().filter(|l| l.cached).count();
                print!(
                    "{}  {}  {} layers ({cached} cached)",
                    clean_line(&c.platform.to_string()),
                    c.manifest_digest,
                    c.layers.len()
                );
                if c.squashed > 0 {
                    print!(", bottom {} squashed", c.squashed);
                }
                println!();
                for w in c.layers.iter().flat_map(|l| &l.warnings) {
                    eprintln!("warning: {}", clean_line(w));
                }
            }
            println!("{tag} → {}", out.digest);
        }
        Cmd::Import {
            from_store,
            name,
            as_name,
        } => {
            let store = open_store(&cli)?;
            let src = Store::open_read_only(from_store)?;
            let as_name = as_name.as_deref().unwrap_or(name);
            let r = import_image(&store, &src, name, as_name)?;
            println!(
                "{as_name} → {} ({} blobs, {} copied)",
                r.digest,
                r.blobs_copied,
                human_size(r.bytes_copied)
            );
        }
        Cmd::Inspect { name, json } => {
            let store = open_store(&cli)?;
            let loaded = load(&store, &resolve_name(&store, name)?)?;
            if *json {
                let entries: Vec<_> = loaded
                    .entries
                    .iter()
                    .map(|(p, m)| json!({"platform": p.to_string(), "digest": m.digest.to_string(), "manifest": m.manifest, "config": m.config}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"digest": loaded.digest.to_string(), "index": loaded.is_index, "images": entries})
                    )?
                );
                return Ok(());
            }
            println!("{}{}", loaded.digest, if loaded.is_index { " (index)" } else { "" });
            for (p, m) in &loaded.entries {
                let pr = &m.config.process;
                let list = |v: &[String]| v.iter().map(|s| clean_line(s)).collect::<Vec<_>>().join(" ");
                println!("\n{}  {}", clean_line(&p.to_string()), m.digest);
                println!("  entrypoint: {}", list(&pr.entrypoint));
                println!("  cmd:        {}", list(&pr.cmd));
                println!("  workdir:    {}", clean_line(pr.working_dir.as_deref().unwrap_or("/")));
                println!("  user:       {}", clean_line(pr.user.as_deref().unwrap_or("root")));
                for e in &pr.env {
                    println!("  env:        {}", clean_line(e));
                }
                let reference = m
                    .config
                    .source
                    .reference
                    .as_deref()
                    .map(|r| format!(" {}", clean_line(r)))
                    .unwrap_or_default();
                println!(
                    "  source:     {}{reference} (unverified provenance)",
                    m.config.source.manifest_digest
                );
                println!("  layers:");
                for l in &m.manifest.layers {
                    let inherits = if l.annotation(kiln_image::types::ANN_INHERITS).is_some() {
                        "  inherits"
                    } else {
                        ""
                    };
                    println!("    {}  {:>10}{inherits}", l.digest, human_size(l.size));
                }
            }
        }
        Cmd::Ls { json } => {
            let store = open_store(&cli)?;
            let mut rows = Vec::new();
            for (name, digest) in store.refs()? {
                let (platforms, size) = match load(&store, &digest) {
                    Ok(l) => {
                        let ps: Vec<String> = l.entries.iter().map(|(p, _)| clean_line(&p.to_string())).collect();
                        let size: u64 = l
                            .entries
                            .iter()
                            .flat_map(|(_, m)| m.manifest.layers.iter().map(|d| d.size))
                            .sum();
                        (ps.join(","), size)
                    }
                    Err(e) => (format!("<{}>", clean_line(&e.to_string())), 0),
                };
                rows.push((name, digest, platforms, size));
            }
            if *json {
                let v: Vec<_> = rows
                    .iter()
                    .map(|(n, d, p, s)| json!({"name": n, "digest": d.to_string(), "platforms": p, "size": s}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("{:<32} {:<19} {:<24} SIZE", "NAME", "DIGEST", "PLATFORMS");
            for (n, d, p, s) in rows {
                println!("{n:<32} {:<19} {p:<24} {}", &d.to_string()[..19], human_size(s));
            }
        }
        Cmd::Gc => {
            // Source OCI blobs are not referenced by kiln images, so GC frees them;
            // a later convert re-verifies them from the source.
            let r = open_store(&cli)?.gc()?;
            println!(
                "removed {} blobs ({}) and {} cache entries",
                r.blobs_removed,
                human_size(r.bytes_freed),
                r.cache_entries_removed
            );
        }
        Cmd::Bench {
            path,
            args,
            changed_top_bytes,
        } => {
            let report = bench::run(path, &args.platforms()?, &args.options(), *changed_top_bytes)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

/// The error chain joined by `: `, skipping causes their parent already prints.
fn error_message(e: &anyhow::Error) -> String {
    let mut msg = String::new();
    for cause in e.chain() {
        let s = cause.to_string();
        if !msg.contains(&s) {
            if !msg.is_empty() {
                msg.push_str(": ");
            }
            msg.push_str(&s);
        }
    }
    msg
}

/// The single, sanitised stderr line for an error (newlines in image text cannot forge lines).
fn render_error(e: &anyhow::Error) -> String {
    format!("kiln: error: {}", clean_line(&error_message(e)))
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", render_error(&e));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_error_is_one_sanitised_line() {
        let e = anyhow::anyhow!("bad path \x1b[31mevil\nkiln: error: forged").context("converting");
        let s = render_error(&e);
        assert!(!s.contains('\x1b'));
        assert!(!s.contains('\n'));
        assert!(s.starts_with("kiln: error: converting: "));
        assert!(s.contains("[31mevil kiln: error: forged"));
    }
}
