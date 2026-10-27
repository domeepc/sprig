package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
)

// Batched object transfer: many objects per HTTP request instead of one.
// The wire format is a stream of records, each "<id> <type> <len>\n" followed
// by exactly <len> bytes of object data.

const (
	batchIDs   = 2000     // ids per request
	batchBytes = 64 << 20 // soft cap on one response or upload
)

type object struct {
	typ  string
	data []byte
}

func writeRecord(w io.Writer, id, typ string, data []byte) {
	fmt.Fprintf(w, "%s %s %d\n", id, typ, len(data))
	w.Write(data)
}

// readRecords parses a record stream, verifying every object's hash.
func readRecords(r io.Reader, fn func(id string, o object) error) error {
	br := bufio.NewReader(r)
	for {
		line, err := br.ReadString('\n')
		if err == io.EOF && line == "" {
			return nil
		}
		if err != nil {
			return fmt.Errorf("truncated object stream")
		}
		f := strings.Fields(line)
		if len(f) != 3 || !isID(f[0]) {
			return fmt.Errorf("bad record header %q", strings.TrimSpace(line))
		}
		n, err := strconv.Atoi(f[2])
		if err != nil || n < 0 || n > maxObjectSize {
			return fmt.Errorf("bad record size %q", f[2])
		}
		data := make([]byte, n)
		if _, err := io.ReadFull(br, data); err != nil {
			return fmt.Errorf("truncated object stream")
		}
		if HashObject(f[1], data) != f[0] {
			return fmt.Errorf("object %s corrupt: hash mismatch", short(f[0]))
		}
		if err := fn(f[0], object{f[1], data}); err != nil {
			return err
		}
	}
}

// handleGetObjects returns the requested objects it has, stopping early once
// the response is large; the client asks again for the rest.
func (s *Server) handleGetObjects(w http.ResponseWriter, req *http.Request) {
	if readLimited(w, req) {
		return
	}
	var body struct {
		IDs []string `json:"ids"`
	}
	if err := json.NewDecoder(http.MaxBytesReader(w, req.Body, 8<<20)).Decode(&body); err != nil {
		httpErr(w, 400, "bad json")
		return
	}
	if len(body.IDs) > batchIDs {
		httpErr(w, 400, "too many ids")
		return
	}
	for _, id := range body.IDs {
		if !isID(id) {
			httpErr(w, 400, "bad id %q", id)
			return
		}
	}
	if s.upstream != nil {
		var need []string
		for _, id := range body.IDs {
			if !s.repo.HasObject(id) {
				need = append(need, id)
			}
		}
		if len(need) > 0 {
			s.upstream.GetObjects(need, func(id string, o object) error {
				_, err := s.repo.PutObject(o.typ, o.data)
				return err
			})
		}
	}
	w.Header().Set("Content-Type", "application/octet-stream")
	bw := bufio.NewWriter(w)
	sent := 0
	for _, id := range body.IDs {
		if sent >= batchBytes {
			break
		}
		typ, data, err := s.repo.GetObject(id)
		if err != nil {
			continue // absent: the client notices and reports it
		}
		writeRecord(bw, id, typ, data)
		sent += len(data)
	}
	bw.Flush()
}

// handlePutObjects stores a record stream in order. Each object's
// dependencies must already be stored, here or earlier in the same stream.
func (s *Server) handlePutObjects(w http.ResponseWriter, req *http.Request) {
	if readLimited(w, req) {
		return
	}
	n := 0
	err := readRecords(http.MaxBytesReader(w, req.Body, batchBytes+maxObjectSize), func(id string, o object) error {
		if msg := s.checkObject(o.typ, o.data); msg != "" {
			return fmt.Errorf("%s: %s", short(id), msg)
		}
		if _, err := s.repo.PutObject(o.typ, o.data); err != nil {
			return err
		}
		n++
		return nil
	})
	if err != nil {
		httpErr(w, 409, "%v (stored %d objects before it)", err, n)
		return
	}
	writeJSON(w, 200, map[string]int{"stored": n})
}

// checkObject returns why an object may not be stored, or "".
func (s *Server) checkObject(typ string, data []byte) string {
	kids, err := Children(typ, data)
	if err != nil {
		return err.Error()
	}
	for _, k := range kids {
		if !s.repo.HasObject(k) {
			return "missing dependency " + short(k)
		}
	}
	return ""
}

// GetObjects downloads ids in batches, calling fn for each (hash-verified).
func (rm *Remote) GetObjects(ids []string, fn func(id string, o object) error) error {
	for len(ids) > 0 {
		chunk := ids[:min(len(ids), batchIDs)]
		body, _ := json.Marshal(map[string]any{"ids": chunk})
		data, _, err := rm.do("POST", "/v1/objects/get", body, nil)
		if err != nil {
			return err
		}
		got := map[string]bool{}
		if err := readRecords(bytes.NewReader(data), func(id string, o object) error {
			got[id] = true
			return fn(id, o)
		}); err != nil {
			return err
		}
		if len(got) == 0 {
			return fmt.Errorf("remote is missing object %s", short(chunk[0]))
		}
		var rest []string
		for _, id := range ids {
			if !got[id] {
				rest = append(rest, id)
			}
		}
		ids = rest
	}
	return nil
}

// PutObjects uploads objects in dependency order, in batches.
func (rm *Remote) PutObjects(ids []string, get func(id string) (object, error)) error {
	var buf bytes.Buffer
	flush := func() error {
		if buf.Len() == 0 {
			return nil
		}
		_, _, err := rm.do("POST", "/v1/objects/put", buf.Bytes(), map[string]string{"Content-Type": "application/octet-stream"})
		buf.Reset()
		return err
	}
	for _, id := range ids {
		o, err := get(id)
		if err != nil {
			return err
		}
		writeRecord(&buf, id, o.typ, o.data)
		if buf.Len() >= batchBytes {
			if err := flush(); err != nil {
				return err
			}
		}
	}
	return flush()
}
