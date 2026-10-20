//go:build !jed_specialization

package jed

// The experiment has no retained state or runtime mode check in production builds.
type (
	specializationPlan struct{}
	scalarKernel       struct{}
)

func (*engine) prepareSpecialization(*selectPlan, []colType) {}
func (*selectPlan) filterKernel() *scalarKernel              { return nil }
func (*selectPlan) projectionKernel(int) *scalarKernel       { return nil }
func (*scalarKernel) eval(storedRow, *evalEnv, *costMeter) (Value, error) {
	panic("execution specialization is not built") // unreachable: selectors inline to nil
}
