import type { BlitContext, BlitHost } from "../../typescript/blit";

export type CheckStatus = "pass" | "warn" | "fail";

export interface DoctorCheck {
  readonly key: string;
  readonly status: CheckStatus;
  readonly label: string;
  readonly detail: string;
}

export interface DoctorCapability {
  readonly key: string;
  readonly label: string;
  readonly group: "service" | "protocol";
  readonly bit: number;
  readonly available: boolean;
}

export interface DoctorReport {
  readonly schema: "blit.doctor.v1";
  readonly status: "healthy" | "degraded";
  readonly server: {
    readonly protocolVersion: number;
    readonly version: string | null;
    readonly bootGeneration: string | null;
    readonly features: number;
    readonly featuresHex: string;
    readonly realtimeUnixNanos: string | null;
  };
  readonly extension: {
    readonly runtime: "quickjs";
    readonly name: string | null;
    readonly id: string;
    readonly revision: string;
    readonly attempt: string;
    readonly taskId: number;
    readonly moduleHash: string;
    readonly detached: boolean;
    readonly persistent: boolean;
    readonly enabled: boolean;
    readonly desiredRunning: boolean;
  };
  readonly checks: readonly DoctorCheck[];
  readonly capabilities: readonly DoctorCapability[];
  readonly summary: {
    readonly passed: number;
    readonly warnings: number;
    readonly failed: number;
    readonly availableCapabilities: number;
    readonly unavailableCapabilities: number;
  };
}

interface CapabilityDefinition {
  readonly key: string;
  readonly label: string;
  readonly group: "service" | "protocol";
  readonly bit: number;
}

const CAPABILITIES: readonly CapabilityDefinition[] = [
  { key: "create_nonce", label: "create nonces", group: "protocol", bit: 0 },
  { key: "restart", label: "terminal restart", group: "protocol", bit: 1 },
  { key: "resize_batch", label: "batched resize", group: "protocol", bit: 2 },
  { key: "copy_range", label: "copy ranges", group: "protocol", bit: 3 },
  { key: "compositor", label: "compositor", group: "service", bit: 4 },
  { key: "audio", label: "audio", group: "service", bit: 5 },
  { key: "files", label: "files", group: "service", bit: 6 },
  { key: "git", label: "git", group: "service", bit: 7 },
  { key: "lsp", label: "LSP", group: "service", bit: 8 },
  { key: "kv", label: "KV", group: "service", bit: 9 },
  { key: "network", label: "network relay", group: "service", bit: 10 },
  { key: "extensions", label: "extensions", group: "service", bit: 11 },
  { key: "channels", label: "native channels", group: "service", bit: 12 },
  { key: "processes", label: "processes", group: "service", bit: 13 },
  { key: "create_status", label: "create status", group: "protocol", bit: 14 },
  { key: "kill_mode", label: "process-group kill", group: "protocol", bit: 15 },
  { key: "pty_deadline", label: "PTY deadlines", group: "protocol", bit: 16 },
  { key: "scroll_by", label: "stable scrollback", group: "protocol", bit: 17 },
  { key: "surface_touch", label: "surface touch", group: "protocol", bit: 18 },
  {
    key: "surface_text_input",
    label: "surface text input",
    group: "protocol",
    bit: 19,
  },
  {
    key: "client_control",
    label: "client control",
    group: "protocol",
    bit: 20,
  },
  { key: "desktop", label: "desktop control", group: "service", bit: 21 },
  { key: "desktop_media", label: "desktop media", group: "service", bit: 22 },
  {
    key: "process_session_env",
    label: "session environment",
    group: "service",
    bit: 23,
  },
  {
    key: "environment",
    label: "server environment",
    group: "service",
    bit: 24,
  },
  { key: "app_socket", label: "app sockets", group: "service", bit: 25 },
  { key: "channel_watch", label: "channel watch", group: "protocol", bit: 26 },
  { key: "client_origin", label: "client origins", group: "protocol", bit: 27 },
  {
    key: "terminal_journal",
    label: "terminal journal",
    group: "service",
    bit: 28,
  },
  { key: "create_exec", label: "exact argv exec", group: "protocol", bit: 29 },
  {
    key: "create_no_subscribe",
    label: "unsubscribed create",
    group: "protocol",
    bit: 30,
  },
];

