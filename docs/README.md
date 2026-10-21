# kiln documentation

## Reference

- [format.md](format.md): the normative specification. The kiln image (index, manifest, config, layer media types), the local store, the erofs profile (superblock, layout, inodes, xattrs, overlay markers, layer semantics), the guest (block devices, init layer, scratch disk, boot sequence, differences from Docker) and the control protocol (transport, framing, messages, session, exit codes).

## Guides

- [running.md](running.md): `kiln run`. A sequence diagram of one run, signals and exit codes, flags, networking and egress policy, persistent disks, which kernel and init boot, run state on disk, the Lima VM on macOS, and known limitations.
- [kubernetes.md](kubernetes.md): running pods in kiln VMs with the containerd shim (`containerd-shim-kiln-v1`). What works and what does not yet, node requirements, containerd and RuntimeClass configuration, a crictl walkthrough and the end-to-end script.
- [registries.md](registries.md): how kiln talks to registries. Caching of requests, credentials from Docker's config, https and address rules.
- [testing.md](testing.md): the test tiers and what each needs, building `kiln-init`, the boot suite on real VMs, and benchmarks.

## Design history

These are working documents. They describe intent at the time they were written, and some describe milestones that are not on `main` yet; where they disagree with the code or with `format.md`, the code and `format.md` win.

- [superpowers/specs/2026-09-30-kiln-design.md](superpowers/specs/2026-09-30-kiln-design.md): the kiln design. Goals, decisions, threat model, components, the build pipeline, the erofs profile, boot artifacts, the guest and the run path, and the milestone plan.
- [superpowers/specs/2026-10-05-kiln-shim-design.md](superpowers/specs/2026-10-05-kiln-shim-design.md): the containerd shim that runs kiln images as a Kubernetes RuntimeClass, with decisions D1–D4.
- [superpowers/specs/2026-10-21-kiln-shim-v1-design.md](superpowers/specs/2026-10-21-kiln-shim-v1-design.md): its first implementation (v1): the erofs snapshotter, the pod model, the tap attachment, the VM workers, and what is out of scope.
- [superpowers/plans/](superpowers/plans/): implementation plans per milestone:
  - [M1a: kiln-erofs](superpowers/plans/2026-10-01-m1a-kiln-erofs.md)
  - [M1b-1: local convert](superpowers/plans/2026-10-02-m1b1-local-convert.md)
  - [M1b-2: registries](superpowers/plans/2026-10-03-m1b2-registry.md)
  - [M2a: vmkit core](superpowers/plans/2026-10-02-m2a-vmkit-core.md)
  - [M2b: vmkit sandbox and networking](superpowers/plans/2026-10-02-m2b-vmkit-sandbox-net.md)
  - [M3a: the guest](superpowers/plans/2026-10-04-m3a-guest.md)
  - [M3b-1: kiln run](superpowers/plans/2026-10-06-m3b1-run.md)
  - [M3b-2: boot artifacts, persistent disks, networking](superpowers/plans/2026-10-07-m3b2-artifacts-persist-net.md)
  - [M3d: services](superpowers/plans/2026-10-11-m3d-services.md) (not implemented yet)
  - [kiln-shim v1](superpowers/plans/2026-10-21-kiln-shim-v1.md)
