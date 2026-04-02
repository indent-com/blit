export default function StatusOverlay(props: {
  status: string;
  isError?: boolean;
}) {
  return (
    <div
      class={`absolute inset-0 z-50 flex items-center justify-center bg-[var(--bg)] font-mono text-sm ${
        props.isError ? "text-red-500" : "text-[var(--dim)]"
      }`}
    >
      {props.status}
    </div>
  );
}
