//! Resolved expression dependencies for persisted indexes (index-dependencies.md).
use super::*;

impl TimeZoneDeps {
    pub(crate) fn is_empty(&self) -> bool {
        !self.dynamic && self.zones.is_empty()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.zones.len() > u16::MAX as usize
            || self
                .zones
                .iter()
                .any(|d| d.name.len() > u16::MAX as usize || d.version.len() > u16::MAX as usize)
        {
            return Err(EngineError::new(
                SqlState::ProgramLimitExceeded,
                "too many timezone dependencies or an oversized dependency name/version",
            ));
        }
        if self
            .zones
            .iter()
            .any(|d| d.name.is_empty() || d.version.is_empty())
        {
            return Err(EngineError::new(
                SqlState::InvalidObjectDefinition,
                "index timezone dependencies require a nonempty name and version",
            ));
        }
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.dynamic |= other.dynamic;
        self.zones.extend(other.zones.iter().cloned());
        self.zones.sort();
        self.zones.dedup();
    }
}

impl ParamTypes {
    pub(crate) fn for_index() -> Self {
        Self {
            index_context: true,
            ..Self::default()
        }
    }

    /// Called at the resolved operation's birth; every enclosing expression shares this state.
    pub(crate) fn note_index_zone(&mut self, zone: &RExpr) -> Result<()> {
        if !self.index_context {
            return Ok(());
        }
        match zone {
            RExpr::ConstNull => (),
            RExpr::ConstText(name) => match crate::timezone::resolve_zone(name) {
                Some(crate::timezone::ZoneRef::Fixed(_)) => (),
                Some(crate::timezone::ZoneRef::Zone(z)) => {
                    self.timezone_deps.merge(&TimeZoneDeps {
                        dynamic: false,
                        zones: vec![TimeZoneDep {
                            name: name.clone(),
                            version: z.tzdata_version.clone(),
                            checksum: z.checksum,
                        }],
                    })
                }
                None => {
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        format!("time zone not recognized: {name}"),
                    ));
                }
            },
            _ => self.timezone_deps.merge(&TimeZoneDeps {
                dynamic: true,
                zones: crate::timezone::index_zone_deps(),
            }),
        }
        Ok(())
    }
}

pub(crate) fn verify_index_timezone_deps(deps: &TimeZoneDeps) -> Result<()> {
    if deps.is_empty() {
        return Ok(());
    }
    let current = crate::timezone::index_zone_deps();
    let matches = if deps.dynamic {
        current == deps.zones
    } else {
        deps.zones.iter().all(|d| current.binary_search(d).is_ok())
    };
    if matches {
        Ok(())
    } else {
        Err(EngineError::new(
            SqlState::CollationVersionMismatch,
            "index timezone dependencies differ from the loaded data; rebuild the index",
        ))
    }
}
