package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Signed changes. A change made locally (commit, merge) is signed over its
// own JSON. A landed change is built by the server, so the client signs what
// it asked for instead (base, edits, message, metadata) and the server keeps
// that request as the proof; verifying checks the signature and that the
// change touched nothing but the signed paths.

type Signature struct {
	Key  string     `json:"key"` // "ed25519:<base64 public key>"
	Sig  string     `json:"sig"` // base64
	Land *LandProof `json:"land,omitempty"`
	// Moved: signed paths the server edited at a new path, because someone
	// else had renamed them since the signed base. Not signed; checked
	// against history instead.
	Moved map[string]string `json:"moved,omitempty"`
}

type LandProof struct {
	Branch string            `json:"branch"`
	Base   string            `json:"base"`
	Files  map[string]string `json:"files"` // path -> blob id, "" = deleted
}

const keyPrefix = "ed25519:"

func keyPath() (string, error) {
	if p := os.Getenv("SPRIG_KEY"); p != "" {
		return p, nil
	}
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "sprig", "signing.key"), nil
}

// signingKey returns the user's private key, or nil if they have none.
func signingKey() (ed25519.PrivateKey, error) {
	p, err := keyPath()
	if err != nil {
		return nil, err
	}
	b, err := os.ReadFile(p)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	seed, err := base64.StdEncoding.DecodeString(strings.TrimSpace(string(b)))
	if err != nil || len(seed) != ed25519.SeedSize {
		return nil, fmt.Errorf("%s is not a sprig signing key", p)
	}
	return ed25519.NewKeyFromSeed(seed), nil
}

func generateKey() (string, string, error) {
	p, err := keyPath()
	if err != nil {
		return "", "", err
	}
	if _, err := os.Stat(p); err == nil {
		return "", "", fmt.Errorf("%s already exists", p)
	}
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return "", "", err
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o700); err != nil {
		return "", "", err
	}
	seed := base64.StdEncoding.EncodeToString(priv.Seed())
	if err := os.WriteFile(p, []byte(seed+"\n"), 0o600); err != nil {
		return "", "", err
	}
	return p, keyPrefix + base64.StdEncoding.EncodeToString(pub), nil
}

func changePayload(c *Change) []byte {
	u := *c
	u.Sig = nil
	if u.Parents == nil {
		u.Parents = []string{} // as PutChange stores it
	}
	b, _ := json.Marshal(&u)
	return append([]byte("sprig-change-v1\n"), b...)
}

func landPayload(p *LandProof, message, author string, meta Meta) []byte {
	b, _ := json.Marshal(map[string]any{"proof": p, "message": message, "author": author, "meta": meta})
	return append([]byte("sprig-land-v1\n"), b...)
}

func sign(key ed25519.PrivateKey, payload []byte) *Signature {
	return &Signature{
		Key: keyPrefix + base64.StdEncoding.EncodeToString(key.Public().(ed25519.PublicKey)),
		Sig: base64.StdEncoding.EncodeToString(ed25519.Sign(key, payload)),
	}
}

func checkSig(s *Signature, payload []byte) error {
	pub, err := base64.StdEncoding.DecodeString(strings.TrimPrefix(s.Key, keyPrefix))
	if err != nil || !strings.HasPrefix(s.Key, keyPrefix) || len(pub) != ed25519.PublicKeySize {
		return errors.New("malformed key")
	}
	sig, err := base64.StdEncoding.DecodeString(s.Sig)
	if err != nil || !ed25519.Verify(pub, payload, sig) {
		return errors.New("signature does not match")
	}
	return nil
}

// VerifyChange checks a change's signature. It returns the signing key, or
// an error saying why the change is not validly signed.
func (r *Repo) VerifyChange(c *Change) (string, error) {
	if c.Sig == nil {
		return "", errors.New("unsigned")
	}
	if c.Sig.Land == nil {
		return c.Sig.Key, checkSig(c.Sig, changePayload(c))
	}
	p := c.Sig.Land
	if err := checkSig(c.Sig, landPayload(p, c.Message, c.Author, c.Meta)); err != nil {
		return c.Sig.Key, err
	}
	if len(c.Parents) > 1 {
		return c.Sig.Key, errors.New("landed change has several parents")
	}
	parent := ""
	if len(c.Parents) == 1 {
		parent = c.Parents[0]
	}
	before, err := r.ChangeFlat(parent)
	if err != nil {
		return c.Sig.Key, err
	}
	after, err := r.Flatten(c.Tree)
	if err != nil {
		return c.Sig.Key, err
	}
	allowed := map[string]bool{}
	for f := range p.Files {
		allowed[f] = true
	}
	if len(c.Sig.Moved) > 0 {
		based, err := r.ChangeFlat(p.Base)
		if err != nil {
			return c.Sig.Key, err
		}
		for from, to := range c.Sig.Moved {
			_, signed := p.Files[from]
			_, inBase := based[from]
			_, stillThere := before[from]
			_, movedThere := before[to]
			if !signed || !inBase || stillThere || !movedThere {
				return c.Sig.Key, fmt.Errorf("unsupported move claim %s -> %s", from, to)
			}
			allowed[to] = true
		}
	}
	for _, d := range DiffFlat(before, after) {
		if !allowed[d.Path] {
			return c.Sig.Key, fmt.Errorf("server changed unsigned path %s", d.Path)
		}
	}
	return c.Sig.Key, nil
}
