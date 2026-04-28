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

// Parallel to PermissionRules: for each rule at the same kind/index in
// `combined_permissions`, the scopes that contribute it, in precedence order
// (highest first). Drives the scope-origin tooltip on combined rule rows.
export interface PermissionRuleOrigins {
  allow: Scope[][];
  deny: Scope[][];
  ask: Scope[][];
}

export type PermissionKind = "allow" | "deny" | "ask";

export interface LoadedScopes {
  project_dir: string;
  scopes: ScopeView[];
  combined_permissions: PermissionRules;
  combined_origins: PermissionRuleOrigins;
}

export interface MoveRequest {
  rule: string;
  kind: PermissionKind;
  from: Scope;
  to: Scope;
}

/**
 * Caller hints for `onMove` / `onMoveKey`. Drop handlers pass
 * `skipConfirm: true` because dragging onto a target column already
 * expresses intent — the diff/confirm modal is friction at that point.
 * Click-to-move keeps the modal as the safer default for less explicit
 * gestures. Recovery on accidental drops still falls back to the
 * per-write `.bak` files until an audit log / undo lands (#19).
 */
export interface MoveOptions {
  skipConfirm?: boolean;
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

/**
 * Launch-time override snapshot from the Rust side. Mirrors `RuntimeInfo`
 * in `src-tauri/src/runtime.rs`. Both fields are null when the app is
 * running unsandboxed; either being set means the user is in scratch mode
 * (#66) and the title bar should warn them.
 */
export interface RuntimeInfo {
  home_override: string | null;
  project_override: string | null;
}
