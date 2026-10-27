//go:build unix

package main

import (
	"errors"
	"io/fs"
	"os"

	"golang.org/x/sys/unix"
)

// listDir reads a directory and stats its non-directory entries relative to
// the open directory, so the kernel resolves one name per file rather than
// the whole path.
func listDir(dir string) ([]dirent, error) {
	f, err := os.Open(dir)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	ents, err := f.ReadDir(-1)
	if err != nil {
		return nil, err
	}
	fd := int(f.Fd())
	out := make([]dirent, 0, len(ents))
	for _, d := range ents {
		e := dirent{name: d.Name(), isDir: d.IsDir()}
		if !e.isDir {
			var st unix.Stat_t
			if err := unix.Fstatat(fd, e.name, &st, unix.AT_SYMLINK_NOFOLLOW); err != nil {
				if errors.Is(err, unix.ENOENT) {
					continue // deleted while we looked
				}
				return nil, &fs.PathError{Op: "lstat", Path: dir + "/" + e.name, Err: err}
			}
			e.st = fstat{size: st.Size, mtime: st.Mtim.Nano()}
			switch st.Mode & unix.S_IFMT {
			case unix.S_IFREG:
				e.st.mode = fs.FileMode(st.Mode & 0o777)
			case unix.S_IFLNK:
				e.st.mode = fs.ModeSymlink
			case unix.S_IFDIR:
				e.isDir = true
			default:
				e.st.mode = fs.ModeIrregular
			}
		}
		out = append(out, e)
	}
	return out, nil
}
