# kiln

kiln turns OCI container images into microVM images: one deterministic erofs filesystem per layer, stacked with overlayfs inside the guest. It runs natively on macOS and Linux, without root.

Status: milestone M1b. `kiln convert` works on local inputs. Registry pull and push come next, then `kiln run` (M3).

## Quickstart

```bash
cargo install --path crates/kiln

# A local OCI layout or `docker save` archive (Docker 25+ writes OCI-in-tar).
docker save --platform linux/arm64 php:8.4-cli -o php.tar
kiln convert --platform linux/arm64 --tag php:8.4-cli php.tar

kiln ls
kiln inspect php:8.4-cli
kiln gc
```

`convert` options:
- `--platform os/arch[/variant]`: repeatable; the default is the host's. Several platforms produce a multi-arch index.
- `--tag NAME`: the name in the store (default: `<file name>:latest`).
- `--ref NAME`: which image to take when the source holds several.
- `--max-layers N` (default 10): more app layers than this squashes the bottom ones into one.
- `--json`: a machine-readable summary.

A second convert of the same image is served from the layer cache. Changing a base layer reconverts only the layers that inherit attributes from it.

## Other commands

- `kiln import --from-store DIR NAME [--as NAME]` copies an image from another store (for example the macOS store mounted read-only into a Lima VM), verifying every blob.
- `kiln bench LAYOUT` measures cold, warm and changed-top-layer conversions and prints JSON.
- `--store DIR` (or `$KILN_HOME`) selects the store; the default is `~/.local/share/kiln`.

## Docs

- `docs/format.md`: the normative image format and erofs profile.
- `docs/superpowers/specs/2026-09-30-kiln-design.md`: the design.

kiln prints image-supplied strings (command lines, environment, error paths) with control characters removed.

License: Apache-2.0.
