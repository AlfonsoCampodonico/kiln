// Command oracle applies one OCI layer tar into a directory exactly as
// containerd's overlayfs snapshotter does. Used by kiln's oracle tests.
//
// Usage: oracle <layer-dir> <layer.tar> [<parent-dir> ...]  (parents nearest first)
package main

import (
	"context"
	"fmt"
	"os"

	"github.com/containerd/containerd/v2/pkg/archive"
)

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: oracle <layer-dir> <layer.tar> [<parent-dir> ...]")
		os.Exit(2)
	}
	f, err := os.Open(os.Args[2])
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	defer f.Close()
	if err := os.MkdirAll(os.Args[1], 0o755); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	_, err = archive.Apply(context.Background(), os.Args[1], f,
		archive.WithConvertWhiteout(archive.OverlayConvertWhiteout),
		archive.WithParents(os.Args[3:]))
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
