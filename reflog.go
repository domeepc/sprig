package main

import (
	"bufio"
	"encoding/json"
	"os"
	"path/filepath"
	"time"
)

// The server's reflog: one JSON line per ref change, appended under the ref
// lock. It is for recovery ("what did main point at before?") and auditing.

type reflogEntry struct {
	Time int64  `json:"time"`
	Ref  string `json:"ref"`
	Old  string `json:"old,omitempty"`
	New  string `json:"new,omitempty"` // empty: deleted
	Via  string `json:"via"`           // land | push | delete
	Who  string `json:"who,omitempty"` // landing author, or the kind of token
}

func (r *Repo) reflogPath() string { return filepath.Join(r.Dir, "reflog.jsonl") }

func (r *Repo) appendReflog(e reflogEntry) error {
	e.Time = time.Now().Unix()
	b, err := json.Marshal(e)
	if err != nil {
		return err
	}
	f, err := os.OpenFile(r.reflogPath(), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	_, err = f.Write(append(b, '\n'))
	if err == nil && r.Durable {
		err = f.Sync()
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	return err
}

func (r *Repo) Reflog() ([]reflogEntry, error) {
	f, err := os.Open(r.reflogPath())
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var out []reflogEntry
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		var e reflogEntry
		if json.Unmarshal(sc.Bytes(), &e) == nil { // a torn last line after a crash is skipped
			out = append(out, e)
		}
	}
	return out, sc.Err()
}
