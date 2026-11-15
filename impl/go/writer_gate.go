package jed

import "sync"

// writerGate is a zero-value-ready mutex with a cancelable wait. Uncontended
// calls allocate nothing; contended callers share a notification channel. A
// canceled waiter leaves no goroutine behind that could later acquire the gate.
type writerGate struct {
	mu      sync.Mutex
	held    bool
	changed chan struct{}
}

func (g *writerGate) Lock() { _ = g.lock(nil) }

func (g *writerGate) lock(done <-chan struct{}) error {
	for {
		g.mu.Lock()
		if err := doneErr(done); err != nil {
			g.mu.Unlock()
			return err
		}
		if !g.held {
			g.held = true
			g.mu.Unlock()
			return nil
		}
		if g.changed == nil {
			g.changed = make(chan struct{})
		}
		changed := g.changed
		g.mu.Unlock()
		select {
		case <-done:
			return doneErr(done)
		case <-changed:
		}
	}
}

func (g *writerGate) TryLock() bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.held {
		return false
	}
	g.held = true
	return true
}

func (g *writerGate) Unlock() {
	g.mu.Lock()
	defer g.mu.Unlock()
	if !g.held {
		panic("jed: unlock of unlocked writer gate")
	}
	g.held = false
	if g.changed != nil {
		close(g.changed)
		g.changed = nil
	}
}
