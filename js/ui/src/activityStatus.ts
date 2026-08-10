import type { BlitActivity } from "@blit-sh/core";
import { t } from "./i18n";

export function activityPercent(activity: BlitActivity): number | null {
  const { completed, total } = activity;
  if (
    completed === undefined ||
    total === undefined ||
    !Number.isFinite(completed) ||
    !Number.isFinite(total) ||
    total <= 0
  )
    return null;
  return Math.max(0, Math.min(100, Math.round((completed / total) * 100)));
}

export function activityDescription(activity: BlitActivity): string {
  const key =
    activity.kind === "upload"
      ? "statusbar.activity.uploading"
      : activity.kind === "download"
        ? "statusbar.activity.downloading"
        : activity.kind === "sync"
          ? "statusbar.activity.syncing"
          : activity.kind === "search"
            ? "statusbar.activity.searching"
            : "statusbar.activity.working";
  return `${t(key)} ${activity.label}${
    activity.target ? ` › ${activity.target}` : ""
  }`;
}
