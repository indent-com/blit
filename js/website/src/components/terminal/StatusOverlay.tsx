export default function StatusOverlay(props: {
  status: string;
  isError?: boolean;
}) {
  if (props.isError) {
    return (
      <div class="absolute inset-0 z-50 flex items-center justify-center bg-[var(--bg)] font-mono text-sm text-red-500">
        {props.status}
      </div>
    );
  }

  // Skeleton line widths to mimic terminal output
  const lines = [
    "w-[60%]",
    "w-[80%]",
    "w-[45%]",
    "w-[70%]",
    "w-[35%]",
    "w-[55%]",
  ];

  return (
    <div class="absolute inset-0 z-50 flex flex-col bg-[var(--bg)]">
      {/* Fake tab bar */}
      <div class="flex h-9 min-h-9 shrink-0 items-center border-b border-[var(--border)] bg-[var(--surface)] px-3 gap-2">
        <div class="h-4 w-36 animate-pulse rounded bg-[var(--border)]" />
        <div class="h-4 w-4 animate-pulse rounded bg-[var(--border)] opacity-50" />
      </div>

      {/* Fake terminal body */}
      <div class="relative flex-1 p-4">
        {/* Skeleton lines */}
        <div class="flex flex-col gap-3 pt-2">
          {lines.map((w) => (
            <div
              class={`h-3 animate-pulse rounded bg-[var(--border)]/40 ${w}`}
            />
          ))}
        </div>

        {/* Centered status text */}
        <div class="absolute inset-0 flex items-center justify-center">
          <span class="font-mono text-sm text-[var(--dim)]">
            {props.status}
          </span>
        </div>
      </div>
    </div>
  );
}
