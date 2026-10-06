//! The `kiln` CLI. Every string from an image is sanitised before printing (T8).
#![forbid(unsafe_code)]

mod bench;
mod run_cmd;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kiln_image::ImageError;
use kiln_image::{
    ConvertOptions, LocalRequest, Output, RegistryRequest, convert_local, convert_registry, import_image, load,
    pull_image, push_image, resolve_name,
};
use kiln_oci::{OciError, Platform};
use kiln_registry::{Client, DockerConfig, Reference, RegistryError};
use kiln_store::Store;
use serde_json::json;

use kiln_proto::sanitize::clean_line;

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
        let mut seen = std::collections::HashSet::new();
        self.platforms
            .iter()
            .filter(|p| seen.insert(p.as_str()))
            .map(|p| Platform::parse(p).with_context(|| format!("invalid --platform {}", clean_line(p))))
            .collect()
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert an image: a local OCI layout directory or `docker save` archive,
    /// or else a registry reference such as `php:8.4-cli`.
    ///
    /// An existing local path always wins over a reference of the same name;
    /// write the full reference (`docker.io/library/php`) to force the registry.
    /// A source that starts with `./`, `../`, `/` or `~`, or ends in `.tar`,
    /// `.tar.gz` or `.tgz`, is always a path and is never looked up in a registry.
    Convert {
        #[arg(value_name = "PATH|REF")]
        source: String,
        /// Image to pick when a local source holds several (its ref name).
        #[arg(long = "ref", value_name = "NAME")]
        source_ref: Option<String>,
        /// Name for the result (default: <file name>:latest for a path, the
        /// normalised reference for a registry image).
        #[arg(long)]
        tag: Option<String>,
        #[command(flatten)]
        args: ConvertArgs,
        /// Print a JSON summary.
        #[arg(long)]
        json: bool,
    },
    /// Pull a kiln image from a registry, verifying every blob.
    Pull {
        #[arg(value_name = "REF")]
        reference: String,
        /// Local name (default: the normalised reference).
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Push a kiln image to a registry.
    Push {
        name: String,
        #[arg(value_name = "REF")]
        reference: String,
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
    /// Boot an image in a microVM and run its command (Linux with KVM; on macOS,
    /// in a Lima VM).
    Run(Box<run_cmd::RunArgs>),
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

/// A registry reference, and a client for its registry with the Docker config's credentials.
fn registry(reference: &str) -> Result<(Reference, Client)> {
    let r = Reference::parse(reference)?;
    let c = client_for(&r)?;
    Ok((r, c))
}

fn client_for(r: &Reference) -> Result<Client> {
    Ok(Client::new(r.registry(), DockerConfig::from_env())?)
}

/// The tag for a registry image: `--tag`, else the normalised reference.
fn reference_tag(tag: Option<&str>, r: &Reference) -> Result<String> {
    match tag {
        Some(t) => {
            kiln_store::check_ref_name(t).context("invalid --tag")?;
            Ok(t.to_string())
        }
        None => {
            let t = r.to_string();
            kiln_store::check_ref_name(&t).context("cannot use the reference as a local name; pass --tag")?;
            Ok(t)
        }
    }
}

/// Whether a source is spelled like a path, so that it is never taken for a registry reference.
fn looks_like_path(source: &str) -> bool {
    ["./", "../", "/", "~"].iter().any(|p| source.starts_with(p))
        || [".tar", ".tar.gz", ".tgz"].iter().any(|s| source.ends_with(s))
}

/// What `kiln convert` was asked to convert, decided before anything is created.
enum Source<'a> {
    Local(&'a Path),
    Registry(Reference, Box<Client>),
}

fn classify_source<'a>(source: &'a str, source_ref: Option<&str>) -> Result<Source<'a>> {
    let path = Path::new(source);
    if path
        .try_exists()
        .with_context(|| format!("cannot access {}", clean_line(source)))?
    {
        return Ok(Source::Local(path));
    }
    if looks_like_path(source) {
        bail!("{}: no such file or directory", clean_line(source));
    }
    if source_ref.is_some() {
        bail!("--ref selects an image inside a local source; it does not apply to registry references");
    }
    let r = Reference::parse(source).with_context(|| {
        format!(
            "{} is not an existing path or a valid image reference",
            clean_line(source)
        )
    })?;
    let c = client_for(&r).map_err(|e| {
        let hint = e.downcast_ref::<RegistryError>().is_some_and(suggests_a_mistyped_path);
        e.context(no_local_path(&r, hint))
    })?;
    Ok(Source::Registry(r, Box::new(c)))
}

/// The registry error inside a convert error, if that is what it is.
fn registry_error(e: &ImageError) -> Option<&RegistryError> {
    match e {
        ImageError::Registry(r) | ImageError::Oci(OciError::Registry(r)) => Some(r),
        _ => None,
    }
}

/// Whether a registry error may just mean the source was meant as a local path
/// that does not exist: the image or its registry was not found, or was refused.
fn suggests_a_mistyped_path(e: &RegistryError) -> bool {
    matches!(
        e,
        RegistryError::NotFound { .. } | RegistryError::Unauthorized { .. } | RegistryError::Resolve { .. }
    )
}

