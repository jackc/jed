//! File attachments under the page service (spec/design/page-service.md §5.5; spec/pageservice/
//! README.md §6.6). An attachment opened under [`Locking::Auto`] has its own coordination domain: an
//! inner handle elected on the attached file exactly as an open is — the owner, serving it, or a page
//! client of whichever process owns it — with its own role, handoff, reader registry, and writer gate.
//! The outer handle's sessions use it as a database of the handle:
//!
//!   - a read begin pins every such attachment's newest snapshot through its inner handle (an owner
//!     adopts its clients' commits; a client sends begin_read) and releases the pins when the reader
//!     ends, after main's pin;
//!   - a write begin takes, after the handle's in-process writer gate, the writer gate of main (when
//!     main is under the page service) and of every read-write such attachment, in real-path byte
//!     order, each wait bounded by the session's lock_timeout_ms, releasing the gates already held in
//!     reverse order on failure; read-only such attachments are pinned for the writer instead;
//!   - an attachment's commit runs through its inner handle under the gate the writer holds: an
//!     owner's validated-COW commit, or a client's CommitDelta through its owner.
//!
//! The one-durable-writer rule is unchanged (attached-databases.md §5).

use super::*;

use std::path::PathBuf;

/// One reader pin on a service attachment's inner reader registry, with the snapshot it pins.
#[derive(Clone)]
pub(crate) struct AttachPin {
    name: String,
    core: Arc<Shared>,
    generation: u64,
    version: u64,
    snap: Arc<Snapshot>,
}

/// Reader pins on service attachments, in the order they were taken; dropping releases them in that
/// order.
#[derive(Default)]
pub(crate) struct AttachPins(Vec<AttachPin>);

impl Drop for AttachPins {
    fn drop(&mut self) {
        for pin in self.0.drain(..) {
            pin.core
                .live
                .lock()
                .expect("live lock not poisoned")
                .deregister_in(pin.generation, pin.version);
        }
    }
}

impl AttachPins {
    /// Overlay each pin's snapshot as its attachment's root.
    fn overlay(&self, attached: &mut HashMap<String, Arc<Snapshot>>) {
        for pin in &self.0 {
            attached.insert(pin.name.clone(), Arc::clone(&pin.snap));
        }
    }

    /// One more pin on each pinned version (a cursor outliving its session's pin), skipping a pin
    /// whose inner role has changed: that role's registry is gone.
    fn add(&self) -> AttachPins {
        let mut out = AttachPins::default();
        for pin in &self.0 {
            let mut live = pin.core.live.lock().expect("live lock not poisoned");
            if live.generation == pin.generation
                && live.counts.get(&pin.version).copied().unwrap_or(0) > 0
            {
                live.register(pin.version);
                out.0.push(pin.clone());
            }
        }
        out
    }

    /// `08006` when a pinned attachment's inner handle changed role since the pin (README.md §6.5).
    pub(crate) fn stale(&self) -> Result<()> {
        #[cfg(unix)]
        for pin in &self.0 {
            let stale = match pin.core.current_role() {
                None => true,
                Some(role) => {
                    role.generation != pin.generation
                        || role.client.as_ref().is_some_and(|conn| conn.is_broken())
                }
            };
            if stale {
                return Err(handoff::stale_role());
            }
        }
        Ok(())
    }
}

/// One writer gate a writer holds: main's (`att` is `None`) or a service attachment's (its name and
/// inner handle).
pub(crate) struct GateRef {
    path: PathBuf,
    att: Option<(String, Arc<Shared>)>,
}

/// A file attachment under the page service, as the gate and pin loops see it.
struct ServiceAttachment {
    name: String,
    inner: Arc<Shared>,
    mode: AttachMode,
    real_path: PathBuf,
}

