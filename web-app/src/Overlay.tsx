import {
  forwardRef,
  type CSSProperties,
  type HTMLAttributes,
  type ReactNode,
} from "react";
import type { TerminalPalette } from "blit-react";
import { layout, overlayChromeStyles, themeFor } from "./theme";

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
  }
>(
  function OverlayPanel({ palette, style, onClick, ...props }, ref) {
    const dark = palette.dark;
    const styles = overlayChromeStyles(themeFor(palette), dark);

    return (
      <div
        {...props}
        ref={ref}
        style={{
          ...layout.panel,
          ...styles.panel,
          ...style,
        }}
        onClick={(e) => {
          e.stopPropagation();
          onClick?.(e);
        }}
      />
    );
  },
);

export function OverlayHeader({
  palette,
  title,
  subtitle,
  actions,
  onClose,
  closeLabel = "Esc",
}: {
  palette: TerminalPalette;
  title: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  onClose?: () => void;
  closeLabel?: string;
}) {
  const dark = palette.dark;
  const styles = overlayChromeStyles(themeFor(palette), dark);

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
