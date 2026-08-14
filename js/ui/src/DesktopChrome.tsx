import {
  For,
  Show,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  type JSX,
} from "solid-js";
import { Portal } from "solid-js/web";
import {
  MENU_NODE_CHECKMARK,
  MENU_NODE_ENABLED,
  MENU_NODE_RADIO,
  MENU_NODE_SEPARATOR,
  MENU_NODE_SUBMENU,
  MENU_NODE_VISIBLE,
  TRAY_MENU_OK,
  TRAY_STATUS_NEEDS_ATTENTION,
  TRAY_STATUS_PASSIVE,
  type BlitConnectionSnapshot,
  type BlitWorkspace,
  type DesktopImage,
  type DesktopNotification,
  type TrayItem,
  type TrayMenu,
  type TrayMenuNode,
} from "@blit-sh/core";
import { desktopWorkerRegistration } from "./preview";
import {
  desktopDelivery,
  desktopNativeTag,
  matchesDesktopNotification,
  trayPrimaryOpensMenu,
} from "./desktopPresentation";
import { t } from "./i18n";
import { ui, z, type Theme, type UIScale } from "./theme";

type TrayEntry = {
  connectionId: string;
  connectionLabel: string;
  readOnly: boolean;
  item: TrayItem;
};

type NotificationEntry = {
  connectionId: string;
  connectionLabel: string;
  bootGeneration: bigint | null;
  readOnly: boolean;
  item: DesktopNotification;
};

type Toast = NotificationEntry & { key: string };
type MenuState = { entry: TrayEntry; menu: TrayMenu };

function imageUrl(image: DesktopImage): string | undefined {
  if (image.png.length === 0) return undefined;
  let binary = "";
  for (let offset = 0; offset < image.png.length; offset += 0x8000) {
    binary += String.fromCharCode(
      ...image.png.subarray(offset, offset + 0x8000),
    );
  }
  return `data:image/png;base64,${btoa(binary)}`;
}

function notificationKey(connectionId: string, id: number): string {
  return `${connectionId}:${id}`;
}

async function postWorker(message: object): Promise<void> {
  const registration = await desktopWorkerRegistration();
  registration?.active?.postMessage(message);
}

function notificationTitle(item: DesktopNotification): string {
  return item.summary || item.appName || t("desktop.notification");
}

function Popup(props: {
  theme: Theme;
  scale: UIScale;
  children: JSX.Element;
  width?: string;
}) {
  return (
    <div
      style={{
        position: "absolute",
        bottom: "100%",
        right: 0,
        "margin-bottom": `${props.scale.tightGap}px`,
        width: props.width ?? "min(28em, calc(100vw - 2em))",
        "max-height": "min(70vh, 36em)",
        overflow: "auto",
        "background-color": props.theme.solidPanelBg,
        color: props.theme.fg,
        border: `1px solid ${props.theme.border}`,
        "box-shadow": "0 8px 24px rgba(0,0,0,0.35)",
        "z-index": z.statusMenu,
      }}
    >
      {props.children}
    </div>
  );
}

