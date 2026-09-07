package jed

import "sort"

// Resolved expression dependencies for persisted indexes (index-dependencies.md).
func (d timeZoneDeps) empty() bool { return !d.Dynamic && len(d.Zones) == 0 }

func (d timeZoneDeps) validate() error {
	if len(d.Zones) > 65535 {
		return newError(ProgramLimitExceeded, "too many timezone dependencies or an oversized dependency name/version")
	}
	for _, dep := range d.Zones {
		if len(dep.Name) > 65535 || len(dep.Version) > 65535 {
			return newError(ProgramLimitExceeded, "too many timezone dependencies or an oversized dependency name/version")
		}
		if dep.Name == "" || dep.Version == "" {
			return newError(InvalidObjectDefinition, "index timezone dependencies require a nonempty name and version")
		}
	}
	return nil
}

func (d *timeZoneDeps) merge(other timeZoneDeps) {
	d.Dynamic = d.Dynamic || other.Dynamic
	for _, next := range other.Zones {
		found := false
		for _, old := range d.Zones {
			if old == next {
				found = true
				break
			}
		}
		if !found {
			d.Zones = append(d.Zones, next)
		}
	}
	sort.Slice(d.Zones, func(i, j int) bool { return d.Zones[i].Name < d.Zones[j].Name })
}

func (p *paramTypes) noteIndexZone(node *rExpr) error {
	if !p.indexContext {
		return nil
	}
	if node.kind == reConstNull {
		return nil
	}
	if node.kind == reConstText {
		zr, ok := ResolveZone(node.cText)
		if !ok {
			return newError(InvalidParameterValue, "time zone not recognized: "+node.cText)
		}
		if !zr.Fixed {
			z := zr.zone
			p.timezoneDeps.merge(timeZoneDeps{Zones: []timeZoneDep{{Name: z.Name, Version: z.TzdataVersion, Checksum: z.Checksum}}})
		}
	} else {
		p.timezoneDeps.merge(timeZoneDeps{Dynamic: true, Zones: indexZoneDeps()})
	}
	return nil
}

func verifyIndexTimezoneDeps(deps timeZoneDeps) error {
	if deps.empty() {
		return nil
	}
	current := indexZoneDeps()
	matches := !deps.Dynamic || len(current) == len(deps.Zones)
	for _, d := range deps.Zones {
		at := sort.Search(len(current), func(i int) bool { return current[i].Name >= d.Name })
		if at == len(current) || current[at] != d {
			matches = false
			break
		}
	}
	if !matches {
		return newError(CollationVersionMismatch, "index timezone dependencies differ from the loaded data; rebuild the index")
	}
	return nil
}
