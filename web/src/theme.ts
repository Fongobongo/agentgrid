// Theme toggle: dark (default) <-> light, persisted in localStorage and
// applied as `data-theme` on <html> so CSS custom properties flip.

export type Theme = 'dark' | 'light';

const KEY = 'agentgrid-theme';

export function storedTheme(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    return v === 'light' ? 'light' : 'dark';
  } catch {
    return 'dark';
  }
}

export function applyTheme(theme: Theme) {
  if (theme === 'light') document.documentElement.setAttribute('data-theme', 'light');
  else document.documentElement.removeAttribute('data-theme');
}

export function setTheme(theme: Theme) {
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // Private mode: fall back to a session-only toggle.
  }
  applyTheme(theme);
}
