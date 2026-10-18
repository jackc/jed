//go:build expression_vm

package main

import (
	"os"

	jed "github.com/jackc/jed/impl/go"
)

func resetExpressionVMExperiment() {
	jed.SetExpressionVMExperimentForTooling(os.Getenv("JED_EXPR_VM_MODE") == "vm")
}
