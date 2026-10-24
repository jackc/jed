// Page-service handoff, Node half (spec/design/page-service.md §8; spec/pageservice/README.md §6.5): a
// handle's page-service state that outlives its roles — its ownership preference, its limits, and the
// election a role change runs. A client whose connection ends re-elects through elect: it promotes
// itself to owner (presence EX, then the file opened as an owner opens it, with recovery, and served) or
// reconnects to whichever process owns the database. The role bookkeeping itself (installing storage,
// generations, drains) is the core's (shared.ts). Node-only.

import { closeSync, openSync } from "node:fs";

import { FileCoordinator, osLocks } from "../coordinator.ts";
import { type PageCodec, sealScratch } from "../crypto.ts";
import type { Environment } from "../environment.ts";
import type { Engine } from "../executor.ts";
import { DEFAULT_FLUSH_INTERVAL_MS } from "../idle_flush.ts";
import type { FileHost } from "../filehost.ts";
import type { LockHost } from "../lockhost.ts";
import type { ServiceHandoffHost, ServiceRole } from "../shared.ts";
import type { ClientOpenOptions } from "./client.ts";
import { electService } from "./election.ts";
import { DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS, type OwnerLimits, ServiceOwner } from "./owner.ts";

export type Ownership = "prefer" | "yield" | "never";

// The handoff limits' defaults (README §6.5).
export const DEFAULT_YIELD_GRACE_MS = 5000;
export const DEFAULT_SHUTDOWN_GRACE_MS = 30000;

// HandoffOptions are the page-service options of an open or create (file.ts OpenOptions /
// CreateOptions): a client keeps them for the day it becomes owner.
export type HandoffOptions = {
  ownership?: Ownership;
  yieldGraceMs?: number;
  shutdownGraceMs?: number;
  idleSessionTimeoutMs?: number;
  idleInTransactionTimeoutMs?: number;
  maxConnections?: number;
  synchronous?: "on" | "off";
  flushIntervalMs?: number;
  skipFsync?: boolean;
  scratchEncryption?: "auto" | "on";
};

// HandoffHosts are a handle's hosts under the page service: its lock host (the OS by default), its
// file host (null: the OS), a hook that gives an elected client's engine the handle's spill and file
// hosts, and whether a client re-elects in the background as soon as its connection ends. The
// deterministic simulator substitutes its own and turns background off, so every re-election runs at
// a transaction begin, in trace order.
export type HandoffHosts = {
  lock?: LockHost;
  files?: FileHost | null;
  adoptClient?: (engine: Engine) => void;
  background?: boolean;
};

export class ServiceHandoff implements ServiceHandoffHost {
  readonly path: string;
  readonly ownership: Ownership;
  readonly yieldGraceMs: number;
  readonly shutdownGraceMs: number;
  readonly skipFsync: boolean;
  readonly limits: OwnerLimits;
  // relaxed is the handle's synchronous = off, whatever role it holds: its file attachments follow it
  // (validated-cow.md §4).
  readonly relaxed: boolean;
  // flushIntervalMs is the handle's time-based flush interval (validated-cow.md §4.1).
  readonly flushIntervalMs: number;
  private readonly readOnly: boolean;
  private readonly timeoutMs: number | undefined;
  private readonly clientOpen: ClientOpenOptions;
  private readonly environment: Environment;
  // openOwner opens the file as an owner opening it does (recovery and the stabilization a writable
  // handle performs), with the handle's open options.
  // held is the data key a promoting page client holds (encrypted.md §14), null otherwise.
  private readonly openOwner: (held: PageCodec | null) => Engine;
  readonly host: LockHost;
  private readonly files: FileHost | null;
  private readonly adoptClient: ((engine: Engine) => void) | null;
  readonly background: boolean;

