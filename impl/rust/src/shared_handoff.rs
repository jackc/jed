//! Page-service roles and handoff (spec/design/page-service.md §8; spec/pageservice/README.md §6.5):
//! how a handle's role — owner or page client — changes between transactions. A client whose
//! connection ends re-elects: it promotes itself to owner (presence EX) or reconnects to whichever
//! process owns the database. An owner yields to a preferred requester, or drains and releases
//! ownership when it closes.
//!
//! A role is an immutable [`ServiceRole`] published in `Shared::role`. A role change installs fresh
//! main-domain storage while holding the reader registry and the storage lock, together with the new
//! role, so a commit — which checks the role it began under while holding the storage lock — either
//! finishes under the old role or fails without writing. Pins are tagged with the role generation
//! (`LiveRegistry::generation`): a role change starts an empty registry, and a pin of an older role
//! deregisters as a no-op. A role change never waits for the old role's transactions: they fail
//! `08006` at their next statement and at `COMMIT`.

use super::*;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::coordinator::{LockHost, OsLockHost};
use crate::file::Ownership;
use crate::filehost::{FileHost, OsFileHost};
use crate::pageservice::client::{ClientLink, ClientPins, HANDOFF_PAUSE, OnBreak, PsConnection};
use crate::pageservice::codec::{COMMIT_STATUS_COMMITTED, COMMIT_STATUS_NOT_COMMITTED};
use crate::pageservice::election::{ElectOptions, Election, elect};
use crate::pageservice::owner::{
    OwnerLimits, PageServiceOwner, ServiceWindow, committed_meta, new_incarnation, previous_meta,
};
use crate::paging::SharedPaging;
use crate::spillhost::SpillHost;

/// One role of a handle under the page service. Exactly one of `owner` and `client` is set, or
/// neither while the handle waits to re-elect after releasing ownership (or after it closed).
pub(crate) struct ServiceRole {
    pub(crate) generation: u64,
    pub(crate) owner: Option<Arc<PageServiceOwner>>,
    pub(crate) client: Option<Arc<PsConnection>>,
    /// The main domain's paging installed with this role.
    pub(crate) paging: Option<Arc<SharedPaging>>,
    /// Skip presence tries until then (the handoff pause of a role with no connection).
    pub(crate) pause_until: Option<Instant>,
}

impl ServiceRole {
    fn between(generation: u64, pause_until: Option<Instant>) -> Arc<ServiceRole> {
        Arc::new(ServiceRole {
            generation,
            owner: None,
            client: None,
            paging: None,
            pause_until,
        })
    }
}

/// The hosts a page-service handle runs over: the lock host (bundle locks, the owner record, the
/// endpoint and its connections, the clock), the file host (`None`: the OS) and spill host (`None`:
/// the OS scratch directory), and whether a lost connection starts a re-election at once. The
/// simulator sets its own hosts and turns `background` off, so every re-election runs at a
/// transaction begin, in trace order.
pub(crate) struct ServiceHosts {
    pub(crate) host: Arc<dyn LockHost>,
    pub(crate) files: Option<Arc<dyn FileHost>>,
    pub(crate) spill: Option<Arc<dyn SpillHost>>,
    pub(crate) background: bool,
}

impl ServiceHosts {
    /// The production hosts.
    pub(crate) fn os() -> ServiceHosts {
        ServiceHosts {
            host: Arc::new(OsLockHost),
            files: None,
            spill: None,
            background: true,
        }
    }
}

/// A handle's page-service state that outlives its roles.
pub(crate) struct ServiceHandoff {
    host: Arc<dyn LockHost>,
    files: Option<Arc<dyn FileHost>>,
    background: bool,
    path: PathBuf,
    /// The handle's open options: promotion opens the file with them, and an owner reads its limits
    /// and ownership preference from them.
    opts: OpenOptions,
    limits: OwnerLimits,
    /// Serializes role changes: re-election, promotion, yield, close.
    mu: Mutex<()>,
    closed: AtomicBool,
    shared: Weak<Shared>,
}

pub(crate) fn stale_role() -> EngineError {
    EngineError::new(
        SqlState::ConnectionFailure,
        "the transaction began before this process's connection to the database owner changed",
    )
}

fn no_owner() -> EngineError {
    EngineError::new(
        SqlState::ConnectionFailure,
        "lost the connection to the database owner, and no process took ownership in time",
    )
}

