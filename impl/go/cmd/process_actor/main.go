// Command process_actor is the test-only line actor for the shared real-process corpus.
package main

import (
	"bufio"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"

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
	timeout := uint64(5000)
	if len(os.Args) > 3 {
		timeout, _ = strconv.ParseUint(os.Args[3], 10, 64)
	}
	var db *jed.Database
	var err error
	if os.Args[1] == "create" {
		db, err = jed.CreateDatabase(jed.CreateOptions{Path: os.Args[2], Locking: jed.LockingShared, FileLockTimeoutMs: &timeout})
	} else {
		db, err = jed.OpenDatabaseWithOptions(os.Args[2], jed.OpenOptions{Locking: jed.LockingShared, FileLockTimeoutMs: &timeout})
	}
	if err != nil {
		return err
	}
	defer db.Close()
	fmt.Println("READY")

	var reader *jed.Session
	var writer *jed.Session
	scanner := bufio.NewScanner(os.Stdin)
	for scanner.Scan() {
		command, argument, _ := strings.Cut(scanner.Text(), "\t")
		var value string
		switch command {
		case "EXEC":
			_, err = db.Exec(context.Background(), sql(argument))
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
