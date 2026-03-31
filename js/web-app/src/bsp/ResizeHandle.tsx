import { useCallback, useRef, useState } from "react";

const HANDLE_SIZE = 2;

export function ResizeHandle({
  direction,
  onDrag,
}: {
  direction: "horizontal" | "vertical";
  onDrag: (fraction: number) => void;
}) {
  const [active, setActive] = useState(false);
  const [hover, setHover] = useState(false);
  const startRef = useRef(0);
  const containerRef = useRef(0);

  const handlePointerDown = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault();
      (e.target as HTMLElement).setPointerCapture(e.pointerId);
      setActive(true);

      const isHoriz = direction === "horizontal";
      const start = isHoriz ? e.clientX : e.clientY;
      const parent = (e.target as HTMLElement).parentElement;
      const containerSize = parent
        ? isHoriz
          ? parent.clientWidth
          : parent.clientHeight
        : 1;

      startRef.current = start;
      containerRef.current = containerSize;

      const onMove = (me: PointerEvent) => {
        const current = isHoriz ? me.clientX : me.clientY;
        const delta = current - startRef.current;
        startRef.current = current;
        onDrag(delta / containerRef.current);
      };

      const onUp = () => {
        setActive(false);
        document.removeEventListener("pointermove", onMove);
        document.removeEventListener("pointerup", onUp);
      };

      document.addEventListener("pointermove", onMove);
      document.addEventListener("pointerup", onUp);
    },
    [direction, onDrag],
  );

  const isHoriz = direction === "horizontal";
  const bg = active
    ? "rgba(128,128,128,0.5)"
    : hover
      ? "rgba(128,128,128,0.3)"
      : "rgba(128,128,128,0.15)";

  return (
    <div
      onPointerDown={handlePointerDown}
      onPointerEnter={() => setHover(true)}
      onPointerLeave={() => setHover(false)}
      style={{
        flexShrink: 0,
        width: isHoriz ? HANDLE_SIZE : "100%",
        height: isHoriz ? "100%" : HANDLE_SIZE,
        cursor: isHoriz ? "col-resize" : "row-resize",
        background: bg,
        transition: "background 0.1s",
        touchAction: "none",
      }}
    />
  );
}
