//go:build !expression_vm

package jed

// The expression VM is an opt-in experiment, with no runtime switch in ordinary builds.
const expressionVMEnabled = false

type expressionVMType struct{}
type expressionVMState struct{}

func (*rExpr) setExpressionVMType(resolvedType) {}
func (*costMeter) expressionVMActive() bool     { return false }
func (*rExpr) evalExpressionVM(storedRow, *evalEnv, *costMeter, bool) (Value, error) {
	panic("expression VM is excluded from this build")
}