/// The byte-order key of a resolved real path (page-service.md §5.5): every process orders the same
/// paths the same way. Not `Path`'s component order, which differs from byte order.
fn path_key(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

impl Shared {
    /// The attachments under the page service in real-path byte order.
    fn service_attachments(&self) -> Vec<ServiceAttachment> {
        if self.service_attach_count.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        let mut out: Vec<ServiceAttachment> = self
            .attachments
            .lock()
            .expect("attachments lock not poisoned")
            .values()
            .filter_map(|att| {
                att.svc.as_ref().map(|inner| ServiceAttachment {
                    name: att.name.clone(),
                    inner: Arc::clone(inner),
                    mode: att.mode,
                    real_path: att.real_path.clone(),
                })
            })
            .collect();
        out.sort_by(|a, b| path_key(&a.real_path).cmp(path_key(&b.real_path)));
        out
    }

    /// The inner handles of the service attachments, in real-path byte order.
    pub(crate) fn service_inners(&self) -> Vec<Arc<Shared>> {
        self.service_attachments()
            .into_iter()
            .map(|att| att.inner)
            .collect()
    }

    /// The committed root of attachment `name` as the handle knows it now: a service attachment's
    /// inner handle's committed snapshot, any other attachment's published root.
    pub(crate) fn attached_root(&self, name: &str) -> Option<Arc<Snapshot>> {
        let service = self
            .attachments
            .lock()
            .expect("attachments lock not poisoned")
            .get(name)
            .and_then(|att| att.svc.clone());
        match service {
            Some(inner) => Some(inner.pin()),
            None => self.committed_attachment(name),
        }
    }

    /// Pin every service attachment for a local reader, overlaying the pinned snapshots onto
    /// `attached`. `refresh` runs each inner handle's read begin (an owner adopts its clients'
    /// commits; a client sends begin_read); without it each pins what it already adopted (a read
    /// session's mint, refreshed at its first read).
    fn pin_service_attachments(
        &self,
        attached: &mut HashMap<String, Arc<Snapshot>>,
        refresh: bool,
    ) -> Result<AttachPins> {
        let mut pins = AttachPins::default();
        for att in self.service_attachments() {
            let (snap, _, version, generation) = if refresh {
                // A failure drops `pins`, releasing the pins already taken.
                att.inner.refresh_and_pin()?
            } else {
                att.inner.pin_latest()
            };
            pins.0.push(AttachPin {
                name: att.name,
                core: att.inner,
                generation,
                version,
                snap,
            });
        }
        pins.overlay(attached);
        Ok(pins)
    }

    /// Pin main's committed version ([`pin_latest`](Self::pin_latest)) and every service
    /// attachment's adopted one.
    pub(crate) fn pin_latest_all(&self) -> (PinnedRoots, AttachPins) {
        let (snap, mut attached, version, generation) = self.pin_latest();
        let pins = self
            .pin_service_attachments(&mut attached, false)
            .expect("pinning without a refresh never fails");
        ((snap, attached, version, generation), pins)
    }

    /// A read begin over main and every service attachment ([`refresh_and_pin`](Self::refresh_and_pin)
    /// for each, main first).
    pub(crate) fn refresh_and_pin_all(&self) -> Result<(PinnedRoots, AttachPins)> {
        let (snap, mut attached, version, generation) = self.refresh_and_pin()?;
        match self.pin_service_attachments(&mut attached, true) {
            Ok(pins) => Ok(((snap, attached, version, generation), pins)),
            Err(error) => {
                self.live
                    .lock()
                    .expect("live lock not poisoned")
                    .deregister_in(generation, version);
                Err(error)
            }
        }
    }

    /// The attachment pins a cursor takes beside its main pin when it does not adopt an autocommit
    /// read's: one more on each of the session's pins, and for a writer, one more on each read-only
    /// attachment's pin and one on each gated attachment's committed (its base).
    pub(crate) fn cursor_attach_pins(&self, session: &AttachPins, writer: bool) -> AttachPins {
        if !writer {
            return session.add();
        }
        let mut pins = self
            .writer_pins
            .lock()
            .expect("writer pins lock poisoned")
            .add();
        let held = self.held_gates.lock().expect("held gates lock poisoned");
        for gate in held.iter() {
            let Some((name, inner)) = &gate.att else {
                continue;
            };
            let (snap, _, version, generation) = inner.pin_latest();
            pins.0.push(AttachPin {
                name: name.clone(),
                core: Arc::clone(inner),
                generation,
                version,
                snap,
            });
        }
        pins
    }

    /// Take main's page-service writer gate.
    fn acquire_main_gate(&self, timeout_ms: u64) -> Result<()> {
        #[cfg(unix)]
        return self.begin_service_write(timeout_ms);
        #[cfg(not(unix))]
        {
            let _ = timeout_ms;
            unreachable!("the page service is Unix-only")
        }
    }

    /// Release main's page-service writer gate.
    fn release_main_gate(&self) {
        #[cfg(unix)]
        self.release_service_write();
    }

    /// Take, after the in-process writer gate the caller holds, the writer gate of main (when main is
    /// under the page service) and of every read-write service attachment, in real-path byte order
    /// (page-service.md §5.5). Each wait is bounded by `timeout_ms`; a failure releases the gates
    /// already held in reverse order. Read-only service attachments are then pinned for the writer,
    /// in real-path order.
    pub(crate) fn acquire_gates(&self, timeout_ms: u64) -> Result<()> {
        let mut gates: Vec<GateRef> = Vec::new();
        #[cfg(unix)]
        if let Some(handoff) = self.handoff.get() {
            gates.push(GateRef {
                path: handoff.path().to_path_buf(),
                att: None,
            });
        }
        let mut read_only = Vec::new();
        for att in self.service_attachments() {
            if att.mode == AttachMode::ReadOnly {
                read_only.push(att);
            } else {
                gates.push(GateRef {
                    path: att.real_path,
                    att: Some((att.name, att.inner)),
                });
            }
        }
        // Stable: main's gate stays first among equal paths.
        gates.sort_by(|a, b| path_key(&a.path).cmp(path_key(&b.path)));
        let mut held: Vec<GateRef> = Vec::with_capacity(gates.len());
        for gate in gates {
            let taken = match &gate.att {
                None => self.acquire_main_gate(timeout_ms),
                Some((_, inner)) => inner.acquire_writer(timeout_ms),
            };
            if let Err(error) = taken {
                self.release_gates(held);
                return Err(error);
            }
            held.push(gate);
        }
        let mut pins = AttachPins::default();
        for att in read_only {
            match att.inner.refresh_and_pin() {
                Ok((snap, _, version, generation)) => pins.0.push(AttachPin {
                    name: att.name,
                    core: att.inner,
                    generation,
                    version,
                    snap,
                }),
                Err(error) => {
                    drop(pins);
                    self.release_gates(held);
                    return Err(error);
                }
            }
        }
        *self.held_gates.lock().expect("held gates lock poisoned") = held;
        *self.writer_pins.lock().expect("writer pins lock poisoned") = pins;
        Ok(())
    }

    /// Release `held` gates in reverse order.
    pub(crate) fn release_gates(&self, held: Vec<GateRef>) {
        for gate in held.into_iter().rev() {
            match &gate.att {
                None => self.release_main_gate(),
                Some((_, inner)) => inner.release_writer(),
            }
        }
    }

    /// `attached` with the writer's service attachments overlaid: a gated attachment's committed
    /// snapshot (current: no other process can commit it) and a read-only one's pinned snapshot. The
    /// caller holds the writer gates.
    pub(crate) fn writer_view(
        &self,
        mut attached: HashMap<String, Arc<Snapshot>>,
    ) -> HashMap<String, Arc<Snapshot>> {
        for gate in self
            .held_gates
            .lock()
            .expect("held gates lock poisoned")
            .iter()
        {
            if let Some((name, inner)) = &gate.att {
                attached.insert(name.clone(), inner.pin());
            }
        }
        self.writer_pins
            .lock()
            .expect("writer pins lock poisoned")
            .overlay(&mut attached);
        attached
    }

    /// `08006` when an attachment the writer holds the gate or a pin of changed role since
    /// (README.md §6.5). The caller holds the writer gates.
    pub(crate) fn writer_stale(&self) -> Result<()> {
        #[cfg(unix)]
        for gate in self
            .held_gates
            .lock()
            .expect("held gates lock poisoned")
            .iter()
        {
            let Some((_, inner)) = &gate.att else {
                continue;
            };
            let writer_role = inner
                .writer_role
                .lock()
                .expect("writer role lock poisoned")
                .clone();
            let current = inner.current_role();
            let stale = match (writer_role, current) {
                (Some(writer_role), Some(current)) => {
                    !Arc::ptr_eq(&writer_role, &current)
                        || current.client.as_ref().is_some_and(|conn| conn.is_broken())
                }
                _ => true,
            };
            if stale {
                return Err(handoff::stale_role());
            }
        }
        self.writer_pins
            .lock()
            .expect("writer pins lock poisoned")
            .stale()
    }

    /// Commit a service attachment's working snapshot through this inner handle, under the gate the
    /// outer writer holds (page-service.md §5.5): the role decides the path — an owner's
    /// validated-COW commit, or a client's CommitDelta through its owner, with a lost acknowledgement
    /// resolved by re-election — then publish it here as a main commit is published. Returns the
    /// attachment's committed snapshot afterwards.
    pub(crate) fn commit_service_attachment(
        &self,
        snap: &mut Snapshot,
        stages_rows: bool,
    ) -> Result<Arc<Snapshot>> {
        self.persist(snap, stages_rows)?;
        snap.demote_clean_leaves(); // post-commit residency flip (bplus-reshape.md B4)
        let role_gen = self.writer_gen();
        let attached = self
            .roots
            .read()
            .expect("roots lock not poisoned")
            .attached
            .clone();
        // A lost acknowledgement resolved as committed under a new role publishes nothing: that
        // role's committed state already holds the commit (README.md §6.5).
        self.publish(Arc::new(snap.clone()), attached, role_gen);
        Ok(self.pin())
    }

    /// The budget precheck of this inner handle's attachment commit (memory.md §8.3), gated by its
    /// own reader registry, which counts other processes' pins when it owns the file.
    pub(crate) fn precheck_service_budget(
        &self,
        name: &str,
        snap: &Snapshot,
        prev: &Snapshot,
    ) -> Result<()> {
        let mut compactable = self.can_compact_committed();
        let oldest = self.oldest_live_version(prev.txid + 1);
        #[cfg(unix)]
        let client_grant = self
            .writer_role
            .lock()
            .expect("writer role lock poisoned")
            .as_ref()
            .and_then(|role| role.client.as_ref().map(|conn| conn.reuse_grant()));
        #[cfg(not(unix))]
        let client_grant: Option<bool> = None;
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        let mut reuse = oldest >= st.free_gen_txid;
        if let Some(grant) = client_grant {
            reuse = grant;
            compactable = false;
        }
        st.precheck_budget(
            snap,
            reuse,
            &BudgetCtx {
                name,
                prev,
                can_compact: compactable,
                stages_rows: snap.staged_bytes() != 0,
            },
        )
    }

    /// The handle's plan-cache epoch: its own, plus every service attachment's inner one, so an
    /// inner handle's adoption of another process's catalog invalidates this handle's cached plans.
    pub(crate) fn plan_epoch_now(&self) -> u64 {
        let mut epoch = self.plan_epoch.load(Ordering::Acquire);
        for att in self.service_attachments() {
            epoch = epoch.wrapping_add(att.inner.plan_epoch.load(Ordering::Acquire));
        }
        epoch
    }

    /// The database name this core's storage-limit errors name: main, or the attachment an inner
    /// handle serves.
    pub(crate) fn domain_name(&self) -> &str {
        self.name.get().map_or("main", String::as_str)
    }
}

impl Database {
    /// The page-service role of attachment `name`: `"owner"`, `"client"`, or `"none"` (not under
    /// the page service, between roles, or not attached). Test-only (spec/pageservice/README.md
    /// §6.5).
    #[doc(hidden)]
    pub fn page_service_attachment_role_for_testing(&self, name: &str) -> &'static str {
        let inner = self
            .0
            .attachments
            .lock()
            .expect("attachments lock not poisoned")
            .get(name)
            .and_then(|att| att.svc.clone());
        match inner {
            Some(inner) => Database(inner).page_service_role_for_testing(),
            None => "none",
        }
    }
}
