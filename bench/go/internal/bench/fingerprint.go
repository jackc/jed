package bench

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// CorpusFingerprint is sha256_hex of datasets.toml's bytes — the staleness contract
// (spec/design/benchmarks.md §5). datasets.toml embeds generator_version, so a
// generation-behavior bump invalidates every engine at once.
func CorpusFingerprint(corpusDir string) (string, error) {
	b, err := os.ReadFile(filepath.Join(corpusDir, "datasets.toml"))
	if err != nil {
		return "", err
	}
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:]), nil
}

// SidecarPath is the fingerprint file next to a generated database file:
// <dataDir>/<dataset>.<engine>.fingerprint.
func SidecarPath(dataDir, dataset, engine string) string {
	return filepath.Join(dataDir, dataset+"."+engine+".fingerprint")
}

// ReadSidecar returns the stored fingerprint, or "" if absent.
func ReadSidecar(dataDir, dataset, engine string) string {
	b, err := os.ReadFile(SidecarPath(dataDir, dataset, engine))
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

// WriteSidecar records a fingerprint after a successful load.
func WriteSidecar(dataDir, dataset, engine, fingerprint string) error {
	return os.WriteFile(SidecarPath(dataDir, dataset, engine), []byte(fingerprint+"\n"), 0o644)
}

// StaleErr is the uniform abort for a fingerprint mismatch (§5).
func StaleErr(dataset, engine string) error {
	return fmt.Errorf("stale benchmark data for %s/%s: run 'rake bench:setup'", dataset, engine)
}
