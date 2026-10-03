# kiln

kiln turns OCI container images into microVM images: one deterministic erofs filesystem per layer, stacked with overlayfs inside the guest. It runs natively on macOS and Linux, without root.

Status: milestone M1b. `kiln convert` works on registry images and local inputs, and `kiln pull`/`kiln push` move kiln images through registries. `kiln run` comes next (M3).

## Quickstart

```bash
cargo install --path crates/kiln

# From a registry (Docker Hub here): only layers that are not cached are downloaded.
kiln convert php:8.4-cli --platform linux/arm64 --platform linux/amd64
kiln inspect php:8.4-cli

# Or a local OCI layout or `docker save` archive (Docker 25+ writes OCI-in-tar;
# drop --platform if your Docker's `save` doesn't have it).
docker save --platform linux/arm64 php:8.4-cli -o php.tar
kiln convert --platform linux/arm64 --tag php:8.4-cli php.tar

kiln ls
kiln inspect php:8.4-cli
kiln gc
```

`convert` options:
- `--platform os/arch[/variant]`: repeatable; the default is the host's. Several platforms produce a multi-arch index.
- `--tag NAME`: the name in the store (default: the normalised reference, such as `docker.io/library/php:8.4-cli`, or `<file name>:latest` for a path).
- `--ref NAME`: which image to take when a local source holds several. References compare normalised, so `php:8.4-cli` matches `docker.io/library/php:8.4-cli`.
- `--max-layers N` (default 10): more app layers than this squashes the bottom ones into one.
- `--json`: a machine-readable summary.

An argument that names an existing path is a local source; anything else must be an image reference. An argument starting with `./`, `../`, `/` or `~`, or ending in `.tar`, `.tar.gz` or `.tgz`, is always taken as a local path, and a full reference such as `docker.io/library/php` forces the registry. A second convert of the same image is served from the layer cache and costs the registry one manifest HEAD, which Docker Hub does not count against its pull limit. Changing a base layer reconverts only the layers that inherit attributes from it.

## Registries

- `kiln push NAME REF` uploads a kiln image (blobs the registry already has are skipped) and `kiln pull REF [--tag NAME]` downloads one, verifying every blob. Digests survive the round trip. `pull` accepts only kiln images (an index of at most 8 platforms); use `convert` for OCI images.
- An anonymous request that a registry still refuses after handing out a token usually means the repository does not exist (Docker Hub answers so), or that it is private and needs `docker login`. An empty username in the Docker config or from a credential helper means no credentials, as in Docker.
- Credentials come from Docker's config (`$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`): `auths`, `credsStore` and `credHelpers` (`docker-credential-*` on `PATH`), as `docker login` writes them. They are sent only to the registry's own origin and its token realm, never to redirect targets.
- Registries are spoken to over https. A registry on `localhost`, `127.0.0.0/8` or `::1` is always spoken to over plain http, as Docker does; a TLS registry reached through a localhost tunnel or port forward is therefore not supported.
- A tag is resolved with a manifest HEAD; manifests and configs already in the store are not downloaded again (they are content-addressed and were verified when stored), so a warm convert makes that single request. A cold one adds a GET of the manifest by the digest the HEAD reported.
- kiln contacts only the registry: descriptor `urls` are ignored, and redirects (at most 5), token realms and upload locations must be https and must not lead to loopback, link-local (cloud metadata), private, CGNAT, unspecified or reserved addresses (including site-local, 6to4 and local-use NAT64 IPv6 ranges). The exception is a registry with no public address at all: what it hands out may use the address classes it is in itself. Proxy settings are not used.
- Downloads are bounded by per-layer and per-image size limits.
- Commands that accept a name (`inspect`, `push`) also find an image by its normalised reference: `kiln inspect php:8.4-cli`.

## Other commands

- `kiln import --from-store DIR NAME [--as NAME]` copies an image from another store (for example the macOS store mounted read-only into a Lima VM), verifying every blob.
- `kiln bench LAYOUT` measures cold, warm and changed-top-layer conversions and prints JSON.
- `--store DIR` (or `$KILN_HOME`) selects the store; the default is `~/.local/share/kiln`.

## Docs

- `docs/format.md`: the normative image format and erofs profile.
- `docs/superpowers/specs/2026-09-30-kiln-design.md`: the design.

kiln prints image-supplied strings (command lines, environment, error paths) with control characters removed.

License: Apache-2.0.
