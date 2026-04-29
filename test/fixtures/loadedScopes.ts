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
  return {
    scope: overrides.scope,
    path: overrides.path ?? `/fake/${overrides.scope}/settings.json`,
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
 * Any scope not listed gets a present-but-empty view so the renderer's
 * "exists/no rules" branch is exercised by default. The combined panel
 * is derived from the per-scope `allow`/`deny`/`ask` lists (origin lists
 * point at the scopes that contributed each rule, in declaration order)
 * unless the caller overrides.
 */
export function buildLoadedScopes(overrides: LoadedScopesOverrides = {}): LoadedScopes {
  const overrideByScope = new Map(overrides.scopes?.map((s) => [s.scope, s]) ?? []);
  const scopes: ScopeView[] = SCOPES.map((scope) =>
    buildScopeView(overrideByScope.get(scope) ?? { scope, exists: false }),
  );

  const combined = emptyPermissions();
  const origins = emptyOrigins();
  const kinds: PermissionKind[] = ["allow", "deny", "ask"];
  for (const kind of kinds) {
    const seen = new Map<string, Scope[]>();
    for (const view of scopes) {
      for (const rule of view.permissions[kind]) {
        const list = seen.get(rule);
        if (list) list.push(view.scope);
        else seen.set(rule, [view.scope]);
      }
    }
    for (const [rule, scopesForRule] of seen) {
      combined[kind].push(rule);
      origins[kind].push(scopesForRule);
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
