"""Compares two directory trees: types, modes, owners, mtimes, content, links, devices, xattrs.

Usage: compare_tree.py REFERENCE CANDIDATE. Exits 1 if they differ. Paths that
`docker export` adds for the container runtime are skipped.
"""
import hashlib
import os
import stat
import sys

SKIP_TOP = {"dev", "proc", "sys"}
SKIP = {".dockerenv", "etc/hosts", "etc/hostname", "etc/resolv.conf", "etc/mtab"}


def describe(path):
    st = os.lstat(path)
    m = st.st_mode
    e = [stat.S_IFMT(m), stat.S_IMODE(m), st.st_uid, st.st_gid]
    if not stat.S_ISDIR(m):
        e.append(int(st.st_mtime))
    if stat.S_ISREG(m):
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        e += [st.st_size, h.hexdigest()]
    if stat.S_ISLNK(m):
        e.append(os.readlink(path))
    if stat.S_ISCHR(m) or stat.S_ISBLK(m):
        e.append(st.st_rdev)
    names = os.listxattr(path, follow_symlinks=False)
    e.append(sorted((k, os.getxattr(path, k, follow_symlinks=False)) for k in names))
    return e


def walk(root):
    out = {}
    for dirpath, dirs, files in os.walk(root):
        for name in dirs + files:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            if rel in SKIP or rel.split("/")[0] in SKIP_TOP:
                continue
            out[rel] = describe(path)
    return out


def main():
    ref, cand = walk(sys.argv[1]), walk(sys.argv[2])
    diff = [k for k in sorted(set(ref) | set(cand)) if ref.get(k) != cand.get(k)]
    print(f"reference {len(ref)} entries, kiln {len(cand)} entries, {len(diff)} differ")
    for k in diff[:20]:
        print(f"  {k}\n    reference: {ref.get(k)}\n    kiln:      {cand.get(k)}")
    sys.exit(1 if diff else 0)


if __name__ == "__main__":
    main()
