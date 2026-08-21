import type { BlitSession, BlitSurface } from "@blit-sh/core";

export const MUSTER_TERMINAL_PREFIX = "muster/";

interface MusterRunIdentity {
  readonly unit: string;
  readonly sequence: string;
}

interface MusterHierarchyIdentity {
  /** The instance prefix, or null for a plain top-level unit. */
  readonly instance: string | null;
  /** The unit name without its instance prefix. */
  readonly unit: string;
  /** The complete unit name used by the supervisor and surface stamp. */
  readonly qualifiedUnit: string;
  /** Sequence number for a run, or `stop`/`reload` for a control terminal. */
  readonly run: string;
}

export interface MusterPreviewRun {
  /** The terminal whose stamped socket owns the group's surfaces. */
  readonly session: BlitSession;
  /** Sequence number, or the control command name. */
  readonly label: string;
  /** Whether the terminal itself is off-screen and needs a preview card. */
  readonly showTerminal: boolean;
  readonly surfaces: readonly BlitSurface[];
}

export interface MusterPreviewUnit {
  /** Unit name without the instance prefix. */
  readonly name: string;
  readonly runs: readonly MusterPreviewRun[];
}

export interface MusterPreviewInstance {
  /** Keep equal instance names on different connections separate. */
  readonly connectionId: string;
  /** Null groups top-level units under Standalone. */
  readonly instance: string | null;
  readonly units: readonly MusterPreviewUnit[];
}

export interface MusterPreviewResources {
  readonly sessions: readonly BlitSession[];
  readonly surfaces: readonly BlitSurface[];
  readonly muster: readonly MusterPreviewInstance[];
}

/** Every terminal under this prefix belongs in the separate Muster block. */
export function isMusterSession(session: BlitSession): boolean {
  return session.tag.startsWith(MUSTER_TERMINAL_PREFIX);
}

/** Collapsing the Muster block also suspends its off-screen PTY streams. */
export function previewSessionsToWatch(
  panelSessions: readonly BlitSession[],
  musterExpanded: boolean,
  expandedStacks?: ReadonlySet<string>,
): readonly BlitSession[] {
  if (!musterExpanded) {
    return panelSessions.filter((session) => !isMusterSession(session));
  }
  if (!expandedStacks) return panelSessions;
  return panelSessions.filter(
    (session) =>
      !isMusterSession(session) ||
      expandedStacks.has(musterStackKeyForSession(session)),
  );
}

/** The user-facing part of a Muster terminal tag. */
export function musterSessionLabel(session: BlitSession): string {
  return session.tag.slice(MUSTER_TERMINAL_PREFIX.length) || session.tag;
}

/** Split a terminal tag into the hierarchy rendered by the preview panel. */
function musterHierarchyIdentity(
  session: BlitSession,
): MusterHierarchyIdentity | null {
  if (!isMusterSession(session)) return null;
  const rest = session.tag.slice(MUSTER_TERMINAL_PREFIX.length);
  const separator = rest.lastIndexOf("/");
  if (separator <= 0 || separator === rest.length - 1) return null;
  const qualifiedUnit = rest.slice(0, separator);
  const run = rest.slice(separator + 1);
  const instanceSeparator = qualifiedUnit.indexOf("/");
  if (instanceSeparator <= 0) {
    return { instance: null, unit: qualifiedUnit, qualifiedUnit, run };
  }
  return {
    instance: qualifiedUnit.slice(0, instanceSeparator),
    unit: qualifiedUnit.slice(instanceSeparator + 1),
    qualifiedUnit,
    run,
  };
}

/** Stable connection-scoped identity for a collapsible stack section. */
export function musterStackKey(
  connectionId: string,
  instance: string | null,
): string {
  return `${connectionId}\0${instance ?? ""}`;
}

function musterStackKeyForSession(session: BlitSession): string {
  return musterStackKey(
    session.connectionId,
    musterHierarchyIdentity(session)?.instance ?? null,
  );
}

/**
 * A normal run is tagged `muster/<unit>/<sequence>`. The unit itself may
 * contain slashes, so only the final separator belongs to the sequence.
 * Control-command terminals (`.../stop`, `.../reload`) remain Muster-owned
 * but intentionally have no run identity and therefore cannot own surfaces.
 */
