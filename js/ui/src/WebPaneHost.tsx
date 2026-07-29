import { createEffect, onCleanup, type JSX } from "solid-js";

export interface WebPaneHostRegistration {
  element: HTMLDivElement;
  interactive: boolean;
  focused: boolean;
  onFocusRequest?: () => void;
}

export type WebPaneHostRegistrar = (
  assignment: string,
  hostId: string,
  registration: WebPaneHostRegistration | null,
) => void;

/**
 * A lightweight destination for a Workspace-owned WebPane. The iframe itself
 * stays mounted in a fixed overlay; moving an assignment between these hosts
 * therefore changes only its geometry, not its browsing context.
 */
export function WebPaneHost(props: {
  assignment: string;
  hostId: string;
  register: WebPaneHostRegistrar;
  interactive?: boolean;
  focused?: boolean;
  onFocusRequest?: () => void;
  style?: JSX.CSSProperties;
}): JSX.Element {
  let element!: HTMLDivElement;

  createEffect(() => {
    const assignment = props.assignment;
    const hostId = props.hostId;
    props.register(assignment, hostId, {
      element,
      interactive: props.interactive ?? true,
      focused: props.focused ?? false,
      onFocusRequest: props.onFocusRequest,
    });
    onCleanup(() => props.register(assignment, hostId, null));
  });

  return (
    <div
      ref={element}
      data-blit-web-pane-host={props.hostId}
      style={{
        width: "100%",
        height: "100%",
        position: "relative",
        ...props.style,
      }}
    />
  );
}
