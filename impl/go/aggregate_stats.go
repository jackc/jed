package jed

import "math"

// The statistical aggregates (spec/design/aggregates.md §23): the variance and standard-deviation
// family over one argument. Over integer or decimal input they are exact: the fold keeps the count
// and the uncapped decimal sums of x and x², and finalization follows PostgreSQL's numeric
// computation (numeric_stddev_internal), so the result is PostgreSQL's numeric value digit for digit.
// Over float input they follow PostgreSQL's float8 transition (the Youngs-Cramer update of
// float8_accum) and return f64.

// momentSurfaces maps each one-argument statistical aggregate to its exact and float plans.
var momentSurfaces = map[string][2]aggPlan{
	"stddev_samp": {planStddevSamp, planStddevSampFloat},
	"stddev":      {planStddevSamp, planStddevSampFloat},
	"stddev_pop":  {planStddevPop, planStddevPopFloat},
	"var_pop":     {planVarPop, planVarPopFloat},
	"var_samp":    {planVarSamp, planVarSampFloat},
	"variance":    {planVarSamp, planVarSampFloat},
}

// momentStat is what a statistical plan finalizes: a sample (N − 1) or population (N) statistic, and
// a standard deviation or a variance.
type momentStat struct{ sample, stddev bool }

// momentStats gives each statistical plan's statistic.
var momentStats = map[aggPlan]momentStat{
	planStddevSamp: {sample: true, stddev: true}, planStddevSampFloat: {sample: true, stddev: true},
	planStddevPop: {stddev: true}, planStddevPopFloat: {stddev: true},
	planVarPop: {}, planVarPopFloat: {},
	planVarSamp: {sample: true}, planVarSampFloat: {sample: true},
}

// momentsExact is the exact one-argument moment state: the count of non-NULL inputs and the decimal
// sums of x and x².
type momentsExact struct {
	n       int64
	sx, sxx Decimal
}

// fold adds one input, skipping NULL. Each fold charges decimal_work for the square and the two
// additions, by the `*` and `+` formulas (spec/design/cost.md §3).
func (s *momentsExact) fold(v Value, m *costMeter) error {
	if v.IsNull() {
		return nil
	}
	x := toDecimal(v)
	x2 := x.mulExact(x)
	m.Charge(costs.DecimalWork * (workMul(x, x) - 1 + workLinear(s.sx, x) - 1 + workLinear(s.sxx, x2) - 1))
	if err := m.Guard(); err != nil {
		return err
	}
	s.sx = s.sx.AddUncapped(x)
	s.sxx = s.sxx.AddUncapped(x2)
	s.n++
	return nil
}

// variance is PostgreSQL's numeric_stddev_internal: NULL over no input (or one, for a sample); else
// (N·Σx² − (Σx)²) / (N·N) for the population or / (N·(N−1)) for a sample, divided at
// select_div_scale and rounded half away from zero, then (for a standard deviation) its square root
// at the same scale. A numerator that is not positive yields 0.
func (s *momentsExact) variance(sample, stddev bool) (Value, error) {
	if s.n == 0 || (sample && s.n <= 1) {
		return NullValue(), nil
	}
	n := decimalFromInt64(s.n)
	rscale := int64(s.sx.Scale) * 2
	numerator := n.mulVar(s.sxx, rscale).sub(s.sx.mulVar(s.sx, rscale))
	if numerator.Neg || numerator.IsZero() {
		return DecimalValue(decimalFromInt64(0)), nil
	}
	denominator := decimalFromInt64(s.n - 1)
	if !sample {
		denominator = n
	}
	denominator = n.mulExact(denominator)
	scale := int64(selectDivScale(numerator, denominator))
	result, err := numerator.divVar(denominator, scale)
	if err != nil {
		return NullValue(), err
	}
	if stddev {
		if result, err = result.sqrtVar(scale, nil); err != nil {
			return NullValue(), err
		}
	}
	result, err = result.CheckCap()
	if err != nil {
		return NullValue(), err
	}
	return DecimalValue(result), nil
}

// momentsFloat is PostgreSQL's float8_accum transition state: N, Σx, and Sxx (the sum of squared
// deviations from the running mean), all f64.
type momentsFloat struct {
	n, sx, sxx float64
}

