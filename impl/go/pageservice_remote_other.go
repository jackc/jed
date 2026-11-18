//go:build !unix

package jed

// OpenRemote opens a database that another machine or process serves over the standard input
// and output of command. The stream transport is available only on Unix; on this platform
// OpenRemote fails with SQLSTATE 0A000.
func OpenRemote(command []string, name string, opts OpenOptions) (*Database, error) {
	if err := opts.validateDurations(); err != nil {
		return nil, err
	}
	if err := opts.normalizeLimits(); err != nil {
		return nil, err
	}
	return nil, newError(FeatureNotSupported, "remote databases are unavailable on this platform")
}
