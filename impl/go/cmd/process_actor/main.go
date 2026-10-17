// Command process_actor is the test-only line actor for the shared real-process corpus.
package main

import (
	"bufio"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	if err := run(); err != nil {
		replyError(err)
		os.Exit(1)
	}
}

func run() error {
	if len(os.Args) < 3 {
		return fmt.Errorf("usage: process_actor create|open PATH")
	}
	// Positional arguments after the path: the open timeout, then the page-service owner's
	// idle-in-transaction timeout (spec/design/page-service.md §11.1). key=value arguments set the
	// handoff options (spec/pageservice/README.md §6.5): ownership=prefer|yield|never, yield_grace,
	// shutdown_grace, idle_session (milliseconds), max_connections.
	timeout := uint64(5000)
	var idle *uint64
	var positional []string
	options := map[string]string{}
	for _, arg := range os.Args[3:] {
		if key, value, ok := strings.Cut(arg, "="); ok {
			options[key] = value
		} else {
			positional = append(positional, arg)
		}
	}
	if len(positional) > 0 {
		timeout, _ = strconv.ParseUint(positional[0], 10, 64)
	}
	if len(positional) > 1 {
		ms, _ := strconv.ParseUint(positional[1], 10, 64)
		idle = &ms
	}
	millis := func(key string) *uint64 {
		value, ok := options[key]
		if !ok {
			return nil
		}
		ms, _ := strconv.ParseUint(value, 10, 64)
		return &ms
	}
	ownership := jed.OwnershipPrefer
	switch options["ownership"] {
	case "yield":
		ownership = jed.OwnershipYield
	case "never":
		ownership = jed.OwnershipNever
	}
	maxConnections, _ := strconv.Atoi(options["max_connections"])
	var db *jed.Database
	var err error
	if os.Args[1] == "owner-idle" {
		// The corpus-through-a-client driver (spec/pageservice/README.md §7): open and close databases
		// as their page-service owner on command.
		return ownerIdle(os.Args[2])
	}
	// create/open use the default locking (the page service); open-exclusive is a non-serving owner.
	locking := jed.LockingAuto
	if os.Args[1] == "open-exclusive" {
		locking = jed.LockingExclusive
	}
	readOnly := os.Args[1] == "open-read-only"
	if os.Args[1] == "create" {
		db, err = jed.CreateDatabase(jed.CreateOptions{
			Path: os.Args[2], Locking: locking, FileLockTimeoutMs: &timeout, IdleInTransactionTimeoutMs: idle,
			IdleSessionTimeoutMs: millis("idle_session"), YieldGraceMs: millis("yield_grace"), ShutdownGraceMs: millis("shutdown_grace"), MaxConnections: maxConnections, Ownership: ownership,
		})
	} else {
		db, err = jed.OpenDatabaseWithOptions(os.Args[2], jed.OpenOptions{
			Locking: locking, FileLockTimeoutMs: &timeout, IdleInTransactionTimeoutMs: idle, ReadOnly: readOnly,
			IdleSessionTimeoutMs: millis("idle_session"), YieldGraceMs: millis("yield_grace"), ShutdownGraceMs: millis("shutdown_grace"), MaxConnections: maxConnections, Ownership: ownership,
		})
	}
	if err != nil {
		return err
	}
	defer db.Close()
	fmt.Println("READY")

	var reader *jed.Session
	var writer *jed.Session
	scanner := bufio.NewScanner(os.Stdin)
	// A command line carries hex-encoded SQL, which may exceed bufio's 64 KiB default token.
	scanner.Buffer(make([]byte, 64*1024), 16<<20)
	for scanner.Scan() {
		command, argument, _ := strings.Cut(scanner.Text(), "\t")
		var value string
		switch command {
		case "EXEC":
			_, err = db.Exec(context.Background(), sql(argument))
		case "ATTACH":
			parts := strings.SplitN(argument, "\t", 3)
			err = db.Attach(parts[0], jed.AttachFile(parts[2]), parts[1] == "1")
		case "DETACH":
			err = db.Detach(argument)
		case "QUERY_I64":
			value, err = queryI64(db, sql(argument))
		case "READ_OPEN":
			reader = db.ReadSession()
		case "READ_QUERY_I64":
			value, err = queryI64(reader, sql(argument))
		case "READ_CLOSE":
			if reader != nil {
				reader.Close()
				reader = nil
			}
		case "WRITE_OPEN":
			writer = db.Session(jed.SessionOptions{})
			ms, _ := strconv.ParseUint(argument, 10, 64)
			writer.SetLockTimeoutMs(ms)
			err = writer.Begin(true)
		case "WRITE_EXEC":
			_, err = writer.Exec(context.Background(), sql(argument))
		case "WRITE_COMMIT":
			err = writer.Commit()
		case "WRITE_ROLLBACK":
			err = writer.Rollback()
		case "COMPACT":
			name := argument
			if name == "" {
				name = "main"
			}
			err = db.Compact(name)
		case "MAX_STORAGE_BYTES":
			bytes, parseErr := strconv.ParseInt(argument, 10, 64)
			if parseErr != nil {
				panic("MAX_STORAGE_BYTES bytes: " + parseErr.Error())
			}
			err = db.SetMaxStorageBytes("main", bytes)
		case "STORAGE_BYTES":
			var bytes int64
			bytes, err = db.StorageBytes("main")
			value = strconv.FormatInt(bytes, 10)
		case "TIME_LOOKUPS", "TIME_SCAN", "TIME_COMMITS":
			// The page-service benchmark's timed loops (spec/design/benchmarks.md §12).
			value, err = timed(db, command, argument)
		case "HANGUP_NEXT_ACK":
			// The commit-resolution test hook (spec/pageservice/README.md §6.4).
			jed.HangUpNextPageServiceAckForTesting(db)
		case "CRASH_NEXT_ACK":
			// Exit instead of acknowledging the next client commit (spec/pageservice/README.md §6.5).
			jed.CrashBeforeNextPageServiceAckForTesting(db)
		case "ROLE":
			// The page-service role test hook (spec/pageservice/README.md §6.5): main's, or an
			// attachment's when the step names one.
			if argument == "" {
				value = jed.PageServiceRoleForTesting(db)
			} else {
				value = jed.PageServiceAttachmentRoleForTesting(db, argument)
			}
		case "TXID":
			value = strconv.FormatUint(db.Txid(), 10)
		case "PAGE_COUNT":
			value = strconv.FormatUint(uint64(db.PageCount()), 10)
		case "CLOSE":
			if reader != nil {
				reader.Close()
			}
			if writer != nil {
				writer.Close()
			}
			replyOK("")
			return db.Close()
		default:
			panic("unknown actor command " + command)
		}
		if err != nil {
			replyError(err)
			err = nil
		} else {
			replyOK(value)
		}
	}
	return scanner.Err()
}