function NotificationCard(props: {
  entry: NotificationEntry;
  theme: Theme;
  scale: UIScale;
  toast?: boolean;
  invoke: (key: string | null) => void;
  dismiss: () => void;
}) {
  const icon = createMemo(() => imageUrl(props.entry.item.icon));
  const image = createMemo(() => imageUrl(props.entry.item.image));
  const defaultAction = () =>
    props.entry.item.actions.some((action) => action.key === "default");
  return (
    <article
      style={{
        display: "grid",
        "grid-template-columns": icon()
          ? "2.5em minmax(0, 1fr) auto"
          : "minmax(0, 1fr) auto",
        gap: `${props.scale.gap}px`,
        padding: `${props.scale.panelPadding}px`,
        "border-bottom": props.toast
          ? undefined
          : `1px solid ${props.theme.subtleBorder}`,
        "background-color": props.theme.solidPanelBg,
        color: props.theme.fg,
        "font-size": `${props.scale.md}px`,
      }}
    >
      <Show when={icon()}>
        {(src) => (
          <img
            src={src()}
            alt=""
            width={40}
            height={40}
            style={{ "object-fit": "contain", "grid-row": "1 / span 3" }}
          />
        )}
      </Show>
      <button
        disabled={!defaultAction() || props.entry.readOnly}
        onClick={() => props.invoke(null)}
        style={{
          ...ui.btn,
          display: "block",
          "min-width": 0,
          "text-align": "left",
          cursor:
            defaultAction() && !props.entry.readOnly ? "pointer" : "default",
        }}
      >
        <strong style={{ display: "block", "overflow-wrap": "anywhere" }}>
          {notificationTitle(props.entry.item)}
        </strong>
        <Show when={props.entry.item.appName || props.entry.connectionLabel}>
          <small style={{ color: props.theme.dimFg }}>
            {[props.entry.item.appName, props.entry.connectionLabel]
              .filter(Boolean)
              .join(" · ")}
          </small>
        </Show>
        <Show when={props.entry.item.body}>
          <span
            style={{
              display: "block",
              "white-space": "pre-wrap",
              "overflow-wrap": "anywhere",
              "margin-top": `${props.scale.tightGap}px`,
            }}
          >
            {props.entry.item.body}
          </span>
        </Show>
      </button>
      <button
        disabled={props.entry.readOnly}
        onClick={props.dismiss}
        title={t("desktop.dismiss")}
        aria-label={t("desktop.dismiss")}
        style={{ ...ui.btn, "align-self": "start", color: props.theme.dimFg }}
      >
        ×
      </button>
      <Show when={image()}>
        {(src) => (
          <img
            src={src()}
            alt=""
            style={{
              "grid-column": icon() ? "2 / span 2" : "1 / span 2",
              "max-width": "100%",
              "max-height": "14em",
              "object-fit": "contain",
            }}
          />
        )}
      </Show>
      <Show
        when={props.entry.item.actions.some(
          (action) => action.key !== "default",
        )}
      >
        <div
          style={{
            "grid-column": icon() ? "2 / span 2" : "1 / span 2",
            display: "flex",
            "flex-wrap": "wrap",
            gap: `${props.scale.tightGap}px`,
          }}
        >
          <For
            each={props.entry.item.actions.filter(
              (action) => action.key !== "default",
            )}
          >
            {(action) => (
              <button
                disabled={props.entry.readOnly}
                onClick={() => props.invoke(action.key)}
                style={{
                  ...ui.btn,
                  padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                  border: `1px solid ${props.theme.border}`,
                }}
              >
                {action.label}
              </button>
            )}
          </For>
        </div>
      </Show>
    </article>
  );
}

function MenuNodes(props: {
  nodes: readonly TrayMenuNode[];
  parentId: number;
  depth: number;
  readOnly: boolean;
  theme: Theme;
  scale: UIScale;
  openSubmenu: (id: number) => void;
  click: (id: number) => void;
}) {
  const children = createMemo(() =>
    props.nodes
      .filter(
        (node) =>
          node.parentId === props.parentId &&
          (node.flags & MENU_NODE_VISIBLE) !== 0,
      )
      .sort((a, b) => a.position - b.position),
  );
  return (
    <div role={props.depth === 0 ? "menu" : "group"}>
      <For each={children()}>
        {(node) => {
          const separator = () => (node.flags & MENU_NODE_SEPARATOR) !== 0;
          const submenu = () => (node.flags & MENU_NODE_SUBMENU) !== 0;
          const checked = () => node.toggleState === 1;
          const role = () =>
            node.flags & MENU_NODE_RADIO
              ? "menuitemradio"
              : node.flags & MENU_NODE_CHECKMARK
                ? "menuitemcheckbox"
                : "menuitem";
          return (
            <Show
              when={!separator()}
              fallback={
                <hr
                  role="separator"
                  style={{
                    border: 0,
                    "border-top": `1px solid ${props.theme.border}`,
                  }}
                />
              }
            >
              <button
                role={role()}
                aria-checked={
                  node.flags & (MENU_NODE_RADIO | MENU_NODE_CHECKMARK)
                    ? checked()
                    : undefined
                }
                aria-haspopup={submenu() ? "menu" : undefined}
                disabled={
                  props.readOnly || (node.flags & MENU_NODE_ENABLED) === 0
                }
                onClick={() =>
                  submenu() ? props.openSubmenu(node.id) : props.click(node.id)
                }
                style={{
                  ...ui.btn,
                  width: "100%",
                  display: "grid",
                  "grid-template-columns": "1.25em minmax(0, 1fr) auto",
                  gap: `${props.scale.tightGap}px`,
                  padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                  "padding-left": `${props.scale.controlX + props.depth * props.scale.gap}px`,
                  "text-align": "left",
                  opacity:
                    (node.flags & MENU_NODE_ENABLED) === 0 || props.readOnly
                      ? 0.5
                      : 1,
                }}
              >
                <span>
                  <Show
                    when={imageUrl(node.icon)}
                    fallback={
                      node.toggleState >= 0 ? (checked() ? "✓" : "") : ""
                    }
                  >
                    {(src) => <img src={src()} alt="" width={16} height={16} />}
                  </Show>
                </span>
                <span>{node.label}</span>
                <span>{submenu() ? "›" : ""}</span>
              </button>
              <Show when={submenu()}>
                <MenuNodes
                  {...props}
                  parentId={node.id}
                  depth={props.depth + 1}
                />
              </Show>
            </Show>
          );
        }}
      </For>
    </div>
  );
}

