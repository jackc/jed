import type { TimeZoneDeps } from "./catalog.ts";
import type { RExpr } from "./executor.ts";
import { engineError } from "./errors.ts";
import { indexZoneDeps, resolveZone } from "./timezone.ts";
import { compareTextC } from "./value.ts";

export function validateTimezoneDeps(deps: TimeZoneDeps): void {
  const utf8 = new TextEncoder();
  if (
    deps.zones.length > 65535 ||
    deps.zones.some(
      (d) => utf8.encode(d.name).length > 65535 || utf8.encode(d.version).length > 65535,
    )
  ) {
    throw engineError(
      "program_limit_exceeded",
      "too many timezone dependencies or an oversized dependency name/version",
    );
  }
  if (deps.zones.some((d) => d.name === "" || d.version === "")) {
    throw engineError(
      "invalid_object_definition",
      "index timezone dependencies require a nonempty name and version",
    );
  }
}

// Resolved expression dependencies for persisted indexes (index-dependencies.md).
export function mergeTimezoneDeps(target: TimeZoneDeps, other: TimeZoneDeps): void {
  target.dynamic ||= other.dynamic;
  for (const next of other.zones) {
    if (
      !target.zones.some(
        (old) =>
          old.name === next.name && old.version === next.version && old.checksum === next.checksum,
      )
    ) {
      target.zones.push(next);
    }
  }
  target.zones.sort((a, b) => compareTextC(a.name, b.name));
}

export function noteIndexZone(deps: TimeZoneDeps, node: RExpr): void {
  if (node.kind === "constNull") return;
  if (node.kind === "constText") {
    const zr = resolveZone(node.value);
    if (!zr)
      throw engineError("invalid_parameter_value", `time zone not recognized: ${node.value}`);
    if (!zr.fixed)
      mergeTimezoneDeps(deps, {
        dynamic: false,
        zones: [
          {
            name: zr.zone.name,
            version: zr.zone.tzdataVersion,
            checksum: zr.zone.checksum,
          },
        ],
      });
  } else mergeTimezoneDeps(deps, { dynamic: true, zones: indexZoneDeps() });
}

export function verifyIndexTimezoneDeps(deps: TimeZoneDeps | undefined): void {
  if (!deps || (!deps.dynamic && deps.zones.length === 0)) return;
  const current = indexZoneDeps();
  if (
    (deps.dynamic && current.length !== deps.zones.length) ||
    !deps.zones.every((d) =>
      current.some(
        (c) => c.name === d.name && c.version === d.version && c.checksum === d.checksum,
      ),
    )
  ) {
    throw engineError(
      "collation_version_mismatch",
      "index timezone dependencies differ from the loaded data; rebuild the index",
    );
  }
}
