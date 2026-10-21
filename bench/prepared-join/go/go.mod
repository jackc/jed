module jed-prepared-join

go 1.26.0

require github.com/jackc/jed/impl/go v0.0.0

require (
	golang.org/x/crypto v0.58.0 // indirect
	golang.org/x/sys v0.49.0 // indirect
)

replace github.com/jackc/jed/impl/go => ../../../impl/go
