import { describe, expect, it } from "vitest";
import {
  negotiatedCameraFormat,
  sameConnections,
  sameDevices,
} from "../mediaDevices";

const track = (
  width: number,
  height: number,
  frameRate = 30,
): MediaStreamTrack =>
  ({ getSettings: () => ({ width, height, frameRate }) }) as MediaStreamTrack;

describe("negotiatedCameraFormat", () => {
  it("keeps a mode that already fits", () => {
    expect(negotiatedCameraFormat(track(1280, 720), 30)).toEqual([
      1280, 720, 30,
    ]);
  });

  it("keeps a tablet's portrait aspect instead of squashing it", () => {
    // The bug this exists for: 1280 is over the 1080 cap and 720 is not, so
    // clamping each axis on its own announced 720x1080 for a 9:16 picture and
    // every face arrived a fifth too wide.
    const [width, height] = negotiatedCameraFormat(track(720, 1280), 30);
    expect(height).toBe(1080);
    expect(width / height).toBeCloseTo(720 / 1280, 2);
  });

  it("scales both axes down together when both are over", () => {
    expect(negotiatedCameraFormat(track(3840, 2160), 30)).toEqual([
      1920, 1080, 30,
    ]);
  });

  it("rounds to even, because 4:2:0 cannot subsample an odd extent", () => {
    const [width, height] = negotiatedCameraFormat(track(641, 481), 30);
    expect(width % 2).toBe(0);
    expect(height % 2).toBe(0);
  });

  it("caps the cadence without touching the extents", () => {
    expect(negotiatedCameraFormat(track(640, 480, 60), 30)).toEqual([
      640, 480, 30,
    ]);
  });
});

const device = (
  deviceId: string,
  kind: MediaDeviceKind,
  label = "",
  groupId = "g",
): MediaDeviceInfo => ({ deviceId, kind, label, groupId }) as MediaDeviceInfo;

describe("sameDevices", () => {
  it("treats a re-enumeration of the same devices as unchanged", () => {
    // The point of the whole comparison: `enumerateDevices` allocates fresh
    // objects every call, so identity says "changed" when nothing did.
    const first = [
      device("front", "videoinput", "Front Camera"),
      device("back", "videoinput", "Back Camera"),
    ];
    const second = [
      device("front", "videoinput", "Front Camera"),
      device("back", "videoinput", "Back Camera"),
    ];
    expect(first[0]).not.toBe(second[0]);
    expect(sameDevices(first, second)).toBe(true);
  });

  it("notices the labels appearing after permission is granted", () => {
    // Safari fills both id and label in only once a capture has been granted;
    // that is a real change and the picker has to rebuild for it.
    expect(
      sameDevices(
        [device("front", "videoinput", "")],
        [device("front", "videoinput", "Front Camera")],
      ),
    ).toBe(false);
  });

  it("notices a device appearing, going away, or being replaced", () => {
    const one = [device("front", "videoinput", "Front Camera")];
    expect(
      sameDevices(one, [...one, device("back", "videoinput", "Back Camera")]),
    ).toBe(false);
    expect(sameDevices(one, [])).toBe(false);
    expect(
      sameDevices(one, [device("other", "videoinput", "Front Camera")]),
    ).toBe(false);
  });

  it("notices a reordering, because the picker renders in list order", () => {
    const front = device("front", "videoinput", "Front Camera");
    const back = device("back", "videoinput", "Back Camera");
    expect(sameDevices([front, back], [back, front])).toBe(false);
  });

  it("ignores a groupId that moved without any device changing", () => {
    expect(
      sameDevices(
        [device("front", "videoinput", "Front Camera", "group-a")],
        [device("front", "videoinput", "Front Camera", "group-b")],
      ),
    ).toBe(true);
  });

  it("distinguishes kinds that share an id", () => {
    expect(
      sameDevices(
        [device("default", "audioinput", "Default")],
        [device("default", "audiooutput", "Default")],
      ),
    ).toBe(false);
  });
});

describe("negotiatedCameraFormat with a measured frame size", () => {
  it("believes the element over settings that disagree", () => {
    // The stretch this exists for: the tablet's settings said 720x1280 while
    // the element painted 1280x720, so the declared portrait box was filled
    // with a landscape picture and every frame arrived squeezed.
    expect(
      negotiatedCameraFormat(track(720, 1280), 30, {
        width: 1280,
        height: 720,
      }),
    ).toEqual([1280, 720, 30]);
  });

  it("falls back to settings when nothing could be measured", () => {
    expect(negotiatedCameraFormat(track(640, 480), 30, null)).toEqual([
      640, 480, 30,
    ]);
  });

  it("fits a measured size that is over the cap", () => {
    const [width, height] = negotiatedCameraFormat(track(0, 0), 30, {
      width: 2560,
      height: 1440,
    });
    expect([width, height]).toEqual([1920, 1080]);
  });
});

/**
 * Speaker routing re-applies whenever the set of connections changes. The list
 * it watches is derived from a workspace snapshot, and that snapshot is rebuilt
 * with fresh array and object identities on every remote change — so identity
 * comparison on the list itself reports a change constantly. A remote media
 * player moving between playing and paused produced one, and each report cost a
 * `setSinkId` on a live context.
 */
describe("sameConnections", () => {
  const connection = (id: string) =>
    ({ id }) as unknown as Parameters<typeof sameConnections>[0][number];

  it("treats a rebuilt list of the same connections as unchanged", () => {
    const a = connection("one");
    const b = connection("two");
    expect(sameConnections([a, b], [a, b])).toBe(true);
    // A fresh array, same members: this is the case that was re-routing audio.
    expect(sameConnections([a, b], [...[a, b]])).toBe(true);
  });

  it("notices a connection joining, leaving, or being replaced", () => {
    const a = connection("one");
    const b = connection("two");
    expect(sameConnections([a], [a, b])).toBe(false);
    expect(sameConnections([a, b], [a])).toBe(false);
    expect(sameConnections([a], [b])).toBe(false);
    expect(sameConnections([], [a])).toBe(false);
  });

  it("notices a reconnect that replaced the object behind the same id", () => {
    // Identity, not id: a relinked connection has a new player on the default
    // sink, and the choice has to be re-applied to it.
    expect(sameConnections([connection("one")], [connection("one")])).toBe(
      false,
    );
  });

  it("treats two empty lists as unchanged", () => {
    // The memo's initial value is `[]`, so this is the comparison made before
    // any connection exists; publishing there would apply a sink to nothing.
    expect(sameConnections([], [])).toBe(true);
  });
});
