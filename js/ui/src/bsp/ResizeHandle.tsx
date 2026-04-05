import { createSignal } from "solid-js";

const HANDLE_SIZE = 2;

export function ResizeHandle(props: {
  direction: "horizontal" | "vertical";
  onDrag: (fraction: number) => void;
}) {
  const [active, setActive] = createSignal(false);
  const [hover, setHover] = createSignal(false);

  let startPos = 0;
  let containerSize = 0;

  function handlePointerDown(e: PointerEvent) {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    setActive(true);

    const isHoriz = props.direction === "horizontal";
    startPos = isHoriz ? e.clientX : e.clientY;
    const parent = (e.target as HTMLElement).parentElement;
    containerSize = parent
      ? isHoriz
        ? parent.clientWidth
        : parent.clientHeight
      : 1;

    const onMove = (me: PointerEvent) => {
      const current = isHoriz ? me.clientX : me.clientY;
      const delta = current - startPos;
      startPos = current;
      props.onDrag(delta / containerSize);
    };

    const onUp = () => {
      setActive(false);
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
    };

    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  }

  const isHoriz = () => props.direction === "horizontal";
  const bg = () =>
    active()
      ? "rgba(128,128,128,0.5)"
      : hover()
        ? "rgba(128,128,128,0.3)"
        : "rgba(128,128,128,0.15)";

  return (
    <div
      onPointerDown={handlePointerDown}
      onPointerEnter={() => setHover(true)}
      onPointerLeave={() => setHover(false)}
      style={{
        "flex-shrink": 0,
        width: isHoriz() ? `${HANDLE_SIZE}px` : "100%",
        height: isHoriz() ? "100%" : `${HANDLE_SIZE}px`,
        cursor: isHoriz() ? "col-resize" : "row-resize",
        background: bg(),
        transition: "background 0.1s",
        "touch-action": "none",
      }}
    />
  );
}
