//! The environment (spec/design/environment.md): one explicit home for every input the engine takes
//! from outside itself except storage — the loaded Unicode data (collations + the casing table), the
//! loaded time-zone data, the host functions, and the random and clock sources.
//!
//! An [`EnvironmentBuilder`] does all the loading, registering, and source choosing; [`build`]
//! (EnvironmentBuilder::build) returns an immutable [`Environment`] with no mutating methods at all,
//! so a database's outside inputs never change while it is open. Each database handle carries exactly
//! one environment, given at `create`/`open` (a fresh empty one when none is given — there is no
//! process-wide default); its attachments, temp domains, and every session minted from it use the same
//! one. A host that wants one set for everything builds one environment and passes it to every
//! `open`/`create`; a library or a test builds its own and stays isolated.
//!
//! The environment fixes WHICH random and clock source a database uses, not the source's state: a
//! source is a shared object (seam.rs) the host may keep a reference to and control (reseed a seeded
//! source, set a manual clock).
//!
//! Cheap to share and derive: an `Environment` is an `Arc`, and its tables are `Arc`s whose entries
//! are themselves `Arc`s, so [`to_builder`](Environment::to_builder) starts a builder that shares
//! everything by reference and copies a table only when that builder changes it.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::collation::{Collation, PropertyTable};
use crate::error::Result;
use crate::executor::{CollationInfo, CollationVerdict};
use crate::extension::{ExtensionRegistry, HostFunction};
use crate::seam::{ClockSource, RandomSource};
use crate::timezone::{TimeZoneInfo, Zone, ZoneRef};

/// The engine's outside inputs, frozen (spec/design/environment.md §3). Cloning shares it.
#[derive(Clone)]
pub struct Environment(Arc<Contents>);

/// Builds an [`Environment`]. Every load and registration is validated as it is made (a malformed
/// bundle is `XX001`, a conflicting host function `42723`); [`build`](Self::build) cannot fail.
#[derive(Clone)]
pub struct EnvironmentBuilder {
    contents: Contents,
}

/// What an environment holds. Each table is an `Arc` so a derived builder shares it until it changes it.
#[derive(Clone)]
struct Contents {
    /// Loaded collations by exact (case-sensitive) name; first-wins. `C` is never here (built in).
    collations: Arc<BTreeMap<String, Arc<Collation>>>,
    /// The Unicode property/casing table (collation.md §16); first-wins. `None` ⇒ ASCII casing.
    property: Option<Arc<PropertyTable>>,
    /// Loaded named zones and link aliases by exact name; first-wins. `UTC` and fixed offsets are
    /// never here (built in, timezones.md §3.2).
    zones: Arc<BTreeMap<String, Arc<Zone>>>,
    /// The host scalar functions (extensibility.md §4.2).
    functions: Arc<ExtensionRegistry>,
    random: Arc<dyn RandomSource>,
    clock: Arc<dyn ClockSource>,
}

impl Default for Environment {
    /// The empty environment: the built-in collation `C` and ASCII casing, `UTC` and fixed offsets,
    /// no host functions, OS randomness, and the wall clock (environment.md §7).
    fn default() -> Self {
        Environment::builder().build()
    }
}

impl Default for EnvironmentBuilder {
    fn default() -> Self {
        EnvironmentBuilder {
            contents: Contents {
                collations: Arc::new(BTreeMap::new()),
                property: None,
                zones: Arc::new(BTreeMap::new()),
                functions: Arc::new(ExtensionRegistry::default()),
                random: crate::seam::os_random_source(),
                clock: crate::seam::wall_clock(),
            },
        }
    }
}

