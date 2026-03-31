import {
  forwardRef,
  type CSSProperties,
  type HTMLAttributes,
  type ReactNode,
} from "react";
import type { TerminalPalette } from "blit-react";
import { layout, overlayChromeStyles, themeFor, uiScale } from "./theme";
import { t } from "./i18n";

export function OverlayBackdrop({
  palette,
  label,
  onClose,
  dismissOnBackdrop = true,
  children,
  style,
}: {
  palette: TerminalPalette;
  label: string;
  onClose?: () => void;
  dismissOnBackdrop?: boolean;
  children: ReactNode;
  style?: CSSProperties;
}) {
  const dark = palette.dark;
  const styles = overlayChromeStyles(themeFor(palette), dark);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={label}
      style={{
        ...layout.overlay,
        ...styles.overlay,
        ...style,
      }}
      onClick={dismissOnBackdrop ? onClose : undefined}
    >
      {children}
    </div>
  );
}

export const OverlayPanel = forwardRef<
  HTMLDivElement,
  HTMLAttributes<HTMLDivElement> & {
    palette: TerminalPalette;
    fontSize?: number;
  }
>(function OverlayPanel({ palette, style, onClick, fontSize, ...props }, ref) {
  const dark = palette.dark;
  const scale = uiScale(fontSize ?? 13);
  const styles = overlayChromeStyles(themeFor(palette), dark, scale);

  return (
    <div
      {...props}
      ref={ref}
      style={{
        ...layout.panel,
        ...styles.panel,
        fontSize: scale.md,
        ...style,
      }}
      onClick={(e) => {
        e.stopPropagation();
        onClick?.(e);
      }}
    />
  );
});

export function OverlayHeader({
  palette,
  title,
  subtitle,
  actions,
  onClose,
  closeLabel = t("overlay.close"),
  fontSize,
}: {
  palette: TerminalPalette;
  title: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  onClose?: () => void;
  closeLabel?: string;
  fontSize?: number;
}) {
  const dark = palette.dark;
  const scale = uiScale(fontSize ?? 13);
  const styles = overlayChromeStyles(themeFor(palette), dark, scale);

  return (
    <header style={styles.header}>
      <div style={styles.headerCopy}>
        <h2 style={styles.title}>{title}</h2>
        {subtitle && <p style={styles.subtitle}>{subtitle}</p>}
      </div>
      {(actions || onClose) && (
        <div style={styles.headerActions}>
          {actions}
          {onClose && (
            <button type="button" style={styles.closeButton} onClick={onClose}>
              {closeLabel}
            </button>
          )}
        </div>
      )}
    </header>
  );
}
