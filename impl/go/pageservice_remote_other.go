//go:build !unix

package jed

// OpenRemote opens a database another machine serves over a transport command's stdio
// (spec/pageservice/README.md §5.3). The stream transport is Unix-only, like serving: here it fails
// 0A000.
func OpenRemote(command []string, name string, opts OpenOptions) (*Database, error) {
	if err := opts.validateDurations(); err != nil {
		return nil, err
	}
	if err := opts.normalizeLimits(); err != nil {
		return nil, err
	}
	return nil, newError(FeatureNotSupported, "remote databases are unavailable on this platform")
}
