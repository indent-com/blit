const THEME_KEY = "blit-theme";
const COOKIE_MAX_AGE = 60 * 60 * 24 * 400; // ~13 months

function getTheme(): "light" | "dark" {
  return (
    (document.documentElement.getAttribute("data-theme") as "light" | "dark") ||
    "dark"
  );
}

function setThemeCookie(theme: string) {
  document.cookie = `${THEME_KEY}=${theme};path=/;max-age=${COOKIE_MAX_AGE};SameSite=Lax`;
}

export default function ThemeToggle() {
  const toggle = () => {
    const next = getTheme() === "dark" ? "light" : "dark";
    document.documentElement.setAttribute("data-theme", next);
    localStorage.setItem(THEME_KEY, next);
    setThemeCookie(next);
  };

  return (
    <button
      onClick={toggle}
      class="flex items-center justify-center bg-transparent border border-[var(--border)] text-[var(--dim)] w-7 h-7 rounded cursor-pointer hover:text-[var(--fg)] hover:border-[var(--dim)] transition-colors"
      aria-label="Toggle theme"
      title="Toggle theme"
    >
      {/* Sun icon — shown in dark mode (click to switch to light) */}
      <svg
        class="theme-icon-sun"
        width="16"
        height="16"
        viewBox="0 0 16 16"
        fill="none"
      >
        <circle
          cx="8"
          cy="8"
          r="3.5"
          stroke="currentColor"
          stroke-width="1.5"
        />
        <path
          d="M8 1v2M8 13v2M1 8h2M13 8h2M3.05 3.05l1.41 1.41M11.54 11.54l1.41 1.41M3.05 12.95l1.41-1.41M11.54 4.46l1.41-1.41"
          stroke="currentColor"
          stroke-width="1.5"
          stroke-linecap="round"
        />
      </svg>
      {/* Moon icon — shown in light mode (click to switch to dark) */}
      <svg
        class="theme-icon-moon"
        width="16"
        height="16"
        viewBox="0 0 16 16"
        fill="none"
      >
        <path
          d="M14 9.2A6 6 0 0 1 6.8 2 6 6 0 1 0 14 9.2Z"
          stroke="currentColor"
          stroke-width="1.5"
          stroke-linejoin="round"
        />
      </svg>
    </button>
  );
}