// fold is float8_accum (and float4_accum, which widens to f64 first): the Youngs-Cramer update. A
// finite input whose update overflows to ±Inf traps 22003; an infinite or NaN input makes Sxx NaN.
// The explicit float64 conversions keep the products from being fused into multiply-adds, so every
// core performs PostgreSQL's exact sequence of rounded operations.
func (s *momentsFloat) fold(v Value) error {
	if v.IsNull() {
		return nil
	}
	x := v.asF64()
	n := s.n + 1
	sx := s.sx + x
	sxx := s.sxx
	if s.n > 0 {
		tmp := float64(x*n) - sx
		sxx += float64(tmp*tmp) / float64(n*s.n)
		if math.IsInf(sx, 0) || math.IsInf(sxx, 0) {
			if !math.IsInf(s.sx, 0) && !math.IsInf(x, 0) {
				return overflowFloatErr()
			}
			sxx = math.NaN()
		}
	} else if math.IsNaN(x) || math.IsInf(x, 0) {
		sxx = math.NaN()
	}
	s.n, s.sx, s.sxx = n, sx, sxx
	return nil
}

// variance is float8_var_pop / float8_var_samp / float8_stddev_pop / float8_stddev_samp: NULL over
// no input (or one, for a sample); else Sxx / N or Sxx / (N − 1), square-rooted for a standard
// deviation.
func (s *momentsFloat) variance(sample, stddev bool) Value {
	if s.n == 0 || (sample && s.n <= 1) {
		return NullValue()
	}
	d := s.n
	if sample {
		d = s.n - 1
	}
	r := s.sxx / d
	if stddev {
		r = math.Sqrt(r)
	}
	return Float64Value(r)
}

// The two-argument statistical aggregates (spec/design/aggregates.md §23): covariance, correlation,
// and the regr_* family over (Y, X) pairs whose arguments are both non-NULL. Like PostgreSQL, which
// defines them only over float8, they widen integer and decimal arguments to f64 and follow
// float8_regr_accum and its final functions, with an f64 result.

// pairStat is which statistic a two-argument plan finalizes.
type pairStat int

const (
	pairCovarPop pairStat = iota
	pairCovarSamp
	pairCorr
	pairRegrAvgx
	pairRegrAvgy
	pairRegrSxx
	pairRegrSyy
	pairRegrSxy
	pairRegrSlope
	pairRegrIntercept
	pairRegrR2
)

// pairSurfaces maps each two-argument statistical aggregate to its plan.
var pairSurfaces = map[string]aggPlan{
	"covar_pop":      planCovarPop,
	"covar_samp":     planCovarSamp,
	"corr":           planCorr,
	"regr_avgx":      planRegrAvgx,
	"regr_avgy":      planRegrAvgy,
	"regr_sxx":       planRegrSxx,
	"regr_syy":       planRegrSyy,
	"regr_sxy":       planRegrSxy,
	"regr_slope":     planRegrSlope,
	"regr_intercept": planRegrIntercept,
	"regr_r2":        planRegrR2,
}

// pairStats gives each two-argument plan its statistic.
var pairStats = map[aggPlan]pairStat{
	planCovarPop:      pairCovarPop,
	planCovarSamp:     pairCovarSamp,
	planCorr:          pairCorr,
	planRegrAvgx:      pairRegrAvgx,
	planRegrAvgy:      pairRegrAvgy,
	planRegrSxx:       pairRegrSxx,
	planRegrSyy:       pairRegrSyy,
	planRegrSxy:       pairRegrSxy,
	planRegrSlope:     pairRegrSlope,
	planRegrIntercept: pairRegrIntercept,
	planRegrR2:        pairRegrR2,
}

// pairsFloat is PostgreSQL's float8_regr_accum transition state: N, Σx, Sxx, Σy, Syy, and Sxy (the
// sums of squared and cross deviations from the running means), all f64.
type pairsFloat struct {
	n, sx, sxx, sy, syy, sxy float64
}