fn handle_closed() -> EngineError {
    EngineError::new(SqlState::ConnectionFailure, "the database handle is closed")
}

fn resolution_unknown(message: &str) -> EngineError {
    EngineError::new(SqlState::TransactionResolutionUnknown, message)
}

/// Whether this user can open `path` read-write (README.md §6.5 case 2): opened and closed unused,
/// so the read-only owner never holds write access.
fn can_open_read_write(files: &dyn FileHost, path: &Path) -> bool {
    files.open_store(path, true, false).is_ok()
}

impl ServiceHandoff {
    /// The file host promotion and the owner's read handle open the file through.
    fn file_host(&self) -> Arc<dyn FileHost> {
        match &self.files {
            Some(files) => Arc::clone(files),
            None => Arc::new(OsFileHost),
        }
    }
}

impl Shared {
    /// Attach the page-service state to a freshly built core.
    fn attach_handoff(self: &Arc<Self>, hosts: ServiceHosts, path: &Path, mut opts: OpenOptions) {
        opts.environment = self.env.clone();
        {
            // The handle's storage uses the handoff's file and spill hosts (the simulator's in a
            // simulation; the OS defaults otherwise), whatever role it holds.
            let mut storage = self.storage.lock().expect("storage lock not poisoned");
            if let Some(files) = &hosts.files {
                storage.files = Some(Arc::clone(files));
            }
            if let Some(spill) = &hosts.spill {
                storage.spill = Some(Arc::clone(spill));
            }
        }
        let handoff = ServiceHandoff {
            host: hosts.host,
            files: hosts.files,
            background: hosts.background,
            path: path.to_path_buf(),
            limits: OwnerLimits::from_options(&opts),
            opts,
            mu: Mutex::new(()),
            closed: AtomicBool::new(false),
            shared: Arc::downgrade(self),
        };
        let _ = self.handoff.set(Arc::new(handoff));
    }

    /// The current role (`None` when not under the page service).
    pub(crate) fn current_role(&self) -> Option<Arc<ServiceRole>> {
        self.role.read().expect("role lock poisoned").clone()
    }

