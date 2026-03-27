import {
  forwardRef,
  type CSSProperties,
  type HTMLAttributes,
  type ReactNode,
} from "react";
import { layout, overlayChromeStyles, themeFor } from "./theme";

export function OverlayBackdrop({
  dark,
  label,
  onClose,
  dismissOnBackdrop = true,
  children,
  style,
}: {
  dark: boolean;
  label: string;
  onClose?: () => void;
  dismissOnBackdrop?: boolean;
  children: ReactNode;
  style?: CSSProperties;
}) {
  const styles = overlayChromeStyles(themeFor(dark), dark);

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

export const OverlayPanel = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement> & {
  dark: boolean;
}>(
  function OverlayPanel({ dark, style, onClick, ...props }, ref) {
    const styles = overlayChromeStyles(themeFor(dark), dark);

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
  dark,
  title,
  subtitle,
  actions,
  onClose,
  closeLabel = "Esc",
}: {
  dark: boolean;
  title: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  onClose?: () => void;
  closeLabel?: string;
}) {
  const styles = overlayChromeStyles(themeFor(dark), dark);

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