// fold is float8_regr_accum over one Row(Y, X) pair, skipping a pair with a NULL: the two-variable
// Youngs-Cramer update. Each argument is first widened to f64 as PostgreSQL's casts to float8 do (an
// integer to the nearest f64, a decimal by its correctly rounded conversion). A finite input whose update overflows to ±Inf traps
// 22003; an infinite or NaN input makes the dependent sums NaN. As for the one-argument fold, the
// explicit conversions keep each operation separately rounded.
func (s *pairsFloat) fold(v Value) error {
	f := *v.composite()
	if f[0].IsNull() || f[1].IsNull() {
		return nil
	}
	y, err := percentileInputF64(f[0])
	if err != nil {
		return err
	}
	x, err := percentileInputF64(f[1])
	if err != nil {
		return err
	}
	n := s.n + 1
	sx, sy := s.sx+x, s.sy+y
	sxx, syy, sxy := s.sxx, s.syy, s.sxy
	if s.n > 0 {
		tmpX := float64(x*n) - sx
		tmpY := float64(y*n) - sy
		scale := 1.0 / float64(n*s.n)
		sxx += float64(float64(tmpX*tmpX) * scale)
		syy += float64(float64(tmpY*tmpY) * scale)
		sxy += float64(float64(tmpX*tmpY) * scale)
		inf := math.IsInf
		if inf(sx, 0) || inf(sxx, 0) || inf(sy, 0) || inf(syy, 0) || inf(sxy, 0) {
			if ((inf(sx, 0) || inf(sxx, 0)) && !inf(s.sx, 0) && !inf(x, 0)) ||
				((inf(sy, 0) || inf(syy, 0)) && !inf(s.sy, 0) && !inf(y, 0)) ||
				(inf(sxy, 0) && !inf(s.sx, 0) && !inf(x, 0) && !inf(s.sy, 0) && !inf(y, 0)) {
				return overflowFloatErr()
			}
			if inf(sxx, 0) {
				sxx = math.NaN()
			}
			if inf(syy, 0) {
				syy = math.NaN()
			}
			if inf(sxy, 0) {
				sxy = math.NaN()
			}
		}
	} else {
		if math.IsNaN(x) || math.IsInf(x, 0) {
			sxx, sxy = math.NaN(), math.NaN()
		}
		if math.IsNaN(y) || math.IsInf(y, 0) {
			syy, sxy = math.NaN(), math.NaN()
		}
	}
	*s = pairsFloat{n: n, sx: sx, sxx: sxx, sy: sy, syy: syy, sxy: sxy}
	return nil
}

// finalize is PostgreSQL's float8 final function for the statistic (the table in aggregates.md §23).
// NULL over no pair.
func (s *pairsFloat) finalize(stat pairStat) Value {
	if s.n < 1 {
		return NullValue()
	}
	switch stat {
	case pairCovarPop:
		return Float64Value(s.sxy / s.n)
	case pairCovarSamp:
		if s.n < 2 {
			return NullValue()
		}
		return Float64Value(s.sxy / (s.n - 1))
	case pairCorr:
		if s.sxx == 0 || s.syy == 0 {
			return NullValue()
		}
		return Float64Value(s.sxy / math.Sqrt(float64(s.sxx*s.syy)))
	case pairRegrAvgx:
		return Float64Value(s.sx / s.n)
	case pairRegrAvgy:
		return Float64Value(s.sy / s.n)
	case pairRegrSxx:
		return Float64Value(s.sxx)
	case pairRegrSyy:
		return Float64Value(s.syy)
	case pairRegrSxy:
		return Float64Value(s.sxy)
	case pairRegrSlope:
		if s.sxx == 0 {
			return NullValue()
		}
		return Float64Value(s.sxy / s.sxx)
	case pairRegrIntercept:
		if s.sxx == 0 {
			return NullValue()
		}
		return Float64Value((s.sy - float64(s.sx*s.sxy)/s.sxx) / s.n)
	case pairRegrR2:
		if s.sxx == 0 {
			return NullValue()
		}
		if s.syy == 0 {
			return Float64Value(1)
		}
		return Float64Value(float64(s.sxy*s.sxy) / float64(s.sxx*s.syy))
	default:
		panic("unhandled pair statistic")
	}
}
