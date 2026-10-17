// Page-service handoff, Node half (spec/design/page-service.md §8; spec/pageservice/README.md §6.5): a
// handle's page-service state that outlives its roles — its ownership preference, its limits, and the
// election a role change runs. A client whose connection ends re-elects through elect: it promotes
// itself to owner (presence EX, then the file opened as an owner opens it, with recovery, and served) or
// reconnects to whichever process owns the database. The role bookkeeping itself (installing storage,
// generations, drains) is the core's (shared.ts). Node-only.

import { closeSync, openSync } from "node:fs";

import { FileCoordinator, osLocks } from "../coordinator.ts";
import type { Environment } from "../environment.ts";
import type { Engine } from "../executor.ts";
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
};

export class ServiceHandoff implements ServiceHandoffHost {
  readonly path: string;
  readonly ownership: Ownership;
  readonly yieldGraceMs: number;
  readonly shutdownGraceMs: number;
  readonly limits: OwnerLimits;
  private readonly readOnly: boolean;
  private readonly timeoutMs: number | undefined;
  private readonly clientOpen: ClientOpenOptions;
  private readonly environment: Environment;
  // openOwner opens the file as an owner opening it does (recovery and the stabilization a writable
  // handle performs), with the handle's open options.
  private readonly openOwner: () => Engine;

  constructor(
    path: string,
    options: HandoffOptions,
    readOnly: boolean,
    timeoutMs: number | undefined,
    clientOpen: ClientOpenOptions,
    environment: Environment,
    openOwner: () => Engine,
  ) {
    this.path = path;
    this.ownership = options.ownership ?? "prefer";
    this.yieldGraceMs = options.yieldGraceMs ?? DEFAULT_YIELD_GRACE_MS;
    this.shutdownGraceMs = options.shutdownGraceMs ?? DEFAULT_SHUTDOWN_GRACE_MS;
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
    return electService(osLocks, this.path, this.timeoutMs, this.readOnly, {
      never: this.ownership === "never",
      pauseUntilMs: 0,
      reelect: false,
      open: this.clientOpen,
      environment: this.environment,
    });
  }

  elect(pauseUntilMs: number): ServiceRole {
    const won = electService(osLocks, this.path, this.timeoutMs, this.readOnly, {
      never: this.ownership === "never",
      pauseUntilMs,
      reelect: true,
      open: this.clientOpen,
      environment: this.environment,
    });
    if (won.client !== null) {
      return {
        kind: "client",
        engine: won.engine,
        service: null,
        client: won.client,
        coordinator: null,
      };
    }
    // Promotion: presence EX is held; open the file as an owner and serve it.
    const coordinator = FileCoordinator.fromElection(osLocks, this.path, won.owner);
    return this.startOwner(coordinator);
  }

  // startOwner makes the file the coordinator holds presence EX on a serving owner's role. A failure
  // releases presence (the path claim stays the handle's).
  startOwner(coordinator: FileCoordinator, opened?: Engine): ServiceRole {
    let engine: Engine;
    try {
      engine = opened ?? this.openOwner();
    } catch (error) {
      coordinator.close();
      throw error;
    }
    try {
      const service = ServiceOwner.start(engine, this.path, this.limits);
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
      closeSync(openSync(this.path, "r+"));
      return true;
    } catch {
      return false;
    }
  }

  releasePath(): void {
    osLocks.releasePath(this.path);
  }
}
