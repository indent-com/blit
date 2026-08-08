import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startTouchDrag } from "../ide/tileDrag";

/** jsdom has no DataTransfer; the drag only ever uses it as a MIME map. */
class FakeDataTransfer {
  private store = new Map<string, string>();
  effectAllowed = "";
  setData(type: string, value: string): void {
    this.store.set(type, value);
  }
  getData(type: string): string {
    return this.store.get(type) ?? "";
  }
  get types(): string[] {
    return [...this.store.keys()];
  }
}

/** Nor DragEvent; a MouseEvent that carries the transfer is all fire()
 *  builds. */
class FakeDragEvent extends MouseEvent {
  dataTransfer: FakeDataTransfer | null;
  constructor(
    type: string,
    init: MouseEventInit & { dataTransfer?: FakeDataTransfer },
  ) {
    super(type, init);
    this.dataTransfer = init.dataTransfer ?? null;
  }
}

/** Nor PointerEvent; a MouseEvent carries the same fields. */
function pointerEvent(type: string, x: number, y: number): Event {
  const ev = new MouseEvent(type, {
    bubbles: true,
    cancelable: true,
    clientX: x,
    clientY: y,
  });
  Object.defineProperty(ev, "pointerId", { value: 1 });
  Object.defineProperty(ev, "pointerType", { value: "touch" });
  return ev;
}

describe("startTouchDrag holdMenu", () => {
  const realDataTransfer = globalThis.DataTransfer;
  const realDragEvent = globalThis.DragEvent;
  const realElementFromPoint = document.elementFromPoint;
  let el: HTMLDivElement;

  beforeEach(() => {
    vi.useFakeTimers();
    (globalThis as Record<string, unknown>).DataTransfer = FakeDataTransfer;
    (globalThis as Record<string, unknown>).DragEvent = FakeDragEvent;
    el = document.createElement("div");
    document.body.appendChild(el);
  });

  afterEach(() => {
    vi.useRealTimers();
    (globalThis as Record<string, unknown>).DataTransfer = realDataTransfer;
    (globalThis as Record<string, unknown>).DragEvent = realDragEvent;
    document.elementFromPoint = realElementFromPoint;
    el.remove();
  });

  /** Press on the row; startTouchDrag reads currentTarget, so the call has
   *  to happen inside a real dispatch. */
  function press(fill: ((data: DataTransfer) => void) | null): void {
    el.addEventListener("pointerdown", (e) =>
      startTouchDrag(e as PointerEvent, fill, "long-press", {
        holdMenu: true,
      }),
    );
    el.dispatchEvent(pointerEvent("pointerdown", 10, 10));
  }

  it("opens the menu when a hold releases without moving", () => {
    const menus: { x: number; y: number }[] = [];
    let drags = 0;
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      menus.push({
        x: (e as MouseEvent).clientX,
        y: (e as MouseEvent).clientY,
      });
    });
    el.addEventListener("dragstart", () => drags++);

    press(null); // a row with nothing to drag still has a menu
    vi.advanceTimersByTime(500); // past the 450ms hold
    window.dispatchEvent(pointerEvent("pointerup", 10, 10));

    expect(menus).toEqual([{ x: 10, y: 10 }]);
    expect(drags).toBe(0);

    // The release also produces a click; the row must never see it.
    let clicks = 0;
    el.addEventListener("click", () => clicks++);
    const click = new MouseEvent("click", { bubbles: true, cancelable: true });
    el.dispatchEvent(click);
    expect(clicks).toBe(0);
    expect(click.defaultPrevented).toBe(true);
  });

  it("does not drop on itself when a held drag never moved", () => {
    const seen: string[] = [];
    el.addEventListener("dragstart", () => seen.push("dragstart"));
    el.addEventListener("drop", () => seen.push("drop"));
    el.addEventListener("dragend", () => seen.push("dragend"));
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      seen.push("contextmenu");
    });
    document.elementFromPoint = () => el;

    press((dt) => dt.setData("text/plain", "x"));
    vi.advanceTimersByTime(500); // the hold begins the drag
    window.dispatchEvent(pointerEvent("pointerup", 10, 10));

    // The drag is balanced, but the payload is not dropped on the row it
    // lifted from — the release is a right-click instead.
    expect(seen).toEqual(["dragstart", "dragend", "contextmenu"]);
  });

  it("drags as before when the held finger moves", () => {
    const seen: string[] = [];
    el.addEventListener("dragstart", () => seen.push("dragstart"));
    el.addEventListener("drop", () => seen.push("drop"));
    el.addEventListener("dragend", () => seen.push("dragend"));
    el.addEventListener("contextmenu", () => seen.push("contextmenu"));
    document.elementFromPoint = () => el;

    press((dt) => dt.setData("text/plain", "x"));
    vi.advanceTimersByTime(500);
    window.dispatchEvent(pointerEvent("pointermove", 40, 40));
    window.dispatchEvent(pointerEvent("pointerup", 40, 40));

    expect(seen).toEqual(["dragstart", "drop", "dragend"]);
  });

  it("leaves a scrolled press alone", () => {
    const seen: string[] = [];
    el.addEventListener("contextmenu", () => seen.push("contextmenu"));

    press(null);
    window.dispatchEvent(pointerEvent("pointermove", 10, 40)); // a scroll
    vi.advanceTimersByTime(500);
    window.dispatchEvent(pointerEvent("pointerup", 10, 40));

    expect(seen).toEqual([]);
  });
});
