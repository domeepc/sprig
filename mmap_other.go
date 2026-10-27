//go:build !unix

package main

import "os"

func mmapFile(f *os.File) ([]byte, error) { return nil, nil }
