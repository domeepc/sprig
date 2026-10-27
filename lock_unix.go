//go:build unix

package main

import (
	"os"
	"path/filepath"
	"syscall"
)

// lockRefs takes an exclusive lock shared by every process using this
// repository, so ref compare-and-swaps from several servers cannot interleave.
func (r *Repo) lockRefs() (func(), error) {
	f, err := os.OpenFile(filepath.Join(r.Dir, "refs.lock"), os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
		f.Close()
		return nil, err
	}
	return func() { f.Close() }, nil // closing releases the lock
}