impl EnvironmentBuilder {
    /// Load a `JUCD` Unicode-data bundle (spec/design/collation.md §4/§9): merge the root + each
    /// per-locale delta (§5.1), add every collation by name, and keep the **property/casing** section
    /// (§16). **First-wins** — a collation name already present is not replaced, and a property table
    /// is kept only if none is loaded yet, so loading the same bundle twice changes nothing. A
    /// malformed bundle is `XX001` (`data_corrupted`). A privileged host act — the engine reads no
    /// file path and reaches no host data (§11); the host sources the bytes.
    pub fn load_unicode_data(mut self, bytes: &[u8]) -> Result<Self> {
        let bundle = crate::collation::open_bundle(bytes)?;
        let (colls, property) = crate::collation::load_bundle(&bundle)?;
        let set = Arc::make_mut(&mut self.contents.collations);
        for c in colls {
            set.entry(c.name.clone()).or_insert_with(|| Arc::new(c));
        }
        if self.contents.property.is_none() {
            self.contents.property = property.map(Arc::new);
        }
        Ok(self)
    }

    /// Load a `JTZ` time-zone bundle (spec/design/timezones.md §3.3): its named zones and link aliases
    /// become usable by `AT TIME ZONE` and the session time zone. **First-wins**, like the Unicode
    /// data. A malformed bundle is `XX001`. (`UTC` and fixed offsets are built in and need no load.)
    pub fn load_time_zone_data(mut self, bytes: &[u8]) -> Result<Self> {
        let zones = crate::timezone::parse_zone_bundle(bytes)?;
        let set = Arc::make_mut(&mut self.contents.zones);
        for z in zones {
            set.entry(z.name.clone()).or_insert(z);
        }
        Ok(self)
    }

    /// Register a host scalar function (spec/design/extensibility.md §4.2). Errors on a **negative
    /// cost** (`22023`) or a second function with an **identical `(name, arg_types)` signature**
    /// (`42723` — a host may overload a name across argument types). A signature that shadows a
    /// built-in is accepted but never reached (built-ins win).
    pub fn register_function(mut self, f: HostFunction) -> Result<Self> {
        Arc::make_mut(&mut self.contents.functions).register_function(f)?;
        Ok(self)
    }

    /// Use `source` for the uuid generators' randomness (spec/design/entropy.md §6). Replaces the
    /// current one; the default is [`os_random_source`](crate::os_random_source).
    pub fn random_source(mut self, source: Arc<dyn RandomSource>) -> Self {
        self.contents.random = source;
        self
    }

    /// Use `source` for the clock functions and `uuidv7` (spec/design/entropy.md §5/§6). Replaces the
    /// current one; the default is [`wall_clock`](crate::wall_clock).
    pub fn clock_source(mut self, source: Arc<dyn ClockSource>) -> Self {
        self.contents.clock = source;
        self
    }

    /// Freeze the builder into an [`Environment`].
    pub fn build(self) -> Environment {
        Environment(Arc::new(self.contents))
    }
}

impl Environment {
    /// A builder starting from the empty environment (see [`Default`]).
    pub fn builder() -> EnvironmentBuilder {
        EnvironmentBuilder::default()
    }

    /// A builder pre-filled with this environment's contents, shared by reference. Building it
    /// unchanged yields an environment with the same contents; the builder may add bundles and
    /// functions and replace the sources, never remove. This environment is unaffected.
    pub fn to_builder(&self) -> EnvironmentBuilder {
        EnvironmentBuilder {
            contents: (*self.0).clone(),
        }
    }

    /// Every loaded collation (spec/design/collation.md §4.2), ascending by name, each as `(name,
    /// unicode_version, cldr_version, content_hash, description, is_default)`. `is_default` is always
    /// `false` here (that is a per-database property) and the verdict always `Full` (the loaded set
    /// IS the version reference). `C` is built in and not listed.
    pub fn loaded_collations(&self) -> Vec<CollationInfo> {
        self.0
            .collations
            .values()
            .map(|c| CollationInfo {
                name: c.name.clone(),
                unicode_version: c.unicode_version.clone(),
                cldr_version: c.cldr_version.clone(),
                content_hash: crate::format::crc32_ieee(&crate::collation::serialize_table(c)),
                description: c.description.clone(),
                is_default: false,
                verdict: CollationVerdict::Full,
            })
            .collect()
    }

