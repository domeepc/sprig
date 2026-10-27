package main

import (
	"container/list"
	"sync"
)

// A process-wide cache of decoded objects. Objects are immutable and
// addressed by content, so a cached copy can never be stale. It matters most
// for packs, where reading one object can mean decoding a chain of deltas:
// with the cache, each base in a chain is decoded once per process.

type cacheEntry struct {
	key  string
	typ  string
	data []byte // the object
	body []byte // what deltas against it apply to (compact form for trees)
	tree *Tree  // parsed form, for trees
	size int
}

type objCache struct {
	mu   sync.Mutex
	max  int
	size int
	ll   *list.List
	m    map[string]*list.Element
}

var objects = &objCache{max: 64 << 20, ll: list.New(), m: map[string]*list.Element{}}

func (c *objCache) get(key string) (*cacheEntry, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if e, ok := c.m[key]; ok {
		c.ll.MoveToFront(e)
		return e.Value.(*cacheEntry), true
	}
	return nil, false
}

func (c *objCache) put(e *cacheEntry) {
	e.size = len(e.data) + len(e.body) + 64
	if e.body != nil && &e.body[0] == &e.data[0] {
		e.size -= len(e.body) // same bytes
	}
	if e.size > c.max/8 {
		return // one huge object should not flush everything else
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if old, ok := c.m[e.key]; ok {
		c.size -= old.Value.(*cacheEntry).size
		c.ll.Remove(old)
	}
	c.m[e.key] = c.ll.PushFront(e)
	c.size += e.size
	for c.size > c.max {
		last := c.ll.Back()
		le := last.Value.(*cacheEntry)
		c.ll.Remove(last)
		delete(c.m, le.key)
		c.size -= le.size
	}
}

// Parsed trees, cached separately by count: callers must not modify them.
var trees = struct {
	sync.Mutex
	m map[string]*Tree
}{m: map[string]*Tree{}}

func cachedTree(id string) (*Tree, bool) {
	trees.Lock()
	defer trees.Unlock()
	t, ok := trees.m[id]
	return t, ok
}

func cacheTree(id string, t *Tree) {
	trees.Lock()
	defer trees.Unlock()
	if len(trees.m) >= 50000 { // shortcut: dropped wholesale when full; an LRU would keep the hot part
		trees.m = map[string]*Tree{}
	}
	trees.m[id] = t
}

// dropCaches forgets every cached object, so the next read goes to disk.
func dropCaches() {
	objects.mu.Lock()
	objects.ll, objects.m, objects.size = list.New(), map[string]*list.Element{}, 0
	objects.mu.Unlock()
	trees.Lock()
	trees.m = map[string]*Tree{}
	trees.Unlock()
}
