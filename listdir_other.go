//go:build !unix

package main

import (
	"errors"
	"io/fs"
	"os"
)

func listDir(dir string) ([]dirent, error) {
	ents, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	out := make([]dirent, 0, len(ents))
	for _, d := range ents {
		e := dirent{name: d.Name(), isDir: d.IsDir()}
		if !e.isDir {
			info, err := d.Info()
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			if err != nil {
				return nil, err
			}
			e.st = statOf(info)
		}
		out = append(out, e)
	}
	return out, nil
}
