//go:build !unix

package main

import "errors"

func (r *Repo) Watch(stop <-chan struct{}, ready func()) error {
	return errors.New("sprig watch needs a unix system")
}

func (r *Repo) askWatcher(cache statCache) (dirty []string, ok, running bool) {
	return nil, false, false
}
