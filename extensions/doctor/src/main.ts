import {
  type CommandDescriptor,
  type CommandResponse,
  serveCommands,
} from "../../typescript/command";
import { inspectDoctor, renderDoctor } from "./report";

const DESCRIPTOR: CommandDescriptor = {
  protocol: "blit.cli.v1",
  summary: "Check the server, extension runtime, and advertised capabilities",
  commands: [
    {
      path: [],
      summary: "Run server and QuickJS extension diagnostics",
      usage: "[--json]",
      options: [
        {
          names: ["--json"],
          takes_value: false,
          help: "emit one application/json result",
        },
      ],
    },
  ],
};

function invoke(args: readonly string[]): CommandResponse {
  const json = args.length === 1 && args[0] === "--json";
  if (args.length !== 0 && !json) {
    return {
      stderr: "Usage: blit @doctor [--json]\n",
      code: 2,
      detail: "unknown argument",
    };
  }

  const report = inspectDoctor(blit);
  if (json) {
    return {
      result: {
        contentType: "application/json",
        data: `${JSON.stringify(report, null, 2)}\n`,
      },
      code: report.status === "healthy" ? 0 : 1,
    };
  }
  return {
    stdout: renderDoctor(report),
    code: report.status === "healthy" ? 0 : 1,
  };
}

export default function main(): number {
  return serveCommands(DESCRIPTOR, ({ args }) => invoke(args));
}