export function DesktopChrome(props: {
  workspace: BlitWorkspace;
  connections: readonly BlitConnectionSnapshot[];
  connectionLabels: ReadonlyMap<string, string>;
  readOnlyConnections: ReadonlySet<string>;
  theme: Theme;
  scale: UIScale;
  compact: boolean;
}) {
  const [toasts, setToasts] = createSignal<Toast[]>([]);
  const [bellOpen, setBellOpen] = createSignal(false);
  const [trayOpen, setTrayOpen] = createSignal(false);
  const [menu, setMenu] = createSignal<MenuState | null>(null);
  const [permission, setPermission] = createSignal<NotificationPermission>(
    typeof Notification === "undefined" ? "denied" : Notification.permission,
  );
  const toastTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const nativeShown = new Map<
    string,
    {
      tag: string;
      connectionId: string;
      bootGeneration: string;
      notificationId: number;
    }
  >();
  let root: HTMLSpanElement | undefined;

  const tray = createMemo<TrayEntry[]>(() => {
    const entries: TrayEntry[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktop || !connection) continue;
      for (const item of connection.desktopStore.tray.values()) {
        entries.push({
          connectionId: snapshot.id,
          connectionLabel: props.connectionLabels.get(snapshot.id) ?? "",
          readOnly: props.readOnlyConnections.has(snapshot.id),
          item,
        });
      }
    }
    return entries.sort(
      (a, b) =>
        props.connections.findIndex((item) => item.id === a.connectionId) -
          props.connections.findIndex((item) => item.id === b.connectionId) ||
        a.item.category - b.item.category ||
        a.item.trayId - b.item.trayId,
    );
  });
  const visibleTray = createMemo(() =>
    tray().filter((entry) => entry.item.status !== TRAY_STATUS_PASSIVE),
  );
  const overflowTray = createMemo(() => {
    const shown = new Set(
      visibleTray()
        .slice(0, props.compact ? 0 : 4)
        .map((entry) => `${entry.connectionId}:${entry.item.trayId}`),
    );
    return tray().filter(
      (entry) => !shown.has(`${entry.connectionId}:${entry.item.trayId}`),
    );
  });
  const desktopEnabled = createMemo(() =>
    props.connections.some((connection) => connection.supportsDesktop),
  );
  const notifications = createMemo<NotificationEntry[]>(() => {
    const entries: NotificationEntry[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktop || !connection) continue;
      for (const item of connection.desktopStore.notifications.values()) {
        entries.push({
          connectionId: snapshot.id,
          connectionLabel: props.connectionLabels.get(snapshot.id) ?? "",
          bootGeneration: snapshot.bootGeneration,
          readOnly: props.readOnlyConnections.has(snapshot.id),
          item,
        });
      }
    }
    return entries;
  });

  const invoke = (entry: NotificationEntry, key: string | null) => {
    if (entry.readOnly) return;
    const store = props.workspace.getConnection(
      entry.connectionId,
    )?.desktopStore;
    if (key == null) {
      store?.invokeDefault(entry.item.notificationId, entry.item.revision);
    } else {
      store?.invokeAction(entry.item.notificationId, entry.item.revision, key);
    }
  };
  const dismiss = (entry: NotificationEntry) => {
    if (entry.readOnly) return;
    props.workspace
      .getConnection(entry.connectionId)
      ?.desktopStore.dismiss(entry.item.notificationId, entry.item.revision);
  };

  const showNative = (entry: NotificationEntry) => {
    const tag = desktopNativeTag(
      entry.connectionId,
      entry.bootGeneration,
      entry.item.notificationId,
    );
    if (!tag) return;
    nativeShown.set(
      notificationKey(entry.connectionId, entry.item.notificationId),
      {
        tag,
        connectionId: entry.connectionId,
        bootGeneration: entry.bootGeneration!.toString(),
        notificationId: entry.item.notificationId,
      },
    );
    void postWorker({
      type: "blit-desktop-notification-show",
      tag,
      connectionId: entry.connectionId,
      bootGeneration: entry.bootGeneration!.toString(),
      notificationId: entry.item.notificationId,
      revision: entry.item.revision,
      title: notificationTitle(entry.item),
      body: entry.item.body,
      icon: imageUrl(entry.item.icon),
      image: imageUrl(entry.item.image),
    });
  };

  const raise = (entry: NotificationEntry) => {
    const delivery = desktopDelivery(document.visibilityState, permission());
    if (delivery === "native") {
      showNative(entry);
      return;
    }
    if (delivery === "toast") {
      const key = notificationKey(
        entry.connectionId,
        entry.item.notificationId,
      );
      setToasts((items) => [
        ...items.filter((item) => item.key !== key),
        { ...entry, key },
      ]);
      const previous = toastTimers.get(key);
      if (previous) clearTimeout(previous);
      toastTimers.set(
        key,
        setTimeout(
          () => {
            setToasts((items) => items.filter((item) => item.key !== key));
            toastTimers.delete(key);
          },
          entry.item.urgency === 2 ? 10_000 : 6_000,
        ),
      );
    }
  };

  createEffect(() => {
    const cleanups: (() => void)[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktop || !connection) continue;
      const label = props.connectionLabels.get(snapshot.id) ?? "";
      const readOnly = props.readOnlyConnections.has(snapshot.id);
      cleanups.push(
        connection.desktopStore.onNotificationRaised((item) =>
          raise({
            connectionId: snapshot.id,
            connectionLabel: label,
            bootGeneration: snapshot.bootGeneration,
            readOnly,
            item,
          }),
        ),
        connection.desktopStore.onTrayMenu((next) => {
          const entry = tray().find(
            (candidate) =>
              candidate.connectionId === snapshot.id &&
              candidate.item.trayId === next.trayId,
          );
          if (next.status === TRAY_MENU_OK && entry) {
            setMenu({ entry, menu: next });
          } else if (next.status !== TRAY_MENU_OK) {
            setMenu(null);
          }
        }),
      );
    }
    onCleanup(() => cleanups.forEach((cleanup) => cleanup()));
  });

  createEffect(() => {
    const active = new Set(
      notifications().map(
        (entry) =>
          `${entry.connectionId}:${entry.bootGeneration}:${entry.item.notificationId}:${entry.item.revision}`,
      ),
    );
    setToasts((items) =>
      items.filter((entry) =>
        active.has(
          `${entry.connectionId}:${entry.bootGeneration}:${entry.item.notificationId}:${entry.item.revision}`,
        ),
      ),
    );
    for (const [key, shown] of nativeShown) {
      const current = notifications().find(
        (entry) =>
          entry.connectionId === shown.connectionId &&
          String(entry.bootGeneration) === shown.bootGeneration &&
          entry.item.notificationId === shown.notificationId,
      );
      if (!current) {
        nativeShown.delete(key);
        void postWorker({
          type: "blit-desktop-notification-close",
          tag: shown.tag,
        });
      }
    }
  });

  const openTrayMenu = (entry: TrayEntry, parentId = 0) => {
    if (entry.readOnly) return;
    setTrayOpen(false);
    setBellOpen(false);
    props.workspace
      .getConnection(entry.connectionId)
      ?.desktopStore.openMenu(
        entry.item.trayId,
        menu()?.entry.item.trayId === entry.item.trayId
          ? menu()!.menu.menuRevision
          : 0,
        parentId,
      );
  };
  const activateTray = (entry: TrayEntry, touch = false) => {
    if (entry.readOnly) return;
    const store = props.workspace.getConnection(
      entry.connectionId,
    )?.desktopStore;
    if (trayPrimaryOpensMenu(entry.item.flags, touch)) {
      openTrayMenu(entry);
    } else {
      store?.activate(entry.item.trayId);
    }
  };

  onMount(() => {
    const pointer = (event: PointerEvent) => {
      if (root && !root.contains(event.target as Node)) {
        setBellOpen(false);
        setTrayOpen(false);
        setMenu(null);
      }
    };
    const key = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setBellOpen(false);
      setTrayOpen(false);
      setMenu(null);
    };
    const worker = (event: MessageEvent) => {
      const data = event.data as {
        type?: string;
        connectionId?: string;
        bootGeneration?: string;
        notificationId?: number;
        revision?: number;
      } | null;
      if (data?.type !== "blit-desktop-notification-click") return;
      const entry = notifications().find(
        (candidate) =>
          candidate.connectionId === data.connectionId &&
          candidate.bootGeneration?.toString() === data.bootGeneration &&
          matchesDesktopNotification(candidate.item, data),
      );
      if (
        entry &&
        entry.item.actions.some((action) => action.key === "default")
      ) {
        invoke(entry, null);
      }
    };
    document.addEventListener("pointerdown", pointer, true);
    document.addEventListener("keydown", key, true);
    navigator.serviceWorker?.addEventListener("message", worker);
    onCleanup(() => {
      document.removeEventListener("pointerdown", pointer, true);
      document.removeEventListener("keydown", key, true);
      navigator.serviceWorker?.removeEventListener("message", worker);
      toastTimers.forEach(clearTimeout);
    });
  });

  const trayButton = (entry: TrayEntry): JSX.Element => {
    let primaryPointerType: string | null = null;
    const icon = imageUrl(entry.item.icon);
    const title = [
      entry.item.tooltipTitle || entry.item.title || entry.item.appId,
      entry.item.tooltipBody,
      entry.connectionLabel,
    ]
      .filter(Boolean)
      .join("\n");
    return (
      <button
        disabled={entry.readOnly}
        onPointerDown={(event) => {
          primaryPointerType = event.pointerType;
        }}
        onPointerCancel={() => {
          primaryPointerType = null;
        }}
        onClick={() => {
          const touch = primaryPointerType === "touch";
          primaryPointerType = null;
          activateTray(entry, touch);
        }}
        onContextMenu={(event) => {
          primaryPointerType = null;
          event.preventDefault();
          openTrayMenu(entry);
        }}
        onAuxClick={(event) => {
          if (event.button !== 1 || entry.readOnly) return;
          props.workspace
            .getConnection(entry.connectionId)
            ?.desktopStore.secondaryActivate(entry.item.trayId);
        }}
        onWheel={(event) => {
          if (entry.readOnly) return;
          event.preventDefault();
          const horizontal = Math.abs(event.deltaX) > Math.abs(event.deltaY);
          const raw = horizontal ? event.deltaX : event.deltaY;
          props.workspace
            .getConnection(entry.connectionId)
            ?.desktopStore.scroll(
              entry.item.trayId,
              Math.max(-1_000, Math.min(1_000, Math.trunc(raw))),
              horizontal,
            );
        }}
        title={title}
        aria-label={title || t("desktop.trayItem")}
        aria-haspopup={
          trayPrimaryOpensMenu(entry.item.flags, true) ? "menu" : undefined
        }
        style={{
          ...ui.btn,
          width: `${props.scale.icon / 2}px`,
          height: `${props.scale.icon / 2}px`,
          display: "grid",
          "place-items": "center",
          padding: "2px",
          "border-radius": "3px",
          "background-color":
            entry.item.status === TRAY_STATUS_NEEDS_ATTENTION
              ? props.theme.warning
              : "transparent",
          opacity: entry.readOnly ? 0.5 : 1,
          "touch-action": "manipulation",
          "-webkit-touch-callout": "none",
        }}
      >
        <Show when={icon} fallback={<span>●</span>}>
          <img
            src={icon}
            alt=""
            draggable={false}
            style={{
              width: "100%",
              height: "100%",
              "object-fit": "contain",
              "pointer-events": "none",
            }}
          />
        </Show>
      </button>
    );
  };

  return (
    <span
      ref={root}
      data-blit-desktop-chrome=""
      style={{ display: "flex", "align-items": "center", position: "relative" }}
    >
      <For each={visibleTray().slice(0, props.compact ? 0 : 4)}>
        {trayButton}
      </For>
      <Show when={overflowTray().length > 0}>
        <button
          onClick={() => {
            setTrayOpen((open) => !open);
            setBellOpen(false);
          }}
          title={t("desktop.trayOverflow")}
          aria-label={t("desktop.trayOverflow")}
          aria-haspopup="menu"
          aria-expanded={trayOpen()}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          ◉{overflowTray().length}
        </button>
        <Show when={trayOpen()}>
          <Popup theme={props.theme} scale={props.scale} width="18em">
            <div role="menu" style={{ padding: `${props.scale.tightGap}px` }}>
              <For each={overflowTray()}>
                {(entry) => (
                  <div
                    style={{
                      display: "flex",
                      "align-items": "center",
                      gap: `${props.scale.gap}px`,
                    }}
                  >
                    {trayButton(entry)}
                    <span
                      style={{ "min-width": 0, "overflow-wrap": "anywhere" }}
                    >
                      {entry.item.title ||
                        entry.item.appId ||
                        t("desktop.trayItem")}
                      <Show when={entry.connectionLabel}>
                        <small
                          style={{ display: "block", color: props.theme.dimFg }}
                        >
                          {entry.connectionLabel}
                        </small>
                      </Show>
                    </span>
                  </div>
                )}
              </For>
            </div>
          </Popup>
        </Show>
      </Show>
      <Show
        when={
          desktopEnabled() &&
          (notifications().length > 0 || permission() !== "granted")
        }
      >
        <button
          onClick={() => {
            setBellOpen((open) => !open);
            setTrayOpen(false);
          }}
          title={t("desktop.notifications")}
          aria-label={t("desktop.notifications")}
          aria-haspopup="menu"
          aria-expanded={bellOpen()}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          ♢
          <Show when={notifications().length > 0}>
            {notifications().length}
          </Show>
        </button>
        <Show when={bellOpen()}>
          <Popup theme={props.theme} scale={props.scale}>
            <Show when={permission() === "default"}>
              <button
                onClick={async () => {
                  if (typeof Notification === "undefined") return;
                  const result = await Notification.requestPermission();
                  setPermission(result);
                  if (result === "granted") await desktopWorkerRegistration();
                }}
                style={{
                  ...ui.btn,
                  width: "100%",
                  padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                  "text-align": "left",
                  border: `1px solid ${props.theme.border}`,
                }}
              >
                {t("desktop.enableSystemNotifications")}
              </button>
            </Show>
            <Show when={permission() === "denied"}>
              <p
                style={{
                  margin: 0,
                  padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                  color: props.theme.dimFg,
                }}
              >
                {t("desktop.systemNotificationsBlocked")}
              </p>
            </Show>
            <Show
              when={notifications().length > 0}
              fallback={
                <p style={{ padding: `${props.scale.panelPadding}px` }}>
                  {t("desktop.noNotifications")}
                </p>
              }
            >
              <For each={notifications()}>
                {(entry) => (
                  <NotificationCard
                    entry={entry}
                    theme={props.theme}
                    scale={props.scale}
                    invoke={(key) => invoke(entry, key)}
                    dismiss={() => dismiss(entry)}
                  />
                )}
              </For>
            </Show>
          </Popup>
        </Show>
      </Show>
      <Show when={menu()} keyed>
        {(state) => (
          <Popup theme={props.theme} scale={props.scale} width="20em">
            <MenuNodes
              nodes={state.menu.nodes}
              parentId={0}
              depth={0}
              readOnly={state.entry.readOnly}
              theme={props.theme}
              scale={props.scale}
              openSubmenu={(id) => openTrayMenu(state.entry, id)}
              click={(id) => {
                props.workspace
                  .getConnection(state.entry.connectionId)
                  ?.desktopStore.clickMenuItem(
                    state.entry.item.trayId,
                    state.menu.menuRevision,
                    id,
                  );
                setMenu(null);
              }}
            />
          </Popup>
        )}
      </Show>
      <Portal>
        <div
          aria-live="polite"
          style={{
            position: "fixed",
            right: "1em",
            bottom: "3em",
            width: "min(28em, calc(100vw - 2em))",
            display: "flex",
            "flex-direction": "column",
            gap: `${props.scale.gap}px`,
            "z-index": z.disconnected,
            "pointer-events": "none",
          }}
        >
          <For each={toasts()}>
            {(toast) => (
              <div
                role="status"
                style={{
                  border: `1px solid ${props.theme.border}`,
                  "box-shadow": "0 8px 24px rgba(0,0,0,0.35)",
                  "pointer-events": "auto",
                }}
              >
                <NotificationCard
                  entry={toast}
                  theme={props.theme}
                  scale={props.scale}
                  toast
                  invoke={(key) => invoke(toast, key)}
                  dismiss={() => dismiss(toast)}
                />
              </div>
            )}
          </For>
        </div>
      </Portal>
    </span>
  );
}
