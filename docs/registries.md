# Registries

kiln has its own minimal OCI distribution client (`kiln-registry`). It is used by `kiln convert` for registry references, by `kiln pull` and `kiln push` for kiln images, and to fetch the pinned kernel blobs.

## Commands

- `kiln convert REF` converts an OCI image from a registry. Only layers that are not cached are downloaded.
- `kiln push NAME REF` uploads a kiln image (blobs the registry already has are skipped) and `kiln pull REF [--tag NAME]` downloads one, verifying every blob. Digests survive the round trip. `pull` accepts only kiln images (an index of at most 8 platforms); use `convert` for OCI images.
- Commands that accept a name (`inspect`, `push`, `run`) also find an image by its normalised reference: `kiln inspect php:8.4-cli` finds `docker.io/library/php:8.4-cli`.

## Requests and caching

- A tag is resolved with a manifest HEAD; manifests and configs already in the store are not downloaded again (they are content-addressed and were verified when stored), so a warm convert makes that single request. A cold one adds a GET of the manifest by the digest the HEAD reported.
- A second convert of the same image is therefore served from the layer cache and costs the registry one manifest HEAD, which Docker Hub does not count against its pull limit.
- Downloads are bounded by per-layer and per-image size limits.

## Credentials

- Credentials come from Docker's config (`$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`): `auths`, `credsStore` and `credHelpers` (`docker-credential-*` on `PATH`), as `docker login` writes them.
- They are sent only to the registry's own origin and its token realm, never to redirect targets.
- An empty username in the Docker config or from a credential helper means no credentials, as in Docker.
- An anonymous request that a registry still refuses after handing out a token usually means the repository does not exist (Docker Hub answers so), or that it is private and needs `docker login`.

## Transport and address rules

- Registries are spoken to over https. A registry on `localhost`, `127.0.0.0/8` or `::1` is always spoken to over plain http, as Docker does; a TLS registry reached through a localhost tunnel or port forward is therefore not supported.
- kiln contacts only the registry: descriptor `urls` are ignored, and redirects (at most 5), token realms and upload locations must be https and must not lead to loopback, link-local (cloud metadata), private, CGNAT, unspecified or reserved addresses (including site-local, 6to4 and local-use NAT64 IPv6 ranges). The exception is a registry with no public address at all: what it hands out may use the address classes it is in itself.
- Proxy settings are not used. Behind an HTTPS proxy, `kiln convert --boot-layers none` avoids the kernel fetch (see [running.md](running.md#what-boots)), but registry inputs still need direct access.
