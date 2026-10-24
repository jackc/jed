//go:build !unix

package jed

// OpenRemote opens a database another machine serves over a transport command's stdio
// (spec/pageservice/README.md §5.3). The stream transport is Unix-only, like serving: here it fails
// 0A000.
func OpenRemote(command []string, name string, opts OpenOptions) (*Database, error) {
	return nil, newError(FeatureNotSupported, "remote databases are unavailable on this platform")
}
