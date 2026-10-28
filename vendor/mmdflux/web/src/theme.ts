export type ThemePreference = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";

interface ResolveThemeInput {
  preference: ThemePreference;
  systemPrefersDark: boolean;
}

interface ThemeControllerOptions {
  root: HTMLElement;
  storage?: Storage;
  matchMedia?: (query: string) => MediaQueryList;
}

interface ThemeController {
  apply: () => ResolvedTheme;
  getPreference: () => ThemePreference;
  setPreference: (preference: ThemePreference) => ResolvedTheme;
}

const THEME_STORAGE_KEY = "mmdflux-playground-theme";

export function resolveTheme(input: ResolveThemeInput): ResolvedTheme {
  if (input.preference === "light") {
    return "light";
  }
  if (input.preference === "dark") {
    return "dark";
  }
  return input.systemPrefersDark ? "dark" : "light";
}

function isThemePreference(value: string): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

function hasStorageApi(storage: Storage | undefined): storage is Storage {
  return (
    typeof storage === "object" &&
    storage !== null &&
    typeof storage.getItem === "function" &&
    typeof storage.setItem === "function"
  );
}

export function createThemeController(
  options: ThemeControllerOptions,
): ThemeController {
  const storage = hasStorageApi(options.storage) ? options.storage : undefined;
  const mediaQuery = options.matchMedia?.("(prefers-color-scheme: dark)");

  const storedPreference = storage?.getItem(THEME_STORAGE_KEY);
  let preference: ThemePreference =
    storedPreference && isThemePreference(storedPreference)
      ? storedPreference
      : "system";

  const apply = (): ResolvedTheme => {
    const resolved = resolveTheme({
      preference,
      systemPrefersDark: Boolean(mediaQuery?.matches),
    });
    options.root.dataset.theme = resolved;
    options.root.dataset.themePreference = preference;
    return resolved;
  };

  mediaQuery?.addEventListener("change", () => {
    if (preference === "system") {
      apply();
    }
  });

  return {
    apply,
    getPreference: () => preference,
    setPreference: (nextPreference: ThemePreference) => {
      preference = nextPreference;
      storage?.setItem(THEME_STORAGE_KEY, nextPreference);
      return apply();
    },
  };
}