    /// Whether `role` is still the current role.
    pub(crate) fn role_is(&self, role: &Arc<ServiceRole>) -> bool {
        self.role
            .read()
            .expect("role lock poisoned")
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, role))
    }

    /// The current role's owner, if this handle is the serving owner.
    pub(crate) fn service_owner(&self) -> Option<Arc<PageServiceOwner>> {
        self.current_role().and_then(|role| role.owner.clone())
    }

    /// The current role's connection, if this handle is a page client.
    pub(crate) fn page_client(&self) -> Option<Arc<PsConnection>> {
        self.current_role().and_then(|role| role.client.clone())
    }

    /// Whether `owner` is the current role's owner.
    pub(crate) fn is_owner(&self, owner: &PageServiceOwner) -> bool {
        self.role
            .read()
            .expect("role lock poisoned")
            .as_ref()
            .and_then(|role| role.owner.as_deref())
            .is_some_and(|current| std::ptr::eq(current, owner))
    }

    /// Publish `next` with its storage and committed snapshot. The caller holds `handoff.mu`. The
    /// reader registry and the storage are held together, so no commit or pin straddles the change.
    /// `engine` is the new role's engine (`None` keeps the storage: a release revokes it after);
    /// `window` is a new owner's service window.
    fn install_role(
        &self,
        next: &Arc<ServiceRole>,
        engine: Option<Engine>,
        window: Option<ServiceWindow>,
    ) {
        let mut live = self.live.lock().expect("live lock not poisoned");
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        if let Some(engine) = engine {
            st.paging = engine
                .paging
                .clone()
                .expect("every engine carries a paging context (B3)");
            st.page_count = engine.page_count;
            st.free_pages = engine.free_pages.clone();
            st.live_at_compaction = engine.live_at_compaction;
            st.free_gen_txid = engine.free_gen_txid;
            st.live = engine.live;
            st.read_only = engine.read_only;
            st.path = engine.path.clone();
            st.service_client = next.client.is_some();
            st.record_commits = true;
            st.last_commit = None;
            st.dir_synced = false;
            self.roots
                .write()
                .expect("roots lock not poisoned")
                .committed = Arc::new(engine.committed);
        }
        live.counts.clear();
        live.generation = next.generation;
        live.service = window;
        live.client = next.client.as_ref().map(|conn| ClientLink {
            conn: Arc::clone(conn),
            pins: ClientPins::default(),
        });
        *self.role.write().expect("role lock poisoned") = Some(Arc::clone(next));
        drop(st);
        drop(live);
        self.plan_epoch.fetch_add(1, Ordering::Release);
    }

    /// Re-elect in the background as soon as `role`'s connection ends (README.md §6.5).
    fn watch_client(&self, role: &Arc<ServiceRole>) {
        let (Some(conn), Some(handoff)) = (&role.client, self.handoff.get()) else {
            return;
        };
        if !handoff.background {
            return;
        }
        let shared = handoff.shared.clone();
        let watched = Arc::downgrade(role);
        let trigger: OnBreak = Arc::new(move || {
            let (shared, watched) = (shared.clone(), watched.clone());
            std::thread::spawn(move || {
                if let (Some(shared), Some(role)) = (shared.upgrade(), watched.upgrade()) {
                    let _ = shared.reelect(&role);
                }
            });
        });
        if !conn.set_on_break(Arc::clone(&trigger)) {
            trigger();
        }
    }

    /// Ask a yielding owner for ownership when this handle prefers to own (README.md §6.5 case 1).
    /// The outcome arrives as the connection's end.
    fn request_ownership_if_preferred(&self, conn: &Arc<PsConnection>) {
        let prefers = self
            .handoff
            .get()
            .is_some_and(|handoff| handoff.opts.ownership == Ownership::Prefer);
        if prefers && conn.owner_yields() {
            let conn = Arc::clone(conn);
            std::thread::spawn(move || {
                let _ = conn.request_ownership();
            });
        }
    }

    /// Replace `stale`, a role whose connection ended (or that released ownership), by running the
    /// election loop under the open timeout. `Ok` once the role changed, here or by a concurrent
    /// re-election; the caller re-reads it. With no owner in time the role stays and this is
    /// `08006`.
    pub(crate) fn reelect(&self, stale: &Arc<ServiceRole>) -> Result<()> {
        let handoff = self
            .handoff
            .get()
            .expect("re-election runs under the page service");
        let _guard = handoff.mu.lock().expect("handoff lock poisoned");
        if handoff.closed.load(Ordering::Acquire) {
            return Err(handle_closed());
        }
        if !self.role_is(stale) {
            return Ok(());
        }
        if stale.client.as_ref().is_some_and(|conn| !conn.is_broken()) {
            return Ok(());
        }
        let pause_until = match &stale.client {
            Some(conn) => conn.handoff_pause_until(),
            None => stale.pause_until,
        };
        let won = elect(
            &handoff.host,
            &handoff.path,
            &ElectOptions {
                timeout_ms: handoff.opts.file_lock_timeout_ms,
                read_only: handoff.opts.read_only,
                never: handoff.opts.ownership == Ownership::Never,
                pause_until,
                reelect: true,
                opts: &handoff.opts,
            },
        )
        .map_err(|e| {
            if e.state == SqlState::ObjectInUse {
                no_owner()
            } else {
                e
            }
        })?;
        match won {
            Election::Owner(coordinator) => self.promote(stale, coordinator),
            Election::Client(conn, engine) => {
                let engine = *engine;
                let paging = engine.paging.clone();
                let next = Arc::new(ServiceRole {
                    generation: stale.generation + 1,
                    owner: None,
                    client: Some(Arc::clone(&conn)),
                    paging,
                    pause_until: None,
                });
                self.install_role(&next, Some(engine), None);
                if let Some(old) = &stale.client {
                    old.close();
                }
                self.watch_client(&next);
                self.request_ownership_if_preferred(&conn);
                Ok(())
            }
        }
    }

    /// Make this handle the owner after it took presence EX (README.md §6.5): open the file as an
    /// owner does (recovery and stabilization), install it, and serve. The caller holds
    /// `handoff.mu`.
    fn promote(&self, stale: &Arc<ServiceRole>, coordinator: FileCoordinator) -> Result<()> {
        let handoff = self
            .handoff
            .get()
            .expect("promotion runs under the page service");
        let files = handoff.file_host();
        let engine = Engine::open_via(
            files.as_ref(),
            &handoff.path,
            OpenOptions {
                locking: Locking::None,
                environment: self.env.clone(),
                ..handoff.opts.clone()
            },
        )?;
        let paging = engine
            .paging
            .clone()
            .expect("an opened engine carries a paging context (B3)");
        let txid = engine.committed.txid;
        let incarnation = new_incarnation()?;
        let mut window = ServiceWindow::new(incarnation, txid, committed_meta(&paging, txid)?);
        if let Some(previous) = previous_meta(&paging, txid) {
            window.seed_previous(txid - 1, &previous);
        }
        let owner = PageServiceOwner::new(
            handoff.shared.clone(),
            &handoff.path,
            incarnation,
            engine.page_size,
            handoff.opts.read_only,
            handoff.opts.ownership,
            handoff.limits,
            Some(coordinator),
            Arc::clone(&handoff.host),
            files,
        )?;
        let next = Arc::new(ServiceRole {
            generation: stale.generation + 1,
            owner: Some(Arc::clone(&owner)),
            client: None,
            paging: Some(paging),
            pause_until: None,
        });
        self.install_role(&next, Some(engine), Some(window));
        if let Some(old) = &stale.client {
            old.close();
        }
        // An owner that cannot bind still owns; its record says it does not serve.
        let _ = owner.serve();
        Ok(())
    }

    /// End owner role `role` (README.md §6.5): stop serving, wait for any client commit past its
    /// point of no return, install `next`, and make the old storage refuse every further access (an
    /// own commit already writing finishes first, under the storage lock). Then remove the endpoint
    /// and owner record and release presence. The caller holds `handoff.mu`.
    fn release_ownership(&self, role: &Arc<ServiceRole>, next: &Arc<ServiceRole>) {
        let owner = role.owner.as_ref().expect("an owner role");
        owner.shutdown();
        self.install_role(next, None, None);
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .paging
            .revoke(EngineError::new(
                SqlState::ConnectionFailure,
                "this process no longer owns the database",
            ));
        owner.remove_serving();
        owner.release_presence();
    }

    /// Finish an accepted yield after its drain (README.md §6.5): release ownership and re-elect as
    /// a client, paused so the requester takes ownership.
    pub(crate) fn yield_ownership(&self, owner: &PageServiceOwner) {
        let Some(handoff) = self.handoff.get() else {
            owner.end_drain();
            return;
        };
        let guard = handoff.mu.lock().expect("handoff lock poisoned");
        let role = self.current_role();
        let role = match role {
            Some(role)
                if !handoff.closed.load(Ordering::Acquire)
                    && role
                        .owner
                        .as_deref()
                        .is_some_and(|o| std::ptr::eq(o, owner)) =>
            {
                role
            }
            _ => {
                drop(guard);
                owner.end_drain();
                return;
            }
        };
        let next = ServiceRole::between(
            role.generation + 1,
            Some(handoff.host.now() + HANDOFF_PAUSE),
        );
        self.release_ownership(&role, &next);
        drop(guard);
        owner.end_drain();
        let _ = self.reelect(&next);
    }

    /// A writer's request to a read-only owner (README.md §6.5 case 2): ask for ownership and wait,
    /// under the session's lock timeout, for the connection to end, then re-elect.
    fn request_write_ownership(
        &self,
        role: &Arc<ServiceRole>,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let handoff = self.handoff.get().expect("a page-service write begin");
        if handoff.opts.ownership == Ownership::Never
            || !can_open_read_write(handoff.file_host().as_ref(), &handoff.path)
        {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "the database owner opened the file read-only",
            ));
        }
        let conn = role.client.as_ref().expect("a client role");
        let _ = conn.request_ownership();
        if !conn.wait_broken(deadline) {
            return Err(EngineError::new(
                SqlState::LockNotAvailable,
                "could not obtain the database writer lock: the read-only owner did not hand over ownership in time",
            ));
        }
        self.reelect(role)
    }

    /// A page-service read begin (`refresh_and_pin`): pin the current role's newest snapshot for a
    /// local reader, re-electing first when the connection ended and retrying a begin the lost
    /// connection never answered (README.md §6.5).
    pub(crate) fn begin_service_read(&self) -> Result<PinnedRoots> {
        loop {
            let role = self
                .current_role()
                .expect("a page-service handle always has a role");
            if let Some(owner) = &role.owner {
                owner.wait_admission();
                if !self.role_is(&role) {
                    continue;
                }
                if let Err(e) = self.adopt_foreign(owner) {
                    if !self.role_is(&role) {
                        continue;
                    }
                    return Err(e);
                }
                let mut live = self.live.lock().expect("live lock not poisoned");
                if !self.role_is(&role) {
                    continue;
                }
                let r = self.roots.read().expect("roots lock not poisoned");
                let version = r.committed.txid;
                live.register(version);
                return Ok((
                    r.committed.clone(),
                    r.attached.clone(),
                    version,
                    live.generation,
                ));
            }
            if let Some(conn) = &role.client {
                if conn.is_broken() {
                    self.reelect(&role)?;
                    continue;
                }
                match self.client_refresh(&role) {
                    Ok(mut live) => {
                        let r = self.roots.read().expect("roots lock not poisoned");
                        let version = r.committed.txid;
                        live.register(version);
                        return Ok((
                            r.committed.clone(),
                            r.attached.clone(),
                            version,
                            live.generation,
                        ));
                    }
                    // The begin was never answered: re-elect and send it again.
                    Err(e) if e.state == SqlState::ConnectionFailure && conn.is_broken() => {
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            self.reelect(&role)?;
        }
    }

    /// Take the writer gate of the current role (`acquire_writer`, under the in-process writer
    /// gate): the owner's gate, or the owner's through `begin_write`. A writer meeting a read-only
    /// owner asks it for ownership first (README.md §6.5).
    pub(crate) fn begin_service_write(&self, timeout_ms: u64) -> Result<()> {
        let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
        loop {
            let role = self
                .current_role()
                .expect("a page-service handle always has a role");
            if let Some(owner) = &role.owner {
                owner.wait_admission();
                if !self.role_is(&role) {
                    continue;
                }
                if let Err(e) = owner.gate.acquire(timeout_ms, &|| false) {
                    if e.state == SqlState::ConnectionFailure
                        && let Some(handoff) = self.handoff.get()
                    {
                        // The gate closed: the owner may be releasing ownership. A release holds
                        // the handoff lock until the next role is installed, so wait it out and
                        // begin again under that role.
                        drop(handoff.mu.lock().expect("handoff lock poisoned"));
                    }
                    if !self.role_is(&role) {
                        continue;
                    }
                    return Err(e);
                }
                if !self.role_is(&role) {
                    owner.gate.release();
                    continue;
                }
                if let Err(e) = self.adopt_foreign(owner) {
                    owner.gate.release();
                    if !self.role_is(&role) {
                        continue;
                    }
                    return Err(e);
                }
                *self.writer_role.lock().expect("writer role lock poisoned") = Some(role);
                return Ok(());
            }
            if let Some(conn) = &role.client {
                if conn.is_broken() {
                    self.reelect(&role)?;
                    continue;
                }
                if conn.owner_read_only() && !conn.read_only() {
                    self.request_write_ownership(&role, deadline)?;
                    continue;
                }
                match self.client_begin_write(&role, timeout_ms) {
                    Ok(()) => {
                        *self.writer_role.lock().expect("writer role lock poisoned") = Some(role);
                        return Ok(());
                    }
                    Err(e) if e.state == SqlState::ConnectionFailure && conn.is_broken() => {
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            self.reelect(&role)?;
        }
    }

    /// Release the writer gate taken under the writer's role.
    pub(crate) fn release_service_write(&self) {
        let role = self
            .writer_role
            .lock()
            .expect("writer role lock poisoned")
            .take();
        let Some(role) = role else {
            return;
        };
        if let Some(owner) = &role.owner {
            owner.gate.release();
        } else if let Some(conn) = &role.client {
            conn.end_write();
        }
    }

    /// A page client's read begin (README.md §3.2, §6.3): one begin_read round trip, then the cache
    /// invalidation and, when the owner's version moved (or the whole cache was invalidated), the
    /// adoption of its snapshot. Returns with the registry lock held and the reply's pin recorded,
    /// for the caller to register the reader that keeps it. Serialized by the connection's cache
    /// lock, so the cache always corresponds to exactly one adopted version.
    fn client_refresh(
        &self,
        role: &Arc<ServiceRole>,
    ) -> Result<std::sync::MutexGuard<'_, LiveRegistry>> {
        let conn = role.client.as_ref().expect("a client role");
        let mut cache = conn.cache.lock().expect("client cache lock poisoned");
        let begun = conn.begin_read(cache.incarnation, cache.adopted)?;
        let mut live = self.live.lock().expect("live lock not poisoned");
        if !self.role_is(role) {
            return Err(stale_role());
        }
        let txid = begun.txid;
        live.client
            .as_mut()
            .expect("a page client's registry links its connection")
            .take(txid);
        if let Err(error) = self.client_adopt(
            role,
            &mut cache,
            begun.incarnation,
            txid,
            begun.invalidate,
            &begun.indices,
            begun.meta,
        ) {
            // No local reader will keep the pin.
            if !live.counts.contains_key(&txid)
                && let Some(link) = live.client.as_mut()
            {
                link.release(txid);
            }
            return Err(error);
        }
        Ok(live)
    }

    /// Apply a `read_begun` / `write_begun` cache answer (README.md §3.2, §6.4): drop the listed
    /// pages (or the whole cache) and, when the owner's version is newer than the adopted one (or the
    /// cache was dropped wholesale), adopt its meta and reload the catalog/interior skeleton. The
    /// caller holds the connection's cache lock. A role change in between fails `08006` without
    /// touching the new role's storage.
    #[allow(clippy::too_many_arguments)]
    fn client_adopt(
        &self,
        role: &Arc<ServiceRole>,
        cache: &mut crate::pageservice::client::CacheId,
        incarnation: crate::pageservice::codec::Id16,
        txid: u64,
        invalidate: u8,
        indices: &[u32],
        meta: Vec<u8>,
    ) -> Result<()> {
        let conn = role.client.as_ref().expect("a client role");
        let paging = role.paging.as_ref().expect("a client role installs paging");
        let all = invalidate == crate::pageservice::codec::INVALIDATE_ALL
            || incarnation != cache.incarnation;
        if all {
            paging.invalidate_all();
        } else {
            for index in indices {
                paging.invalidate(*index);
            }
        }
        if !all && txid <= cache.adopted {
            return Ok(());
        }
        conn.set_meta(meta);
        paging.pager().refresh_allocated_pages()?;
        let engine = Engine::open_shared_paging(Arc::clone(paging), &self.env)?;
        {
            let mut storage = self.storage.lock().expect("storage lock not poisoned");
            if !self.role_is(role) {
                return Err(stale_role());
            }
            storage.page_count = engine.page_count;
            storage.free_pages = engine.free_pages.clone();
            storage.live_at_compaction = engine.live_at_compaction;
            storage.free_gen_txid = engine.free_gen_txid;
            storage.live = engine.live;
            self.roots
                .write()
                .expect("roots lock not poisoned")
                .committed = Arc::new(engine.committed);
        }
        cache.incarnation = incarnation;
        cache.adopted = txid;
        self.plan_epoch.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Take the owner's writer gate for this process's writer (README.md §3.5, §6.4) and adopt the
    /// snapshot it pins, so the write transaction starts from the owner's newest commit. The caller
    /// holds the in-process writer gate. The wait for the owner's gate holds neither the cache nor
    /// the send lock, so this process's readers keep reading.
    fn client_begin_write(&self, role: &Arc<ServiceRole>, timeout_ms: u64) -> Result<()> {
        let conn = role.client.as_ref().expect("a client role");
        if conn.read_only() {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "cannot write through a read-only page-service connection",
            ));
        }
        let (incarnation, last_txid) = {
            let cache = conn.cache.lock().expect("client cache lock poisoned");
            (cache.incarnation, cache.adopted)
        };
        let begun = conn.begin_write(incarnation, last_txid, timeout_ms)?;
        conn.start_write(begun.txid, begun.reuse_grant);
        let adopted = {
            let mut cache = conn.cache.lock().expect("client cache lock poisoned");
            self.client_adopt(
                role,
                &mut cache,
                begun.incarnation,
                begun.txid,
                begun.invalidate,
                &begun.indices,
                begun.meta,
            )
        };
        if adopted.is_err() {
            conn.end_write();
        }
        adopted
    }

    /// Record this process's own published commit as its adopted version: the owner published
    /// exactly it, with no commit between the write base and it, so the cache is current
    /// (README.md §6.4).
    pub(crate) fn client_adopt_own_commit(&self, conn: &PsConnection, txid: u64, meta: Vec<u8>) {
        let mut cache = conn.cache.lock().expect("client cache lock poisoned");
        if txid <= cache.adopted {
            return;
        }
        conn.set_meta(meta);
        cache.adopted = txid;
    }

    /// A page client's commit (page-service.md §5.6, §6): the ordinary validated-COW recipe over
    /// the service store, never reclaiming (free-list rebuilds are owner-only), reusing the free list
    /// only under the owner's reuse grant, and never compacting for its storage budget. The store's
    /// sync sends the CommitDelta. A failure after which the owner certainly did not commit leaves the
    /// pager usable; a lost acknowledgement is resolved by re-election (README.md §6.5); any other
    /// failure keeps the pager poisoned.
    pub(crate) fn persist_client(
        &self,
        role: &Arc<ServiceRole>,
        snap: &Snapshot,
        stages_rows: bool,
    ) -> Result<()> {
        let conn = role.client.as_ref().expect("a client role");
        let prev = Arc::clone(
            &self
                .roots
                .read()
                .expect("roots lock not poisoned")
                .committed,
        );
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        if !self.role_is(role) {
            return Err(stale_role());
        }
        let budget = BudgetCtx {
            name: "main",
            prev: &prev,
            can_compact: false,
            stages_rows,
        };
        let Err(error) = st.commit_durable(snap, false, conn.reuse_grant(), Some(&budget)) else {
            return Ok(());
        };
        if let Some((txid, digest)) = conn.take_lost_commit() {
            st.paging.pager().discard_pending();
            drop(st);
            return self.resolve_lost_commit(role, txid, digest, error);
        }
        {
            let mut pager = st.paging.pager();
            pager.discard_pending();
            if conn.take_uncommitted() {
                pager.recover_uncommitted();
            }
        }
        st.paging.invalidate_all();
        Err(error)
    }

    /// Settle a client commit whose acknowledgement was lost (README.md §6.4–§6.5): the re-election
    /// either promotes this process, which answers from its own resolution window, or reconnects it
    /// to the new owner, which it asks; with no owner in time the outcome is unknown (`08007`).
    fn resolve_lost_commit(
        &self,
        stale: &Arc<ServiceRole>,
        txid: u64,
        digest: u64,
        lost: EngineError,
    ) -> Result<()> {
        let unknown = || {
            resolution_unknown(
                "lost the connection to the database owner during commit; whether the transaction committed is unknown",
            )
        };
        if self.reelect(stale).is_err() {
            return Err(resolution_unknown(
                "lost the connection to the database owner during commit, and no owner answered whether it committed",
            ));
        }
        let Some(role) = self.current_role() else {
            return Err(unknown());
        };
        let status = if let Some(owner) = &role.owner {
            // A commit past its point of no return holds the commit lock until published.
            drop(owner.commit_lock.lock().expect("commit lock poisoned"));
            self.page_service_resolve(txid, digest)
        } else if let Some(conn) = &role.client {
            conn.commit_status(txid, digest).map_err(|_| unknown())?
        } else {
            return Err(unknown());
        };
        match status {
            COMMIT_STATUS_COMMITTED => Ok(()),
            COMMIT_STATUS_NOT_COMMITTED => Err(lost),
            _ => Err(unknown()),
        }
    }

    /// End the handle's page-service role at close (`graceful`: `Database::close`) or drop: a
    /// serving owner drains its clients for the shutdown grace (graceful only) and releases
    /// ownership; a client closes its connection. Then the process-local path claim is released.
    /// Idempotent.
    pub(crate) fn end_service(&self, graceful: bool) {
        let Some(handoff) = self.handoff.get() else {
            return;
        };
        if handoff.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let guard = handoff.mu.lock().expect("handoff lock poisoned");
        if let Some(role) = self.current_role() {
            if let Some(owner) = &role.owner {
                if graceful {
                    owner.close_gracefully(self);
                } else {
                    owner.end_drain();
                }
                self.release_ownership(&role, &ServiceRole::between(role.generation + 1, None));
            } else if let Some(conn) = &role.client {
                conn.close();
            }
        }
        drop(guard);
        handoff.host.release_path(&handoff.path);
    }
}

/// Make a freshly created or opened `db` the page-service owner of the database at `path`
/// (spec/pageservice/README.md §6.1–§6.2): draw the incarnation, record the committed meta page,
/// install the role, then write the owner record, bind the endpoint, and start accepting.
/// `coordinator` holds presence EX; the path is claimed for the handle.
pub(super) fn start_service_owner(
    db: Database,
    path: &Path,
    coordinator: FileCoordinator,
    opts: OpenOptions,
    hosts: ServiceHosts,
) -> Result<Database> {
    let shared = &db.0;
    shared.attach_handoff(hosts, path, opts);
    let handoff = Arc::clone(shared.handoff.get().expect("attached above"));
    let (paging, page_size) = {
        let mut storage = shared.storage.lock().expect("storage lock not poisoned");
        storage.record_commits = true;
        (storage.paging.clone(), storage.page_size)
    };
    let txid = shared.committed_version();
    let incarnation = new_incarnation()?;
    let mut window = ServiceWindow::new(incarnation, txid, committed_meta(&paging, txid)?);
    // The resolution window also starts with the commit before the recovered one (README.md §4).
    if let Some(previous) = previous_meta(&paging, txid) {
        window.seed_previous(txid - 1, &previous);
    }
    let owner = PageServiceOwner::new(
        Arc::downgrade(shared),
        path,
        incarnation,
        page_size,
        handoff.opts.read_only,
        handoff.opts.ownership,
        handoff.limits,
        Some(coordinator),
        Arc::clone(&handoff.host),
        handoff.file_host(),
    )?;
    let role = Arc::new(ServiceRole {
        generation: 1,
        owner: Some(Arc::clone(&owner)),
        client: None,
        paging: Some(paging),
        pause_until: None,
    });
    shared.install_role(&role, None, Some(window));
    owner.serve()?;
    Ok(db)
}

/// Open `path` under the page service (`Locking::Auto`, spec/pageservice/README.md §6.1): as the
/// serving owner, or as a page client of the owner.
pub(super) fn open_service(path: &Path, opts: OpenOptions) -> Result<Database> {
    let real = crate::coordinator::resolve_open_path(path)?;
    open_service_with(ServiceHosts::os(), &real, opts, true)
}

/// Run the election for the database at `real` (already resolved) over `hosts` and open the handle
/// in the role it wins. `claim` is false when the caller already holds the process-local path claim.
pub(crate) fn open_service_with(
    hosts: ServiceHosts,
    real: &Path,
    opts: OpenOptions,
    claim: bool,
) -> Result<Database> {
    let real = real.to_path_buf();
    let host = Arc::clone(&hosts.host);
    let won = elect(
        &host,
        &real,
        &ElectOptions {
            timeout_ms: opts.file_lock_timeout_ms,
            read_only: opts.read_only,
            never: opts.ownership == Ownership::Never,
            pause_until: None,
            reelect: !claim,
            opts: &opts,
        },
    )?;
    match won {
        Election::Owner(coordinator) => {
            let files: Arc<dyn FileHost> = match &hosts.files {
                Some(files) => Arc::clone(files),
                None => Arc::new(OsFileHost),
            };
            let engine = match Engine::open_via(
                files.as_ref(),
                &real,
                OpenOptions {
                    // Coordination is the election's presence EX.
                    locking: Locking::None,
                    ..opts.clone()
                },
            ) {
                Ok(engine) => engine,
                Err(e) => {
                    drop(coordinator);
                    if claim {
                        host.release_path(&real);
                    }
                    return Err(e);
                }
            };
            let db = Database::from_engine_coordinated(engine, None);
            db.0.storage
                .lock()
                .expect("storage lock not poisoned")
                .budget
                .limit = opts.max_storage_bytes;
            start_service_owner(db, &real, coordinator, opts, hosts)
        }
        Election::Client(conn, engine) => {
            let paging = engine.paging.clone();
            let db = Database::from_engine_coordinated(*engine, None);
            {
                let mut storage = db.0.storage.lock().expect("storage lock not poisoned");
                storage.service_client = true;
                // The owner publishes this process's commits; the client adopts its own from the meta.
                storage.record_commits = true;
                storage.budget.limit = opts.max_storage_bytes;
            }
            db.0.attach_handoff(hosts, &real, opts);
            let role = Arc::new(ServiceRole {
                generation: 1,
                owner: None,
                client: Some(Arc::clone(&conn)),
                paging,
                pause_until: None,
            });
            db.0.install_role(&role, None, None);
            db.0.watch_client(&role);
            db.0.request_ownership_if_preferred(&conn);
            Ok(db)
        }
    }
}
