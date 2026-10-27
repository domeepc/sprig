//go:build unix

package main

import (
	"bufio"
	"errors"
	"fmt"
	"io/fs"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/fsnotify/fsnotify"
)

// `sprig watch` keeps a list of paths that changed, so status and commit can
// re-check only those instead of stat-ing every file. Snapshot asks it over a
// unix socket in .sprig; with no watcher running, nothing changes.
//
// Protocol: the client sends "since <ns>\n", the time its stat cache was
// saved. The answer is "unknown\n" if the watcher cannot vouch for that whole
// period, or "ok\n" followed by one changed path per line and an empty line.

const watchSlack = int64(time.Second) // covers edits in the same timestamp tick as a cache save

// watchSock is the watcher's socket: in .sprig, or, when that path is too
// long for a unix socket, in a private per-user directory under the system
// temp dir. That directory must be ours and mode 0700, so nobody else can
// plant a socket that answers "nothing changed".
func (r *Repo) watchSock() (string, error) {
	if p := filepath.Join(r.Dir, "watch.sock"); len(p) <= 100 {
		return p, nil
	}
	dir := filepath.Join(os.TempDir(), fmt.Sprintf("sprig-%d", os.Getuid()))
	if err := os.Mkdir(dir, 0o700); err != nil && !errors.Is(err, fs.ErrExist) {
		return "", err
	}
	st, err := os.Lstat(dir)
	if err != nil {
		return "", err
	}
	if sys, ok := st.Sys().(*syscall.Stat_t); !st.IsDir() || st.Mode().Perm() != 0o700 || !ok || int(sys.Uid) != os.Getuid() {
		return "", fmt.Errorf("%s is not a private directory", dir)
	}
	p := filepath.Join(dir, HashObject("sock", []byte(r.Dir))[:16]+".sock")
	if len(p) > 100 {
		return "", fmt.Errorf("no usable socket path for %s", r.Dir)
	}
	return p, nil
}

// Watch serves change queries until stop is closed.
func (r *Repo) Watch(stop <-chan struct{}, ready func()) error {
	if r.Bare {
		return errors.New("bare repository has no working tree")
	}
	sock, err := r.watchSock()
	if err != nil {
		return err
	}
	w, err := fsnotify.NewWatcher()
	if err != nil {
		return err
	}
	defer w.Close()
	var mu sync.Mutex
	since := time.Now().UnixNano() // earliest time this watcher can vouch for
	changed := map[string]int64{}
	addTree := func(dir string) {
		filepath.WalkDir(dir, func(p string, d fs.DirEntry, err error) error {
			if err != nil || !d.IsDir() {
				return nil
			}
			if n := d.Name(); n == ".sprig" || n == ".git" {
				return filepath.SkipDir
			}
			if err := w.Add(p); err != nil {
				mu.Lock()
				since = time.Now().UnixNano() // can't see this directory: vouch for nothing older
				mu.Unlock()
			}
			return nil
		})
	}
	addTree(r.Root)

	os.Remove(sock)
	ln, err := net.Listen("unix", sock)
	if err != nil {
		return err
	}
	defer os.Remove(sock)
	go func() {
		<-stop
		ln.Close()
	}()
	go func() {
		for {
			select {
			case ev, ok := <-w.Events:
				if !ok {
					return
				}
				rel, err := filepath.Rel(r.Root, ev.Name)
				if err != nil {
					continue
				}
				rel = filepath.ToSlash(rel)
				if top, _, _ := strings.Cut(rel, "/"); top == ".sprig" || top == ".git" {
					continue
				}
				now := time.Now().UnixNano()
				mu.Lock()
				changed[rel] = now
				if len(changed) > 200000 { // keep memory bounded; older queries get "unknown"
					changed, since = map[string]int64{}, now
				}
				mu.Unlock()
				if ev.Has(fsnotify.Create) {
					if st, err := os.Lstat(ev.Name); err == nil && st.IsDir() {
						addTree(ev.Name) // files created before the watch was added are covered: the client walks this dir
					}
				}
			case _, ok := <-w.Errors: // e.g. queue overflow: events may be lost
				if !ok {
					return
				}
				mu.Lock()
				changed, since = map[string]int64{}, time.Now().UnixNano()
				mu.Unlock()
			}
		}
	}()
	if ready != nil {
		ready()
	}
	for {
		conn, err := ln.Accept()
		if err != nil {
			select {
			case <-stop:
				return nil
			default:
				return err
			}
		}
		go func() {
			defer conn.Close()
			conn.SetDeadline(time.Now().Add(time.Second))
			line, err := bufio.NewReader(conn).ReadString('\n')
			ts, perr := strconv.ParseInt(strings.TrimPrefix(strings.TrimSpace(line), "since "), 10, 64)
			if err != nil || perr != nil {
				return
			}
			bw := bufio.NewWriter(conn)
			mu.Lock()
			if ts-watchSlack < since {
				bw.WriteString("unknown\n")
			} else {
				bw.WriteString("ok\n")
				for p, t := range changed {
					if t >= ts-watchSlack {
						bw.WriteString(p + "\n")
					}
				}
				bw.WriteString("\n")
			}
			mu.Unlock()
			bw.Flush()
		}()
	}
}

// askWatcher returns the paths changed since the stat cache was saved, or
// ok=false when no watcher can vouch for that. running reports that a watcher
// answered, so the caller should save a fresh cache it can vouch for next time.
func (r *Repo) askWatcher(cache statCache) (dirty []string, ok, running bool) {
	if r.Bare || os.Getenv("SPRIG_NO_WATCH") != "" {
		return nil, false, false
	}
	sock, err := r.watchSock()
	if err != nil {
		return nil, false, false
	}
	conn, err := net.DialTimeout("unix", sock, 50*time.Millisecond)
	if err != nil {
		return nil, false, false
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(500 * time.Millisecond))
	fmt.Fprintf(conn, "since %d\n", cache.Written)
	br := bufio.NewReader(conn)
	l, err := br.ReadString('\n')
	if err != nil {
		return nil, false, false
	}
	if l != "ok\n" || cache.Written == 0 || len(cache.Files) == 0 {
		return nil, false, true
	}
	for {
		l, err := br.ReadString('\n')
		if err != nil {
			return nil, false, true // cut short: don't trust a partial list
		}
		if l == "\n" {
			return dirty, true, true
		}
		p := strings.TrimSuffix(l, "\n")
		if p == ".sprigignore" {
			return nil, false, true // what counts as a file changed: look at everything
		}
		dirty = append(dirty, p)
	}
}
