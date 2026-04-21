export type Scope = "local" | "project" | "user_local" | "user";

// UI-side scope order: broadest scope on the left, narrowest on the right.
// That's the opposite of precedence order, so this list intentionally
// diverges from Rust's `Scope::ALL` (which still iterates highest-precedence
// first to drive the effective-permissions union). The only keep-in-sync
// invariant is membership: every scope variant must appear here so column
// rendering and move-target buttons cover the full set.
export const SCOPES: readonly Scope[] = ["user", "user_local", "project", "local"] as const;

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export interface ScopeView {
  scope: Scope;
  path: string | null;
  exists: boolean;
  permissions: PermissionRules;
  // Non-permission top-level keys with their raw JSON values. Rust preserves
  // on-disk key order via serde_json's `preserve_order` feature; JS preserves
  // insertion order too for string keys, *except* it shuffles integer-like
  // keys ("0", "1", …) to the front. For Claude Code's settings shape
  // (env var names, hook event names, theme scalar) that's a non-issue —
  // none of the realistic top-level or nested keys are integer-like.
  other_values: { [key: string]: JsonValue };
  parse_error: string | null;
}

export interface PermissionRules {
  allow: string[];
  deny: string[];
  ask: string[];
}

export type PermissionKind = "allow" | "deny" | "ask";

export interface LoadedScopes {
  project_dir: string;
  scopes: ScopeView[];
  effective_permissions: PermissionRules;
}

export interface MoveRequest {
  rule: string;
  kind: PermissionKind;
  from: Scope;
  to: Scope;
}

export interface MoveSide {
  scope: Scope;
  path: string;
  path_exists: boolean;
  rules_before: string[];
  rules_after: string[];
  will_write: boolean;
  note: string | null;
}

export interface MovePreview {
  rule: string;
  kind: PermissionKind;
  from: MoveSide;
  to: MoveSide;
}

/**
 * DOM id of the global rule-filter input. Exported as a single source of
 * truth because both the render path (ui.ts sets it) and the keyboard
 * shortcut (main.ts looks it up) need to agree.
 */
export const SEARCH_INPUT_ID = "rule-search";