type queryer interface {
	Query(context.Context, string, ...any) (*jed.Rows, error)
}

func queryI64(handle queryer, query string) (string, error) {
	rows, err := handle.Query(context.Background(), query)
	if err != nil {
		return "", err
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		values, err := rows.Values()
		if err != nil {
			return "", err
		}
		fields := make([]string, len(values))
		for i, value := range values {
			fields[i] = fmt.Sprint(value)
		}
		out = append(out, strings.Join(fields, ":"))
	}
	return strings.Join(out, ","), rows.Err()
}

func sql(value string) string {
	bytes, err := hex.DecodeString(value)
	if err != nil {
		panic("command SQL is not hex")
	}
	return string(bytes)
}

func replyOK(value string) {
	fmt.Printf("OK\t%s\n", value)
}

func replyError(err error) {
	var engine *jed.EngineError
	if errors.As(err, &engine) {
		fmt.Printf("ERR\t%s\t%s\n", engine.Code(), hex.EncodeToString([]byte(engine.Message)))
	} else {
		fmt.Printf("ERR\tXXXXX\t%s\n", hex.EncodeToString([]byte(err.Error())))
	}
}

// ownerIdle serves OWNER_OPEN <hex path> / OWNER_CLOSE / CLOSE: it owns one database at a time under
// the page service, so another core's conformance harness can run queries as its client.
// root is the repository root, whose spec bundles the owner loads.
func ownerIdle(root string) error {
	env, err := ownerEnvironment(root)
	if err != nil {
		return err
	}
	fmt.Println("READY")
	var db *jed.Database
	scanner := bufio.NewScanner(os.Stdin)
	// A command line carries hex-encoded SQL, which may exceed bufio's 64 KiB default token.
	scanner.Buffer(make([]byte, 64*1024), 16<<20)
	for scanner.Scan() {
		command, argument, _ := strings.Cut(scanner.Text(), "\t")
		var err error
		switch command {
		case "OWNER_OPEN":
			timeout := uint64(5000)
			db, err = jed.OpenDatabaseWithOptions(sql(argument), jed.OpenOptions{Locking: jed.LockingAuto, SkipFsync: true, FileLockTimeoutMs: &timeout, Environment: env})
		case "OWNER_CLOSE":
			if db != nil {
				err = db.Close()
				db = nil
			}
		case "CLOSE":
			if db != nil {
				_ = db.Close()
			}
			replyOK("")
			return nil
		default:
			panic("unknown owner-idle command " + command)
		}
		if err != nil {
			replyError(err)
		} else {
			replyOK("")
		}
	}
	return scanner.Err()
}

