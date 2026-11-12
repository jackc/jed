These are explicit shared corrections after baseline
0b76dd6e9c2675b552eb8af9e820065351ffa4b3. They do not alter the frozen Corpus
resources, GeneratedSpec, or the baseline input fingerprint.

statistics_kmv_boundary.test
Source: spec/conformance/suites/query/statistics_kmv_boundary.test
Correction: K retained KMV hashes remain exact until a K+1st distinct hash appears;
duplicates and NULL rows do not turn a full exact set into an estimate.
SHA-256: bba8932e0e99b38d4e588367a5227d235b3e499753601971d5c6cd5034b07858
