//go:build !unix && !windows

package main

import (
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"time"
)

// lockRefs uses an exclusive-create lock file where no OS file lock exists.
// shortcut: a crashed holder leaves refs.lock behind; it is taken over after 30s.
func (r *Repo) lockRefs() (func(), error) {
	p := filepath.Join(r.Dir, "refs.lock")
	for start := time.Now(); ; {
		f, err := os.OpenFile(p, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o644)
		if err == nil {
			f.Close()
			return func() { os.Remove(p) }, nil
		}
		if !errors.Is(err, fs.ErrExist) {
			return nil, err
		}
		if st, err := os.Stat(p); err == nil && time.Since(st.ModTime()) > 30*time.Second {
			os.Remove(p)
		}
		if time.Since(start) > time.Minute {
			return nil, errors.New("timed out waiting for refs.lock")
		}
		time.Sleep(5 * time.Millisecond)
	}
}
