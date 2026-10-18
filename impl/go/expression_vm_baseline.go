//go:build expression_vm && expression_vm_baseline

package jed

// Keep the benchmark driver and experiment type layouts, but eliminate every
// evaluator hook just as an ordinary build does. This avoids charging only the
// tree baseline a runtime mode check per recursive node.
const expressionVMEnabled = false
