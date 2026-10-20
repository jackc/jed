//go:build jed_specialization

package jed

// The wrapper selects a precompiled loop once, at the existing cursor boundary.
// It shares the original source, meter, LIMIT/OFFSET state and close behavior;
// only immutable kernels are added. No input/output row is retained here.
type kernelScanCursor struct {
	*streamingCursor
	kernels *scanKernels
}

func specializeScanCursor(c *streamingCursor) cursor {
	if !specializationScan || specializationMode == "ordinary" || c.plan.kernels == nil || c.distinct || c.ahead != nil {
		return c
	}
	ks := c.plan.kernels
	if c.plan.filter != nil && ks.filter == nil {
		return c
	}
	for i := range c.plan.projections {
		if ks.project[i] == nil {
			return c
		}
	}
	return &kernelScanCursor{streamingCursor: c, kernels: ks}
}

func (c *kernelScanCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	if c.limit != nil && c.produced >= *c.limit {
		c.done = true
		c.seen.release()
		return nil, false, nil
	}
	for {
		// Same pull and resolve order as streamingCursor.nextRow. In particular,
		// poll only AFTER the source pull; LIMIT 0 and a full window pull nothing.
		row, ok, err := c.pull()
		if err != nil {
			return nil, false, err
		}
		if !ok {
			c.done = true
			c.seen.release()
			return nil, false, nil
		}
		if err := c.meter.Step(); err != nil {
			return nil, false, err
		}
		c.meter.Charge(costs.StorageRowRead)
		if c.point != nil {
			row, err = c.point.resolveColumns(row, c.plan.relMasks[0])
		} else {
			row, err = c.scan.resolveColumns(row, c.plan.relMasks[0])
		}
		if err != nil {
			return nil, false, err
		}
		if c.kernels.filter != nil {
			keep, err := c.kernels.filter.evalFilter(row, c.env, c.meter)
			if err != nil {
				return nil, false, err
			}
			if !keep {
				continue
			}
		}
		c.passed++
		if c.passed <= c.offset {
			continue
		}
		c.meter.Charge(costs.RowProduced)
		projected := make([]Value, len(c.plan.projections))
		for i := range projected {
			v, err := c.kernels.project[i].eval(row, c.env, c.meter)
			if err != nil {
				return nil, false, err
			}
			projected[i] = v
		}
		c.produced++
		return projected, true, nil
	}
}
