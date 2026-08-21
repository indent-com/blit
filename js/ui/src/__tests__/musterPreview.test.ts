import { describe, expect, it } from "vitest";
import type {
  BlitSession,
  BlitSurface,
  BlitSurfaceOrigin,
} from "@blit-sh/core";
import {
  groupMusterPreviewResources,
  isMusterSession,
  musterAppIdForUnit,
  musterStackKey,
  musterSessionLabel,
  previewSessionsToWatch,
} from "../musterPreview";

function session(
  id: string,
  tag: string,
  connectionId = "local",
  state: BlitSession["state"] = "active",
): BlitSession {
  return {
    id,
    connectionId,
    ptyId: Number(id.replace(/\D/g, "")) || 1,
    tag,
    title: null,
    usedRows: 0,
    command: null,
    state,
    exitStatus: state === "exited" ? 0 : null,
  };
}

function surface(
  surfaceId: number,
  origin?: BlitSurfaceOrigin,
  connectionId = "local",
): BlitSurface {
  return {
    connectionId,
    surfaceId,
    parentId: 0,
    title: `surface ${surfaceId}`,
    appId: "self-reported",
    origin,
    width: 800,
    height: 600,
    logicalWidth: 800,
    logicalHeight: 600,
  };
}

const origin = (unit: string, sequence: string): BlitSurfaceOrigin => ({
  sandboxEngine: "wayland",
  appId: musterAppIdForUnit(unit),
  instanceId: sequence,
});

describe("Muster preview grouping", () => {
  it("uses the supervisor's stable UTF-8 app stamp", () => {
    expect(musterAppIdForUnit("api")).toBe("muster-e74fc019056aae07");
    expect(musterAppIdForUnit("epic/server")).toBe("muster-44865129361efa52");
    expect(musterAppIdForUnit("épée")).toBe("muster-3ae05f984f97964a");
  });

  it("recognizes every muster-prefixed terminal, including control runs", () => {
    const run = session("run", "muster/main/api/7");
    const control = session("stop", "muster/main/api/stop");
    expect(isMusterSession(run)).toBe(true);
    expect(isMusterSession(control)).toBe(true);
    expect(musterSessionLabel(run)).toBe("main/api/7");
    expect(isMusterSession(session("shell", "mustered/api/7"))).toBe(false);
  });

  it("stops watching Muster terminals while their block is collapsed", () => {
    const shell = session("shell", "shell");
    const api = session("api", "muster/api/7");
    const sessions = [shell, api];

    expect(previewSessionsToWatch(sessions, true)).toBe(sessions);
    expect(previewSessionsToWatch(sessions, false)).toEqual([shell]);
  });

  it("only watches terminals in expanded Muster stacks", () => {
    const shell = session("shell", "shell");
    const main = session("main", "muster/main/api/7");
    const other = session("other", "muster/other/api/8");
    const standalone = session("standalone", "muster/api/9");
    const sessions = [shell, main, other, standalone];

    expect(previewSessionsToWatch(sessions, true, new Set())).toEqual([shell]);
    expect(
      previewSessionsToWatch(
        sessions,
        true,
        new Set([musterStackKey("local", "main")]),
      ),
    ).toEqual([shell, main]);
  });

  it("moves owned terminals and stamped surfaces into bottom hierarchy groups", () => {
    const shell = session("shell1", "shell");
    const api = session("api2", "muster/api/7");
    const stop = session("stop3", "muster/api/stop");
    // This terminal is displayed, so it is absent from panelSessions. Its
    // parked surface must still be attributed beneath it.
    const worker = session("worker4", "muster/main/worker/3");

    const ordinary = surface(1);
    const apiWindow = surface(2, origin("api", "7"));
    const oldApiWindow = surface(3, origin("api", "6"));
    const workerWindow = surface(4, origin("main/worker", "3"));
    const remoteLookalike = surface(5, origin("api", "7"), "remote");

    const grouped = groupMusterPreviewResources(
      [shell, api, stop],
      [shell, api, stop, worker],
      [ordinary, apiWindow, oldApiWindow, workerWindow, remoteLookalike],
    );

    expect(grouped.sessions.map((item) => item.id)).toEqual(["shell1"]);
    expect(grouped.surfaces.map((item) => item.surfaceId)).toEqual([1, 3, 5]);
    expect(
      grouped.muster.map((instance) => ({
        connectionId: instance.connectionId,
        instance: instance.instance,
        units: instance.units.map((unit) => ({
          name: unit.name,
          runs: unit.runs.map((run) => ({
            id: run.session.id,
            label: run.label,
            showTerminal: run.showTerminal,
            surfaces: run.surfaces.map((item) => item.surfaceId),
          })),
        })),
      })),
    ).toEqual([
      {
        connectionId: "local",
        instance: null,
        units: [
          {
            name: "api",
            runs: [
              {
                id: "api2",
                label: "7",
                showTerminal: true,
                surfaces: [2],
              },
              {
                id: "stop3",
                label: "stop",
                showTerminal: true,
                surfaces: [],
              },
            ],
          },
        ],
      },
      {
        connectionId: "local",
        instance: "main",
        units: [
          {
            name: "worker",
            runs: [
              {
                id: "worker4",
                label: "3",
                showTerminal: false,
                surfaces: [4],
              },
            ],
          },
        ],
      },
    ]);
  });

  it("nests an instance run under its stack and unit", () => {
    const server = session("server4", "muster/blit/server/4");
    const grouped = groupMusterPreviewResources([server], [server], []);

    expect(grouped.muster).toEqual([
      {
        connectionId: "local",
        instance: "blit",
        units: [
          {
            name: "server",
            runs: [
              {
                session: server,
                label: "4",
                showTerminal: true,
                surfaces: [],
              },
            ],
          },
        ],
      },
    ]);
  });

  it("keeps equal instance and unit names on separate connections", () => {
    const local = session("local4", "muster/blit/server/4", "local");
    const prod = session("prod4", "muster/blit/server/4", "prod");
    const grouped = groupMusterPreviewResources(
      [local, prod],
      [local, prod],
      [],
    );

    expect(
      grouped.muster.map((instance) => ({
        connectionId: instance.connectionId,
        instance: instance.instance,
        runs: instance.units[0]?.runs.map((run) => run.session.id),
      })),
    ).toEqual([
      { connectionId: "local", instance: "blit", runs: ["local4"] },
      { connectionId: "prod", instance: "blit", runs: ["prod4"] },
    ]);
  });

  it("does not trust a surface's self-reported app id", () => {
    const api = session("api", "muster/api/1");
    const lookalike = {
      ...surface(9),
      appId: musterAppIdForUnit("api"),
    };
    const grouped = groupMusterPreviewResources([api], [api], [lookalike]);
    expect(grouped.muster[0]?.units[0]?.runs[0]?.surfaces).toEqual([]);
    expect(grouped.surfaces).toEqual([lookalike]);
  });
});
