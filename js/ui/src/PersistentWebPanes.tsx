import {
  For,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  type JSX,
} from "solid-js";
import { Portal } from "solid-js/web";
import { parseWebAssignment } from "./bsp/layout";
import { WebPane, type WebPaneHandle } from "./WebPane";
import type {
  WebPaneHostRegistrar,
  WebPaneHostRegistration,
} from "./WebPaneHost";
import { selectWebPaneHost } from "./webPaneHostSelection";

interface RegisteredHost extends WebPaneHostRegistration {
  hostId: string;
}

export interface WebPaneHostRegistry {
  register: WebPaneHostRegistrar;
  hosts: (assignment: string) => readonly RegisteredHost[];
  version: () => number;
}

/** Registry shared by lightweight hosts and the persistent iframe layer. */
export function createWebPaneHostRegistry(): WebPaneHostRegistry {
  const hosts = new Map<string, Map<string, WebPaneHostRegistration>>();
  const [version, setVersion] = createSignal(0);
  const bump = () => setVersion((value) => value + 1);
  const observer = new ResizeObserver(bump);

  const register: WebPaneHostRegistrar = (assignment, hostId, registration) => {
    let assignmentHosts = hosts.get(assignment);
    const previous = assignmentHosts?.get(hostId);
    if (previous) observer.unobserve(previous.element);

    if (registration) {
      if (!assignmentHosts) {
        assignmentHosts = new Map();
        hosts.set(assignment, assignmentHosts);
      }
      assignmentHosts.set(hostId, registration);
      observer.observe(registration.element);
    } else if (assignmentHosts) {
      assignmentHosts.delete(hostId);
      if (assignmentHosts.size === 0) hosts.delete(assignment);
    }
    bump();
  };

  // ResizeObserver covers size changes. Capture scroll as well because a host
  // can move without resizing when an ancestor scrolls.
  window.addEventListener("scroll", bump, true);
  window.addEventListener("resize", bump);
  onCleanup(() => {
    observer.disconnect();
    window.removeEventListener("scroll", bump, true);
    window.removeEventListener("resize", bump);
  });

  return {
    register,
    hosts: (assignment) =>
      Array.from(hosts.get(assignment) ?? [], ([hostId, registration]) => ({
        hostId,
        ...registration,
      })),
    version,
  };
}

interface HostPlacement {
  host: RegisteredHost;
  rect: DOMRect;
}

function placementFor(
  registry: WebPaneHostRegistry,
  assignment: string,
): HostPlacement | null {
  void registry.version();
  const visible = registry
    .hosts(assignment)
    .map((host) => ({ host, rect: host.element.getBoundingClientRect() }))
    .filter(
      ({ host, rect }) =>
        host.element.isConnected && rect.width > 0 && rect.height > 0,
    );
  const selected = selectWebPaneHost(visible.map(({ host }) => host));
  return selected
    ? (visible.find(({ host }) => host === selected) ?? null)
    : null;
}

/**
 * One stable WebPane per assignment. Hosts can appear and disappear around it;
 * a one-turn removal grace period bridges foreground↔dock transfers so Solid
 * never destroys the iframe browsing context during background/restore.
 */
export function PersistentWebPanes(props: {
  assignments: readonly string[];
  registry: WebPaneHostRegistry;
  onHandle: (assignment: string, handle: WebPaneHandle) => void;
}): JSX.Element {
  const [mounted, setMounted] = createSignal<string[]>([]);
  const removalTimers = new Map<string, ReturnType<typeof setTimeout>>();

  createEffect(() => {
    const next = new Set(props.assignments);
    for (const assignment of next) {
      const timer = removalTimers.get(assignment);
      if (timer !== undefined) {
        clearTimeout(timer);
        removalTimers.delete(assignment);
      }
    }
    setMounted((previous) => {
      const additions = Array.from(next).filter(
        (assignment) => !previous.includes(assignment),
      );
      for (const assignment of previous) {
        if (next.has(assignment) || removalTimers.has(assignment)) continue;
        removalTimers.set(
          assignment,
          setTimeout(() => {
            removalTimers.delete(assignment);
            if (!props.assignments.includes(assignment)) {
              setMounted((current) =>
                current.filter((value) => value !== assignment),
              );
            }
          }, 0),
        );
      }
      return additions.length > 0 ? [...previous, ...additions] : previous;
    });
  });

  onCleanup(() => {
    for (const timer of removalTimers.values()) clearTimeout(timer);
  });

  return (
    <Portal mount={document.body}>
      <For each={mounted()}>
        {(assignment) => {
          const web = parseWebAssignment(assignment)!;
          const placement = createMemo(() =>
            placementFor(props.registry, assignment),
          );
          const style = (): JSX.CSSProperties => {
            const current = placement();
            if (!current) return { display: "none" };
            return {
              position: "fixed",
              left: `${current.rect.left}px`,
              top: `${current.rect.top}px`,
              width: `${current.rect.width}px`,
              height: `${current.rect.height}px`,
              overflow: "hidden",
              "z-index": 5,
              "pointer-events": current.host.interactive ? "auto" : "none",
            };
          };
          return (
            <div style={style()}>
              <WebPane
                dest={web.connectionId}
                url={web.url}
                onHandle={(handle) => props.onHandle(assignment, handle)}
                onFocusRequest={() => placement()?.host.onFocusRequest?.()}
              />
            </div>
          );
        }}
      </For>
    </Portal>
  );
}
