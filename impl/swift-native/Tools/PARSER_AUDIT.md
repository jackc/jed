# Frozen syntax audit

Run `mise run swift-native:parser-audit` from the repository. The task reads every
`.test` blob below `spec/conformance/suites` at baseline
`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`, extracts every statement/query SQL record,
and passes it to the handwritten Swift lexer/parser. No record is skipped based
on host directives, storage mode, or expected error. Concurrency commands that
are not statement/query records are outside this syntax audit.

Verified 2026-11-05 on Linux with Swift 6.4.2:

| Measure | Count |
| --- | ---: |
| Frozen corpus files examined | 749 |
| SQL records parsed or rejected | 21,525 |
| Successfully parsed records | 21,171 |
| Rejections matching the corpus SQLSTATE | 354 |
| Unexpected rejections | 0 |
| Parsed records whose eventual expected SQLSTATE is `42601` | 262 |

This is syntax coverage, not a SQL conformance pass. Resolution, type checking,
embedded-language parsing (for example JSONPath), and execution may produce an
expected error after SQL parsing succeeds. The task consequently permits parsing
an expected-error record, including the 262 expected `42601` cases. Any parser
rejection of an expected-success record, or any rejection with a different error
code, fails the task and prints the frozen file, source line, SQL, and error.
Results, types, costs, error fields, transaction behavior, and storage behavior
require the future shared conformance adapter and are not checked here.

The task compiles only the parser and its foundation/data sources directly with
`swiftc`. Its executable and module cache live under the experiment's ignored
`.build/parser-audit` directory; no default repository task invokes it.