/// The context of a failed registry convert: the reference, and that no local
/// path of that name exists either when the error suggests one was meant.
fn no_local_path(r: &Reference, hint: bool) -> String {
    let r = clean_line(&r.to_string());
    if hint {
        format!("{r} (no such local path either)")
    } else {
        r
    }
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

/// The plural suffix for a count of `n`.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
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
        "layersDownloaded": out.layers_downloaded,
        "bytesDownloaded": out.bytes_downloaded,
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
            source,
            source_ref,
            tag,
            args,
            json,
        } => {
            let platforms = args.platforms()?;
            let source = classify_source(source, source_ref.as_deref())?;
            let tag = match &source {
                Source::Local(path) => {
                    let tag = match tag {
                        Some(t) => t.clone(),
                        None => default_tag(path)?,
                    };
                    kiln_store::check_ref_name(&tag).with_context(|| format!("invalid --tag {}", clean_line(&tag)))?;
                    tag
                }
                Source::Registry(r, _) => reference_tag(tag.as_deref(), r)?,
            };
            let store = open_store(&cli)?;
            let out = match &source {
                Source::Local(path) => {
                    let req = LocalRequest {
                        source_ref: source_ref.as_deref(),
                        platforms: &platforms,
                        tag: Some(&tag),
                    };
                    convert_local(&store, path, &req, &args.options())?
                }
                Source::Registry(r, client) => {
                    let req = RegistryRequest {
                        platforms: &platforms,
                        tag: Some(&tag),
                    };
                    convert_registry(&store, client, r, &req, &args.options()).map_err(|e| {
                        if matches!(e, ImageError::KilnRemote(_)) {
                            // Already names the reference and says what to do.
                            return anyhow::Error::from(e);
                        }
                        let hint = registry_error(&e).is_some_and(suggests_a_mistyped_path);
                        anyhow::Error::from(e).context(no_local_path(r, hint))
                    })?
                }
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(&convert_summary(&out, &tag))?);
                return Ok(());
            }
            for c in &out.images {
                let cached = c.layers.iter().filter(|l| l.cached).count();
                print!(
                    "{}  {}  {} layer{} ({cached} cached)",
                    clean_line(&c.platform.to_string()),
                    c.manifest_digest,
                    c.layers.len(),
                    plural(c.layers.len())
                );
                if c.squashed > 0 {
                    print!(", bottom {} squashed", c.squashed);
                }
                println!();
                for w in c.layers.iter().flat_map(|l| &l.warnings) {
                    eprintln!("warning: {}", clean_line(w));
                }
            }
            if out.layers_downloaded > 0 {
                println!(
                    "downloaded {} layer{} ({})",
                    out.layers_downloaded,
                    plural(out.layers_downloaded),
                    human_size(out.bytes_downloaded)
                );
            }
            println!("{} → {}", clean_line(&tag), out.digest);
        }
        Cmd::Pull { reference, tag, json } => {
            let store = open_store(&cli)?;
            let (r, client) = registry(reference)?;
            let tag = reference_tag(tag.as_deref(), &r)?;
            let rep = pull_image(&store, &client, &r, &tag)?;
            if *json {
                let v = json!({"tag": tag, "digest": rep.digest.to_string(), "blobs": rep.blobs, "bytes": rep.bytes, "skipped": rep.skipped});
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!(
                "{} → {} ({} blobs, {} downloaded)",
                clean_line(&tag),
                rep.digest,
                rep.blobs,
                human_size(rep.bytes)
            );
        }
        Cmd::Push { name, reference, json } => {
            let store = open_store(&cli)?;
            let (r, client) = registry(reference)?;
            let rep = push_image(&store, &client, name, &r)?;
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
        Cmd::Import {
            from_store,
            name,
            as_name,
        } => {
            let store = open_store(&cli)?;
            let src = Store::open_read_only(from_store)?;
            let as_name = as_name.as_deref().unwrap_or(name);
            kiln_store::check_ref_name(as_name).with_context(|| format!("invalid name {}", clean_line(as_name)))?;
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
                // Names are validated when written, but refs.json may have been edited by hand.
                let n = clean_line(&n);
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
        Cmd::Run(_) => unreachable!("handled in main"),
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
    // Many-layer images keep a few files open per layer; macOS defaults to 256.
    // Best effort: the hard limit may already be the soft one.
    let _ = rlimit::increase_nofile_limit(u64::MAX);
    let cli = Cli::parse();
    if let Cmd::Run(args) = &cli.cmd {
        return match run_cmd::run(|| open_store(&cli), args) {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("{}", render_error(&e));
                ExitCode::from(run_cmd::EXIT_ERROR)
            }
        };
    }
    match run(cli) {
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
    fn only_missing_or_refused_images_suggest_a_mistyped_path() {
        let not_found = RegistryError::NotFound {
            what: "manifest",
            url: "u".into(),
            detail: String::new(),
        };
        let unauthorized = RegistryError::Unauthorized {
            url: "u".into(),
            status: 401,
            detail: String::new(),
        };
        let resolve = RegistryError::Resolve {
            host: "h".into(),
            source: std::io::Error::other("no such host"),
        };
        for e in [not_found, unauthorized, resolve] {
            assert!(suggests_a_mistyped_path(&e), "{e}");
            let e = ImageError::Oci(OciError::Registry(e));
            assert!(registry_error(&e).is_some_and(suggests_a_mistyped_path), "{e}");
        }
        assert!(!suggests_a_mistyped_path(&RegistryError::Schema1));
        assert!(registry_error(&ImageError::UnsupportedPlatform("linux/s390x".into())).is_none());
        let r = Reference::parse("php:8.4-cli").unwrap();
        assert_eq!(
            no_local_path(&r, true),
            "docker.io/library/php:8.4-cli (no such local path either)"
        );
        assert_eq!(no_local_path(&r, false), "docker.io/library/php:8.4-cli");
    }

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
