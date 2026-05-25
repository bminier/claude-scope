import type {
  JsonValue,
  KindConflict,
  LoadedScopes,
  PathCollision,
  PermissionKind,
  PermissionRuleOrigins,
  PermissionRules,
  Preferences,
  Redundancy,
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
  /** Non-permission top-level keys, merged into the unified `values` map. */
  other_values?: { [key: string]: JsonValue };
  parse_error?: string | null;
}

export function emptyPermissions(): PermissionRules {
  return { allow: [], deny: [], ask: [] };
}

export function emptyOrigins(): PermissionRuleOrigins {
  return { allow: [], deny: [], ask: [] };
}

/**
 * Pull the permission rule snapshot out of a ScopeView's unified `values`
 * map. Mirrors the Rust `SettingsDoc::permissions` accessor (missing /
 * malformed shapes degrade to empty lists rather than throwing) so the
 * fixture's combined-panel walk and the renderer agree on what's there.
 */
export function permissionsOf(view: ScopeView): PermissionRules {
  const out = emptyPermissions();
  const perms = view.values.permissions;
  if (!perms || typeof perms !== "object" || Array.isArray(perms)) return out;
  const obj = perms as { [k: string]: JsonValue };
  for (const kind of ["allow", "deny", "ask"] as const) {
    const arr = obj[kind];
    if (!Array.isArray(arr)) continue;
    out[kind] = arr.filter((v): v is string => typeof v === "string");
  }
  return out;
}

export function buildScopeView(overrides: ScopeViewOverrides): ScopeView {
  const permissions = { ...emptyPermissions(), ...(overrides.permissions ?? {}) };
  // `path` is `string | null` on the wire — the Rust backend serializes
  // null when it can't resolve a scope path. `??` would collapse a
  // caller-supplied null into the default string, hiding that branch from
  // tests, so check for `undefined` explicitly.
  const path =
    overrides.path !== undefined ? overrides.path : `/fake/${overrides.scope}/settings.json`;
  // Compose `values` so `permissions` rides at the top of the on-disk key
  // order when the override supplies any rules — matches what the backend
  // emits for a real settings.json with `{"permissions": {...}, ...}`.
  const values: { [key: string]: JsonValue } = {};
  const hasPermissions =
    permissions.allow.length > 0 || permissions.deny.length > 0 || permissions.ask.length > 0;
  if (hasPermissions) {
    values.permissions = {
      allow: permissions.allow,
      deny: permissions.deny,
      ask: permissions.ask,
    };
  }
  if (overrides.other_values) {
    for (const [k, v] of Object.entries(overrides.other_values)) values[k] = v;
  }
  return {
    scope: overrides.scope,
    path,
    exists: overrides.exists ?? true,
    values,
    parse_error: overrides.parse_error ?? null,
  };
}

interface LoadedScopesOverrides {
  project_dir?: string;
  scopes?: ScopeViewOverrides[];
  combined_permissions?: Partial<PermissionRules>;
  combined_origins?: Partial<PermissionRuleOrigins>;
  path_collisions?: PathCollision[];
  kind_conflicts?: KindConflict[];
  redundancies?: Redundancy[];
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
      for (const rule of permissionsOf(view)[kind]) {
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
    path_collisions: overrides.path_collisions ?? [],
    kind_conflicts: overrides.kind_conflicts ?? [],
    redundancies: overrides.redundancies ?? [],
  };
}

export function buildPreferences(overrides: Partial<Preferences> = {}): Preferences {
  return {
    visible_scopes: overrides.visible_scopes ?? [...SCOPES],
    theme: overrides.theme ?? "auto",
    backup_on_write: overrides.backup_on_write ?? true,
    recent_projects: overrides.recent_projects ?? [],
    audit_log_rotate: overrides.audit_log_rotate ?? true,
    audit_log_max_size_mb: overrides.audit_log_max_size_mb ?? 10,
    // `??` instead of `||` so explicit `null` (= never group) is
    // honored — `null || 2` would silently flip to 2.
    group_rules_at: overrides.group_rules_at !== undefined ? overrides.group_rules_at : 2,
    // Default collapsed mirrors the Rust default (#155). Tests that
    // need the expanded view override explicitly.
    combined_panel_collapsed: overrides.combined_panel_collapsed ?? true,
  };
}

export function buildRuntimeInfo(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
  return {
    home_override: overrides.home_override ?? null,
    project_override: overrides.project_override ?? null,
  };
}
