import type {
  JsonValue,
  LoadedScopes,
  PermissionKind,
  PermissionRuleOrigins,
  PermissionRules,
  Preferences,
  RuntimeInfo,
  Scope,
  ScopeView,
} from "../../src/types.ts";
import { SCOPES } from "../../src/types.ts";

/**
 * Tiny fixture builders for the renderer tests. Mirrors the shape the Rust
 * backend returns from `load_scopes` / `load_preferences` / `load_runtime_info`
 * — built TS-side so unit tests stay fast and don't touch disk. Disk-backed
 * `.claude/` fixtures (E2E) are out of scope for the first unit layer.
 *
 * All builders return fresh objects so tests can mutate the result without
 * affecting other tests.
 */

interface ScopeViewOverrides {
  scope: Scope;
  path?: string | null;
  exists?: boolean;
  permissions?: Partial<PermissionRules>;
  other_values?: { [key: string]: JsonValue };
  parse_error?: string | null;
}

export function emptyPermissions(): PermissionRules {
  return { allow: [], deny: [], ask: [] };
}

export function emptyOrigins(): PermissionRuleOrigins {
  return { allow: [], deny: [], ask: [] };
}

export function buildScopeView(overrides: ScopeViewOverrides): ScopeView {
  const permissions = { ...emptyPermissions(), ...(overrides.permissions ?? {}) };
  // `path` is `string | null` on the wire — the Rust backend serializes
  // null when it can't resolve a scope path. `??` would collapse a
  // caller-supplied null into the default string, hiding that branch from
  // tests, so check for `undefined` explicitly.
  const path =
    overrides.path !== undefined ? overrides.path : `/fake/${overrides.scope}/settings.json`;
  return {
    scope: overrides.scope,
    path,
    exists: overrides.exists ?? true,
    permissions,
    other_values: overrides.other_values ?? {},
    parse_error: overrides.parse_error ?? null,
  };
}

interface LoadedScopesOverrides {
  project_dir?: string;
  scopes?: ScopeViewOverrides[];
  combined_permissions?: Partial<PermissionRules>;
  combined_origins?: Partial<PermissionRuleOrigins>;
}

/**
 * Builds a `LoadedScopes` payload for a given set of per-scope overrides.
 * Any scope not listed gets an `exists: false` view so the renderer's
 * "(file not present)" branch is exercised by default — that's the
 * realistic baseline for a project where most scope files don't exist.
 * The combined panel is derived from the per-scope `allow`/`deny`/`ask`
 * lists, mirroring the Rust backend in two ways: contributors are
 * accumulated in precedence order (highest first — Local→Project→
 * UserLocal→User), and a rule repeated within a single scope only
 * counts that scope once. Caller can override either field directly.
 */
export function buildLoadedScopes(overrides: LoadedScopesOverrides = {}): LoadedScopes {
  const overrideByScope = new Map(overrides.scopes?.map((s) => [s.scope, s]) ?? []);
  const scopes: ScopeView[] = SCOPES.map((scope) =>
    buildScopeView(overrideByScope.get(scope) ?? { scope, exists: false }),
  );

  // SCOPES is UI order (broadest first); the backend iterates precedence
  // order (highest first), which is the reverse. Walk that order so the
  // origin lists match what the real `combined_permissions()` produces
  // and what the scope-origin tooltip claims to show.
  const precedenceOrder = [...scopes].reverse();
  const combined = emptyPermissions();
  const origins = emptyOrigins();
  const kinds: PermissionKind[] = ["allow", "deny", "ask"];
  for (const kind of kinds) {
    for (const view of precedenceOrder) {
      for (const rule of view.permissions[kind]) {
        const idx = combined[kind].indexOf(rule);
        if (idx === -1) {
          combined[kind].push(rule);
          origins[kind].push([view.scope]);
        } else if (!origins[kind][idx].includes(view.scope)) {
          // Same rule listed twice inside one scope file is legal JSON
          // (and easy to produce by hand-editing); the real backend
          // dedupes the per-rule contributor list, so the fixture must
          // too — otherwise the tooltip-order test would render
          // "Local, Local" where the app shows "Local".
          origins[kind][idx].push(view.scope);
        }
      }
    }
  }

  return {
    project_dir: overrides.project_dir ?? "/fake/project",
    scopes,
    combined_permissions: { ...combined, ...(overrides.combined_permissions ?? {}) },
    combined_origins: { ...origins, ...(overrides.combined_origins ?? {}) },
  };
}

export function buildPreferences(overrides: Partial<Preferences> = {}): Preferences {
  return {
    visible_scopes: overrides.visible_scopes ?? [...SCOPES],
  };
}

export function buildRuntimeInfo(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
  return {
    home_override: overrides.home_override ?? null,
    project_override: overrides.project_override ?? null,
  };
}