// ownerEnvironment loads the spec's Unicode and time-zone bundles: like any opener, an owner must
// resolve the collations a file's catalog references (XX002 otherwise), and the corpus files use these.
func ownerEnvironment(root string) (*jed.Environment, error) {
	b := jed.NewEnvironmentBuilder()
	unicode, err := os.ReadFile(filepath.Join(root, "spec", "collation", "fixtures", "unicode.jucd"))
	if err != nil {
		return nil, err
	}
	if err := b.LoadUnicodeData(unicode); err != nil {
		return nil, err
	}
	tz, err := os.ReadFile(filepath.Join(root, "spec", "tz", "fixtures", "tzdata.jtz"))
	if err != nil {
		return nil, err
	}
	if err := b.LoadTimeZoneData(tz); err != nil {
		return nil, err
	}
	return b.Build(), nil
}

// timed runs one of the page-service benchmark's loops (spec/design/benchmarks.md §12) and returns
// "<elapsed ns>:<checksum>". argument is "<iterations>[\t<rows>]". Probe k reads id
// (k * 7919) mod rows + 1, so every core runs the same workload; the checksum is the sum of the
// values read (TIME_LOOKUPS), the last scan's sum (TIME_SCAN), or 0 (TIME_COMMITS).
func timed(db *jed.Database, command, argument string) (string, error) {
	fields := strings.Split(argument, "\t")
	n, _ := strconv.ParseInt(fields[0], 10, 64)
	rows := int64(1)
	if len(fields) > 1 {
		rows, _ = strconv.ParseInt(fields[1], 10, 64)
	}
	ctx := context.Background()
	var checksum int64
	start := time.Now()
	switch command {
	case "TIME_LOOKUPS":
		stmt, err := db.Prepare("SELECT v FROM bench_ps WHERE id = $1")
		if err != nil {
			return "", err
		}
		for k := int64(0); k < n; k++ {
			var v int64
			if err := db.QueryRowPrepared(ctx, stmt, k*7919%rows+1).Scan(&v); err != nil {
				return "", err
			}
			checksum += v
		}
	case "TIME_SCAN":
		for k := int64(0); k < n; k++ {
			if err := db.QueryRow(ctx, "SELECT sum(v)::i64 FROM bench_ps").Scan(&checksum); err != nil {
				return "", err
			}
		}
	case "TIME_COMMITS":
		stmt, err := db.Prepare("UPDATE bench_ps SET v = v + 1 WHERE id = $1")
		if err != nil {
			return "", err
		}
		for k := int64(0); k < n; k++ {
			if _, err := db.ExecPrepared(ctx, stmt, k*7919%rows+1); err != nil {
				return "", err
			}
		}
	}
	return fmt.Sprintf("%d:%d", time.Since(start).Nanoseconds(), checksum), nil
}