function check(
  checks: DoctorCheck[],
  key: string,
  status: CheckStatus,
  label: string,
  detail: string,
): void {
  checks.push({ key, status, label, detail });
}

function capabilityAvailable(features: number, bit: number): boolean {
  return ((features >>> bit) & 1) === 1;
}

function extensionLabel(context: BlitContext): string {
  return typeof context.name === "string" && context.name.length > 0
    ? `@${context.name}`
    : "unnamed extension";
}

export function inspectDoctor(
  host: Pick<
    BlitHost,
    "context" | "monotonicNow" | "random" | "realtimeNow" | "sleep"
  >,
): DoctorReport {
  const context = host.context;
  const checks: DoctorCheck[] = [];
  let realtimeUnixNanos: bigint | null = null;

  check(
    checks,
    "protocol",
    context.protocolVersion === 1 ? "pass" : "fail",
    "protocol",
    context.protocolVersion === 1
      ? "version 1"
      : `version ${context.protocolVersion}; this extension expects version 1`,
  );

  const commandFeatures = (1 << 11) | (1 << 12);
  check(
    checks,
    "command_transport",
    (context.features & commandFeatures) === commandFeatures ? "pass" : "fail",
    "command transport",
    "blit.cli.v1 invocation arrived over a native channel",
  );

  const named = typeof context.name === "string" && context.name.length > 0;
  const identityHealthy =
    named &&
    context.extensionId > 0n &&
    context.definitionRevision > 0n &&
    context.attempt > 0n &&
    /^[0-9a-f]{64}$/.test(context.moduleHash);
  check(
    checks,
    "identity",
    identityHealthy ? "pass" : "fail",
    "extension identity",
    identityHealthy
      ? `${extensionLabel(context)} revision ${context.definitionRevision}, attempt ${context.attempt}`
      : "name, IDs, or module digest are invalid",
  );

  const lifecycleHealthy =
    context.persistent && context.enabled && context.desiredRunning;
  check(
    checks,
    "lifecycle",
    lifecycleHealthy ? "pass" : "fail",
    "lifecycle",
    lifecycleHealthy
      ? "persistent, enabled, and desired-running"
      : `persistent=${context.persistent}, enabled=${context.enabled}, desired-running=${context.desiredRunning}`,
  );

  const hasServerVersion =
    typeof context.serverVersion === "string" &&
    context.serverVersion.length > 0;
  const hasBootGeneration = typeof context.bootGeneration === "bigint";
  check(
    checks,
    "server_identity",
    hasServerVersion && hasBootGeneration ? "pass" : "warn",
    "server identity",
    hasServerVersion && hasBootGeneration
      ? `Blit ${context.serverVersion}, boot ${context.bootGeneration}`
      : "server version or boot generation was not advertised",
  );

  try {
    const before = host.monotonicNow();
    realtimeUnixNanos = host.realtimeNow();
    host.sleep(1);
    const after = host.monotonicNow();
    const delta = after - before;
    const healthy = delta >= 0n && realtimeUnixNanos > 0n;
    check(
      checks,
      "clocks",
      healthy ? "pass" : "fail",
      "clocks",
      healthy
        ? `${(Number(delta) / 1_000_000).toFixed(3)} ms monotonic sleep; realtime available`
        : "monotonic or realtime clock returned an invalid value",
    );
  } catch (error) {
    check(checks, "clocks", "fail", "clocks", String(error));
  }

  try {
    const random = host.random(32);
    check(
      checks,
      "entropy",
      random.length === 32 ? "pass" : "fail",
      "entropy",
      `${random.length} of 32 requested bytes returned`,
    );
  } catch (error) {
    check(checks, "entropy", "fail", "entropy", String(error));
  }

  const features = context.features >>> 0;
  const capabilities = CAPABILITIES.map((definition) => ({
    ...definition,
    available: capabilityAvailable(features, definition.bit),
  }));
  const passed = checks.filter((item) => item.status === "pass").length;
  const warnings = checks.filter((item) => item.status === "warn").length;
  const failed = checks.filter((item) => item.status === "fail").length;
  const availableCapabilities = capabilities.filter(
    (item) => item.available,
  ).length;

  return {
    schema: "blit.doctor.v1",
    status: failed === 0 ? "healthy" : "degraded",
    server: {
      protocolVersion: context.protocolVersion,
      version: hasServerVersion ? context.serverVersion! : null,
      bootGeneration: hasBootGeneration
        ? context.bootGeneration!.toString()
        : null,
      features,
      featuresHex: `0x${features.toString(16).padStart(8, "0")}`,
      realtimeUnixNanos: realtimeUnixNanos?.toString() ?? null,
    },
    extension: {
      runtime: "quickjs",
      name: named ? context.name! : null,
      id: context.extensionId.toString(),
      revision: context.definitionRevision.toString(),
      attempt: context.attempt.toString(),
      taskId: context.taskId,
      moduleHash: context.moduleHash,
      detached: context.detached,
      persistent: context.persistent,
      enabled: context.enabled,
      desiredRunning: context.desiredRunning,
    },
    checks,
    capabilities,
    summary: {
      passed,
      warnings,
      failed,
      availableCapabilities,
      unavailableCapabilities: capabilities.length - availableCapabilities,
    },
  };
}

