export type Scope = "local" | "project" | "user_local" | "user";

// UI-side scope order: broadest scope on the left, narrowest on the right.
// That's the opposite of precedence order, so this list intentionally
// diverges from Rust's `Scope::ALL` (which still iterates highest-precedence
// first to drive the combined-permissions union). The only keep-in-sync
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
  combined_permissions: PermissionRules;
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

export interface MoveKeyRequest {
  key: string;
  from: Scope;
  to: Scope;
}

export interface MoveKeySide {
  scope: Scope;
  path: string;
  path_exists: boolean;
  // The Rust side omits `value_before` / `value_after` entirely when the key
  // is absent (see `#[serde(skip_serializing_if = "Option::is_none")]`), so
  // `undefined` means "absent" while `null` is a real JSON null value. The
  // UI relies on that distinction when rendering verdicts and values.
  value_before?: JsonValue;
  value_after?: JsonValue;
  will_write: boolean;
  note: string | null;
}

export interface MoveKeyPreview {
  key: string;
  from: MoveKeySide;
  to: MoveKeySide;
}

/**
 * DOM id of the global rule-filter input. Exported as a single source of
 * truth because both the render path (ui.ts sets it) and the keyboard
 * shortcut (main.ts looks it up) need to agree.
 */
export const SEARCH_INPUT_ID = "rule-search";

/**
 * User preferences persisted to the OS config dir. Schema mirrors the Rust
 * `Preferences` struct; the backend fills in defaults for missing fields,
 * so this is always complete as read from the IPC.
 */
export interface Preferences {
  visible_scopes: Scope[];
}
