//go:build unix

package main

import (
	"os"
	"syscall"
)

// mmapFile maps a whole file read-only. The mapping stays valid after the
// file is deleted (a gc replacing the pack), until the process exits.
func mmapFile(f *os.File) ([]byte, error) {
	st, err := f.Stat()
	if err != nil || st.Size() == 0 || st.Size() > 1<<40 {
		return nil, err
	}
	return syscall.Mmap(int(f.Fd()), 0, int(st.Size()), syscall.PROT_READ, syscall.MAP_SHARED)
}
