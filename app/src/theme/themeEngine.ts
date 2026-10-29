// ── Theme Engine ────────────────────────────────────────────────────
// Core types and runtime utilities for the custom theme system.

export interface ThemeColorPalette {
  bg: string;
  surface: string;
  primary: string;
  secondary: string;
  text: string;
  subtext: string;
  border: string;
  hover: string;
}

export interface CustomTheme {
  id: string;
  name: string;
  isDark: boolean;
  palette: ThemeColorPalette;
  isBuiltin?: boolean;
}

const STYLE_ID = 'dynamic-theme';

/** Choose the higher-contrast foreground for palette colors emitted by color inputs. */
export function primaryForeground(primary: string): '#111111' | '#ffffff' {
  const match = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(primary.trim());
  if (!match) return '#ffffff'; // Preserve prior behavior for externally imported CSS colors.
  const hex = match[1].length === 3 ? [...match[1]].map(c => c + c).join('') : match[1];
  const channels = [0, 2, 4].map(i => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map(c => c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  const luminance = channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
  const darkLuminance = 0.005605391624202723; // #111111
  return (luminance + 0.05) / (darkLuminance + 0.05) >= 1.05 / (luminance + 0.05)
    ? '#111111' : '#ffffff';
}

/**
 * Inject a `<style>` block that overrides the @theme CSS variables,
 * and toggle the .dark/.light class on <html>.
 */
export function applyTheme(theme: CustomTheme): void {
  const root = document.documentElement;

  // Toggle dark/light class
  if (theme.isDark) {
    root.classList.add('dark');
    root.classList.remove('light');
  } else {
    root.classList.add('light');
    root.classList.remove('dark');
  }

  // Build CSS variable overrides
  const p = theme.palette;
  const css = `:root, :root.light, :root.dark {
  --color-telegram-bg: ${p.bg};
  --color-telegram-surface: ${p.surface};
  --color-telegram-primary: ${p.primary};
  --tr-on-primary: ${primaryForeground(p.primary)};
  --color-telegram-secondary: ${p.secondary};
  --color-telegram-text: ${p.text};
  --color-telegram-subtext: ${p.subtext};
  --color-telegram-border: ${p.border};
  --color-telegram-hover: ${p.hover};
  --color-telegram-glass-bg: ${theme.isDark ? p.surface : '#ffffff'};
  --color-telegram-glass-border: ${theme.isDark ? '#ffffff' : '#000000'};
}`;

  // Replace or create the style element
  let el = document.getElementById(STYLE_ID) as HTMLStyleElement | null;
  if (!el) {
    el = document.createElement('style');
    el.id = STYLE_ID;
    document.head.appendChild(el);
  }
  el.textContent = css;
}

/**
 * Remove the injected style block so the base @theme values take effect.
 */
export function removeCustomTheme(): void {
  const el = document.getElementById(STYLE_ID);
  if (el) el.remove();
}

/** Generate a unique ID for user-created themes. */
export function generateThemeId(): string {
  return `custom-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}
