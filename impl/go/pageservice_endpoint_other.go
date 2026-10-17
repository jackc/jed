//go:build !unix

package jed

// Windows serving is not yet specified (spec/design/page-service.md §14): `locking = service` fails
// 0A000 before any of these is reached.

import (
	"errors"
	"net"
)

func pageServiceSupported() bool { return false }

func currentUID() uint32 { return 0 }

func endpointPath(string, uint32, [16]byte) string { return "" }

func listenEndpoint(string) (net.Listener, error) {
	return nil, errors.New("page service endpoints are unsupported on this platform")
}

func dialEndpoint(string) (net.Conn, error) {
	return nil, errors.New("page service endpoints are unsupported on this platform")
}

func writeOwnerRecord(string, ownerRecord) error {
	return newError(FeatureNotSupported, "the page service is unavailable on this platform")
}

func readOwnerRecord(string) (ownerRecord, bool) { return ownerRecord{}, false }

func ownerRecordPath(dbPath string) string { return dbPath + ".lock/owner" }