function capabilityLines(
  report: DoctorReport,
  group: DoctorCapability["group"],
): string[] {
  const capabilities = report.capabilities.filter(
    (item) => item.group === group,
  );
  const available = capabilities
    .filter((item) => item.available)
    .map((item) => item.label);
  const unavailable = capabilities
    .filter((item) => !item.available)
    .map((item) => item.label);
  return [
    ...wrapList("  ✓ available: ", available),
    ...wrapList("  ○ absent:    ", unavailable),
  ];
}

function wrapList(prefix: string, values: readonly string[]): string[] {
  if (values.length === 0) return [`${prefix}none`];
  const continuation = " ".repeat(prefix.length);
  const lines: string[] = [];
  let line = prefix;
  for (const value of values) {
    const separator = line === prefix ? "" : ", ";
    if (
      line.length > prefix.length &&
      line.length + separator.length + value.length > 88
    ) {
      lines.push(`${line},`);
      line = continuation + value;
    } else {
      line += separator + value;
    }
  }
  lines.push(line);
  return lines;
}

export function renderDoctor(report: DoctorReport): string {
  const extensionId = BigInt(report.extension.id)
    .toString(16)
    .padStart(16, "0");
  const checkLines = report.checks.map((item) => {
    const symbol =
      item.status === "pass" ? "✓" : item.status === "warn" ? "!" : "✗";
    return `  ${symbol} ${item.label}: ${item.detail}`;
  });

  return [
    "Blit doctor",
    "",
    "Server",
    `  protocol ${report.server.protocolVersion} · Blit ${report.server.version ?? "unreported"} · boot ${report.server.bootGeneration ?? "unreported"}`,
    `  features ${report.server.featuresHex}`,
    "",
    "Extension",
    `  @${report.extension.name ?? "unnamed"} · id ${extensionId} · revision ${report.extension.revision} · attempt ${report.extension.attempt}`,
    `  native QuickJS · task ${report.extension.taskId} · module ${report.extension.moduleHash.slice(0, 12)}…`,
    "",
    "Checks",
    ...checkLines,
    "",
    "Services",
    ...capabilityLines(report, "service"),
    "",
    "Protocol capabilities",
    ...capabilityLines(report, "protocol"),
    "",
    "Summary",
    `  ${report.status} — ${report.summary.passed} passed, ${report.summary.warnings} warnings, ${report.summary.failed} failed; ${report.summary.unavailableCapabilities} optional capabilities absent`,
    "",
  ].join("\n");
}
