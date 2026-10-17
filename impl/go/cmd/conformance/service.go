package main

// The corpus through a page client (spec/pageservice/README.md §7): in `service` mode another core's
// process actor owns the database while each query record runs here as a page client of it, so
// rows, errors, plans, and costs are checked across the page service and across cores.

import (
	"bufio"
	"encoding/hex"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	jed "github.com/jackc/jed/impl/go"
)

// ownerActor is the other core's process actor, started once with `owner-idle` and told to open
// and close each corpus file's database as its owner.
type ownerActor struct {
	cmd    *exec.Cmd
	stdin  *bufio.Writer
	stdout *bufio.Reader
}

func startOwnerActor(core string) (*ownerActor, error) {
	root := filepath.Dir(filepath.Dir(suitesDir())) // spec/conformance/suites → repo root
	root = filepath.Dir(root)
	var argv []string
	switch core {
	case "rust":
		argv = []string{filepath.Join(root, "impl/rust/target/release/process_actor")}
	case "go":
		argv = []string{filepath.Join(root, "impl/go/process_actor")}
	case "node":
		argv = []string{"node", filepath.Join(root, "impl/ts/src/bin/process_actor.ts")}
	default:
		return nil, fmt.Errorf("JED_SERVICE_OWNER must be rust, go, or node; got %q", core)
	}
	argv = append(argv, "owner-idle", root)
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Stderr = os.Stderr
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("start %s owner actor: %w", core, err)
	}
	a := &ownerActor{cmd: cmd, stdin: bufio.NewWriter(stdin), stdout: bufio.NewReader(stdout)}
	line, err := a.stdout.ReadString('\n')
	if err != nil || line != "READY\n" {
		return nil, fmt.Errorf("%s owner actor did not start: %q %v", core, line, err)
	}
	return a, nil
}

func (a *ownerActor) command(name, argument string) error {
	if argument != "" {
		name += "\t" + argument
	}
	if _, err := a.stdin.WriteString(name + "\n"); err != nil {
		return err
	}
	if err := a.stdin.Flush(); err != nil {
		return err
	}
	line, err := a.stdout.ReadString('\n')
	if err != nil {
		return fmt.Errorf("owner actor: %w", err)
	}
	if !strings.HasPrefix(line, "OK") {
		status, rest, _ := strings.Cut(strings.TrimSuffix(line, "\n"), "\t")
		code, detail, _ := strings.Cut(rest, "\t")
		message, _ := hex.DecodeString(detail)
		return fmt.Errorf("owner actor %s failed: %s %s %s", name, status, code, message)
	}
	return nil
}

func (a *ownerActor) open(path string) error {
	return a.command("OWNER_OPEN", hex.EncodeToString([]byte(path)))
}

func (a *ownerActor) close() error { return a.command("OWNER_CLOSE", "") }

func (a *ownerActor) stop() {
	_ = a.command("CLOSE", "")
	_ = a.cmd.Wait()
}

// serviceOwner is the owner actor in service mode; nil in the memory and disk modes.
var serviceOwner *ownerActor

// clientQueries counts the query records a page client answered; ownerRetries those it refused as
// writes (25006) and the owner then ran.
var clientQueries, ownerRetries int

// serviceLocking is the file locking mode the harness opens with: the test-only page-service mode in
// service mode, the default otherwise.
func serviceLocking() jed.Locking {
	if serviceOwner != nil {
		return jed.LockingService
	}
	return jed.LockingAuto
}
