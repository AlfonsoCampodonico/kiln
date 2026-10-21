//! `kiln snapshot`: push, pull and inspect snapshot artifacts (design §5.1).
//! Every string from a snapshot is sanitised before printing (T8).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use kiln_proto::sanitize::clean_line;
use kiln_run::options::parse_size;
use kiln_snapshot::{DEFAULT_CHUNK_SIZE, Fetch, Loaded, assemble, chunk_bundle, load, pull, push, resolve, write_lazy};
use kiln_store::Store;
use serde_json::json;

use crate::{human_size, plural, reference_tag, registry};

#[derive(Subcommand)]
pub enum SnapshotCmd {
    /// Chunk a vmkit snapshot bundle into the store, tag it as REF, and push it.
    /// A name or digest already in the store is pushed as it is.
    Push {
        #[arg(value_name = "DIR|NAME")]
        source: String,
        #[arg(value_name = "REF")]
        reference: String,
        /// Chunk size: a power of two from 64K to 16M.
        #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value_t = DEFAULT_CHUNK_SIZE)]
        chunk_size: u64,
        #[arg(long)]
        json: bool,
    },
    /// Pull a snapshot, verifying every chunk, and write its bundle into DIR
    /// (created if missing, else it must be empty).
    Pull {
        #[arg(value_name = "REF")]
        reference: String,
        dir: PathBuf,
        /// Fetch no chunks: write only bundle.json and the chunk index lazy.json,
        /// for a restore that fetches chunks on demand.
        #[arg(long)]
        lazy: bool,
        /// Local name (default: the normalised reference).
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show a stored snapshot's VMM, architecture, memory, files and chunks.
    Inspect {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// `dest` must not exist or be an empty directory: checked before any download.
fn check_dest(dest: &Path) -> Result<()> {
    match std::fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!("{} is not empty", clean_line(&dest.display().to_string()));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot use {}", clean_line(&dest.display().to_string()))),
    }
}

/// A string at `path` in the bundle, sanitised; `?` when absent.
fn bundle_str(loaded: &Loaded, path: &[&str]) -> String {
    let mut v = &loaded.config.bundle;
    for key in path {
        v = &v[*key];
    }
    match v {
        serde_json::Value::String(s) => clean_line(s),
        serde_json::Value::Number(n) => n.to_string(),
        _ => "?".into(),
    }
}

fn inspect(loaded: &Loaded, as_json: bool) -> Result<()> {
    let files: Vec<_> = loaded
        .config
        .files
        .iter()
        .map(|f| (f, f.chunks.iter().filter(|c| c.is_none()).count()))
        .collect();
    let stored_chunks = loaded.manifest.layers.len();
    if as_json {
        // JSON escapes control characters, so the bundle is printed as it is.
        let v = json!({
            "digest": loaded.digest.to_string(),
            "chunkSize": loaded.config.chunk_size,
            "bundle": loaded.config.bundle,
            "files": files.iter().map(|(f, zero)| json!({
                "name": f.name,
                "size": f.size,
                "chunks": f.chunks.len(),
                "zeroChunks": zero,
            })).collect::<Vec<_>>(),
            "logicalSize": loaded.logical_size(),
            "storedSize": loaded.stored_size(),
            "storedChunks": stored_chunks,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let row = |label: &str, value: String| println!("  {:<14}{value}", format!("{label}:"));
    println!("{}", loaded.digest);
    row(
        "vmm",
        format!(
            "{} {}",
            bundle_str(loaded, &["vmm", "name"]),
            bundle_str(loaded, &["vmm", "version"])
        ),
    );
    row("arch", bundle_str(loaded, &["arch"]));
    row("memory", format!("{} MiB", bundle_str(loaded, &["memory_mib"])));
    row("vcpus", bundle_str(loaded, &["vcpus"]));
    row("chunk size", human_size(loaded.config.chunk_size));
    row("logical size", human_size(loaded.logical_size()));
    row(
        "stored size",
        format!(
            "{} in {stored_chunks} distinct chunk{}",
            human_size(loaded.stored_size()),
            plural(stored_chunks)
        ),
    );
    println!("  files:");
    for (f, zero) in files {
        println!(
            "    {:<16} {:>10}  {} chunk{} ({zero} zero)",
            f.name,
            human_size(f.size),
            f.chunks.len(),
            plural(f.chunks.len())
        );
    }
    Ok(())
}

pub fn run(store: Store, cmd: &SnapshotCmd) -> Result<()> {
    match cmd {
        SnapshotCmd::Push {
            source,
            reference,
            chunk_size,
            json,
        } => {
            let (r, client) = registry(reference)?;
            let path = Path::new(source);
            let name = if path.is_dir() {
                let tag = reference_tag(None, &r)?;
                // Held across chunking and tagging, so GC cannot remove the new blobs in between.
                let _lock = store.lock_shared()?;
                let built = chunk_bundle(&store, path, *chunk_size)
                    .with_context(|| format!("chunking {}", clean_line(source)))?;
                kiln_snapshot::tag(&store, &tag, &built.manifest_digest)?;
                tag
            } else {
                source.clone()
            };
            let rep = push(&store, &client, &name, &r)?;
            if *json {
                let v = json!({"reference": r.to_string(), "digest": rep.digest.to_string(), "blobs": rep.blobs, "bytes": rep.bytes, "skipped": rep.skipped});
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!(
                "{} → {} ({} blobs, {} uploaded, {} already present)",
                rep.digest,
                clean_line(&r.to_string()),
                rep.blobs,
                human_size(rep.bytes),
                rep.skipped
            );
        }
        SnapshotCmd::Pull {
            reference,
            dir,
            lazy,
            tag,
            json,
        } => {
            let (r, client) = registry(reference)?;
            let tag = reference_tag(tag.as_deref(), &r)?;
            check_dest(dir)?;
            let fetch = if *lazy { Fetch::Lazy } else { Fetch::Eager };
            let pulled = pull(&store, &client, &r, &tag, fetch)?;
            if *lazy {
                write_lazy(&pulled.loaded, dir)?;
            } else {
                assemble(&store, &pulled.loaded, dir)?;
            }
            let rep = &pulled.report;
            if *json {
                let v = json!({"tag": tag, "digest": rep.digest.to_string(), "lazy": lazy, "blobs": rep.blobs, "bytes": rep.bytes, "skipped": rep.skipped});
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!(
                "{} → {} ({} blobs, {} downloaded{})",
                clean_line(&tag),
                rep.digest,
                rep.blobs,
                human_size(rep.bytes),
                if *lazy { ", chunks on demand" } else { "" }
            );
        }
        SnapshotCmd::Inspect { name, json } => {
            let loaded = load(&store, &resolve(&store, name)?)?;
            inspect(&loaded, *json)?;
        }
    }
    Ok(())
}