  constructor(
    path: string,
    options: HandoffOptions,
    readOnly: boolean,
    timeoutMs: number | undefined,
    clientOpen: ClientOpenOptions,
    environment: Environment,
    openOwner: (held: PageCodec | null) => Engine,
    hosts: HandoffHosts = {},
  ) {
    this.path = path;
    this.host = hosts.lock ?? osLocks;
    this.files = hosts.files ?? null;
    // A client engine adopted from the handle's hosts (the simulator's spill host in a simulation) keeps
    // the handle's scratch encryption: "on" seals its scratch files (encrypted.md §11).
    // An encrypted database's scratch files are always sealed (encrypted.md §11).
    const adopt = hosts.adoptClient ?? null;
    this.adoptClient =
      adopt === null
        ? null
        : (engine) => {
            adopt(engine);
            const encrypt =
              options.scratchEncryption === "on" ||
              (engine.paging !== null && engine.paging.codec() !== null);
            engine.spillSink = sealScratch(engine.spillSink, encrypt, environment);
          };
    this.background = hosts.background ?? true;
    this.ownership = options.ownership ?? "prefer";
    this.relaxed = options.synchronous === "off";
    this.flushIntervalMs = options.flushIntervalMs ?? DEFAULT_FLUSH_INTERVAL_MS;
    this.yieldGraceMs = options.yieldGraceMs ?? DEFAULT_YIELD_GRACE_MS;
    this.shutdownGraceMs = options.shutdownGraceMs ?? DEFAULT_SHUTDOWN_GRACE_MS;
    this.skipFsync = options.skipFsync ?? false;
    this.limits = {
      idleInTransactionTimeoutMs:
        options.idleInTransactionTimeoutMs ?? DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS,
      idleSessionTimeoutMs: options.idleSessionTimeoutMs ?? 0,
      maxConnections: options.maxConnections ?? 0,
      preferYield: this.ownership === "yield",
    };
    this.readOnly = readOnly;
    this.timeoutMs = timeoutMs;
    this.clientOpen = clientOpen;
    this.environment = environment;
    this.openOwner = openOwner;
  }

  // elect runs the election (an open's when first, else a re-election that keeps the path claim):
  // an elected client arrives opened; an elected owner holds presence EX.
  electAtOpen(): ReturnType<typeof electService> {
    const won = electService(this.host, this.path, this.timeoutMs, this.readOnly, {
      never: this.ownership === "never",
      pauseUntilMs: 0,
      reelect: false,
      open: this.clientOpen,
      environment: this.environment,
    });
    if (won.engine !== null) this.adoptClient?.(won.engine);
    return won;
  }

  // held is the data key the stale role holds: a promotion opens the file with it, and so does a
  // reconnection as a client, so a credential changed since does not stop it (encrypted.md §14).
  elect(pauseUntilMs: number, held: PageCodec | null = null): ServiceRole {
    const won = electService(this.host, this.path, this.timeoutMs, this.readOnly, {
      never: this.ownership === "never",
      pauseUntilMs,
      reelect: true,
      open: this.clientOpen,
      environment: this.environment,
      held,
    });
    if (won.client !== null) {
      this.adoptClient?.(won.engine);
      return {
        kind: "client",
        engine: won.engine,
        service: null,
        client: won.client,
        coordinator: null,
      };
    }
    // Promotion: presence EX is held; open the file as an owner and serve it.
    const coordinator = FileCoordinator.fromElection(this.host, this.path, won.owner);
    return this.startOwner(coordinator, undefined, held);
  }

  // startOwner makes the file the coordinator holds presence EX on a serving owner's role. A failure
  // releases presence (the path claim stays the handle's).
  startOwner(
    coordinator: FileCoordinator,
    opened?: Engine,
    held: PageCodec | null = null,
  ): ServiceRole {
    let engine: Engine;
    try {
      engine = opened ?? this.openOwner(held);
    } catch (error) {
      coordinator.close();
      throw error;
    }
    try {
      const service = ServiceOwner.start(engine, this.path, this.limits, this.host);
      return { kind: "owner", engine, service, client: null, coordinator };
    } catch (error) {
      engine.paging?.close();
      engine.paging = null;
      coordinator.close();
      throw error;
    }
  }

  canOpenReadWrite(): boolean {
    try {
      if (this.files !== null) this.files.openStore(this.path, true, false).close();
      else closeSync(openSync(this.path, "r+"));
      return true;
    } catch {
      return false;
    }
  }

  // hosts are the handoff's lock, file, and client hosts and its background flag, which a file
  // attachment's own handoff copies (page-service.md §5.5): the simulator's in a simulation.
  hosts(): HandoffHosts {
    return {
      lock: this.host,
      files: this.files,
      adoptClient: this.adoptClient ?? undefined,
      background: this.background,
    };
  }

  releasePath(): void {
    this.host.releasePath(this.path);
  }

  // nowMicros and pause are the lock host's clock and poll pause, which a drain reads.
  nowMicros(): number {
    return this.host.nowMicros();
  }

  pause(): void {
    this.host.pause();
  }
}