function musterRunIdentity(session: BlitSession): MusterRunIdentity | null {
  const identity = musterHierarchyIdentity(session);
  if (!identity || !/^\d+$/.test(identity.run)) return null;
  return { unit: identity.qualifiedUnit, sequence: identity.run };
}

/**
 * Keep this byte-for-byte equivalent to muster supervisor's `app_id_for`.
 * The stable FNV-1a stamp is how a surface remains attributable after the
 * supervisor that created its Wayland socket has restarted.
 */
export function musterAppIdForUnit(unit: string): string {
  let hash = 0xcbf29ce484222325n;
  for (const byte of new TextEncoder().encode(unit)) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return `muster-${hash.toString(16).padStart(16, "0")}`;
}

function ownerKey(
  connectionId: string,
  appId: string,
  instanceId: string,
): string {
  return `${connectionId}\0${appId}\0${instanceId}`;
}

/**
 * Split the right-side panel into its ordinary flat resources and the Muster
 * block rendered below them. Surfaces match terminals through the server's
 * trusted socket stamp, never through the self-reported Wayland app id.
 *
 * `allSessions` is deliberately broader than `panelSessions`: a Muster
 * terminal can be displayed in a pane while one of its windows is parked.
 * That window still gets a terminal parent in the hierarchy, but the terminal
 * preview itself is omitted because it is already on screen.
 */
export function groupMusterPreviewResources(
  panelSessions: readonly BlitSession[],
  allSessions: readonly BlitSession[],
  panelSurfaces: readonly BlitSurface[],
): MusterPreviewResources {
  const sessions = panelSessions.filter((session) => !isMusterSession(session));
  const shownSessionIds = new Set(panelSessions.map((session) => session.id));
  const owners = new Map<string, BlitSession>();

  for (const session of allSessions) {
    if (session.state === "closed") continue;
    const identity = musterRunIdentity(session);
    if (!identity) continue;
    owners.set(
      ownerKey(
        session.connectionId,
        musterAppIdForUnit(identity.unit),
        identity.sequence,
      ),
      session,
    );
  }

  const groups: Array<{
    session: BlitSession;
    showTerminal: boolean;
    surfaces: BlitSurface[];
  }> = [];
  const groupsBySession = new Map<string, (typeof groups)[number]>();
  const ensureGroup = (session: BlitSession) => {
    let group = groupsBySession.get(session.id);
    if (!group) {
      group = {
        session,
        showTerminal: shownSessionIds.has(session.id),
        surfaces: [],
      };
      groupsBySession.set(session.id, group);
      groups.push(group);
    }
    return group;
  };

  // Muster terminals retain the panel's existing arrival order, including
  // stop/reload command terminals and retained runs with no live windows.
  for (const session of panelSessions) {
    if (isMusterSession(session)) ensureGroup(session);
  }

  const surfaces: BlitSurface[] = [];
  for (const surface of panelSurfaces) {
    const origin = surface.origin;
    const owner = origin
      ? owners.get(
          ownerKey(surface.connectionId, origin.appId, origin.instanceId),
        )
      : undefined;
    if (owner) ensureGroup(owner).surfaces.push(surface);
    else surfaces.push(surface);
  }

  const muster: Array<{
    connectionId: string;
    instance: string | null;
    units: Array<{ name: string; runs: MusterPreviewRun[] }>;
  }> = [];
  const instances = new Map<string, (typeof muster)[number]>();
  const units = new Map<string, (typeof muster)[number]["units"][number]>();

  for (const group of groups) {
    const identity = musterHierarchyIdentity(group.session) ?? {
      instance: null,
      unit: musterSessionLabel(group.session),
      qualifiedUnit: musterSessionLabel(group.session),
      run: `#${group.session.ptyId}`,
    };
    const instanceKey = musterStackKey(
      group.session.connectionId,
      identity.instance,
    );
    let instance = instances.get(instanceKey);
    if (!instance) {
      instance = {
        connectionId: group.session.connectionId,
        instance: identity.instance,
        units: [],
      };
      instances.set(instanceKey, instance);
      muster.push(instance);
    }

    const unitKey = `${instanceKey}\0${identity.unit}`;
    let unit = units.get(unitKey);
    if (!unit) {
      unit = { name: identity.unit, runs: [] };
      units.set(unitKey, unit);
      instance.units.push(unit);
    }
    unit.runs.push({ ...group, label: identity.run });
  }

  return { sessions, surfaces, muster };
}