    /// Every loaded named zone and alias (spec/design/timezones.md §3.3), ascending by name, each as
    /// `(name, tzdata_version)`. `UTC` and fixed offsets are built in and not listed.
    pub fn loaded_time_zones(&self) -> Vec<TimeZoneInfo> {
        self.0
            .zones
            .values()
            .map(|z| TimeZoneInfo {
                name: z.name.clone(),
                tzdata_version: z.tzdata_version.clone(),
            })
            .collect()
    }

    /// Whether `a` and `b` are the same environment (not merely equal contents).
    pub(crate) fn ptr_eq(a: &Environment, b: &Environment) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    /// Look up a loaded collation by exact (case-sensitive) name. `None` ⇒ no loaded bundle provides
    /// it. `C` is never here (table-free, built in).
    pub(crate) fn collation(&self, name: &str) -> Option<Arc<Collation>> {
        self.0.collations.get(name).cloned()
    }

    /// The property/casing table, if a loaded bundle provided one (collation.md §16). `None` ⇒ the
    /// ASCII-casing baseline. The casing functions read it once per evaluation and pass it to the pure
    /// kernels.
    pub(crate) fn property(&self) -> Option<&PropertyTable> {
        self.0.property.as_deref()
    }

    /// The version-skew verdict (spec/design/collation.md §12, compatibility.md §7): given a collation
    /// `name` and the `(unicode, cldr)` version a FILE pinned its keys under (§5), `Some((loaded_unicode,
    /// loaded_cldr))` when this environment provides `name` at a **different** version — the object
    /// using it is read-only (`XX002` on write) — else `None` (`Full`: the same version, or no loaded
    /// table to disagree). A pure comparison, so every core computes the identical verdict.
    pub(crate) fn version_skew(
        &self,
        name: &str,
        file_unicode: &str,
        file_cldr: &str,
    ) -> Option<(String, String)> {
        let loaded = self.collation(name)?;
        if loaded.unicode_version != file_unicode || loaded.cldr_version != file_cldr {
            Some((loaded.unicode_version.clone(), loaded.cldr_version.clone()))
        } else {
            None
        }
    }

    /// Resolve a zone name to a built-in fixed offset (`UTC` / `±HH[:MM[:SS]]`) or a loaded named
    /// zone (timezones.md §3.2). `None` ⇒ unknown (the caller raises `22023`).
    pub(crate) fn resolve_zone(&self, name: &str) -> Option<ZoneRef> {
        crate::timezone::builtin_zone(name)
            .or_else(|| self.0.zones.get(name).cloned().map(ZoneRef::Zone))
    }

    /// The loaded zones as persisted index dependencies, ascending by exact UTF-8 name
    /// (index-dependencies.md).
    pub(crate) fn index_zone_deps(&self) -> Vec<crate::catalog::TimeZoneDep> {
        self.0
            .zones
            .values()
            .map(|z| crate::catalog::TimeZoneDep {
                name: z.name.clone(),
                version: z.tzdata_version.clone(),
                checksum: z.checksum,
            })
            .collect()
    }

    /// The host scalar functions.
    pub(crate) fn functions(&self) -> &ExtensionRegistry {
        &self.0.functions
    }

    /// Fill `buf` from the random source.
    pub(crate) fn fill(&self, buf: &mut [u8]) -> Result<()> {
        self.0.random.fill(buf)
    }

    /// Read the clock source, in micros since the Unix epoch.
    pub(crate) fn now_micros(&self) -> i64 {
        self.0.clock.now_micros()
    }
}

impl std::fmt::Debug for Environment {
    /// Names what is loaded (the sources and kernels are opaque), so the options structs that carry
    /// an environment can still `derive(Debug)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("collations", &self.0.collations.keys().collect::<Vec<_>>())
            .field("casing", &self.0.property.is_some())
            .field("zones", &self.0.zones.len())
            .field("functions", &*self.0.functions)
            .finish()
    }
}

impl std::fmt::Debug for EnvironmentBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentBuilder").finish_non_exhaustive()
    }
}
