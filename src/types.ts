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

/**
 * One segment of a JSON path. Mirrors Rust's `PathSeg` (untagged
 * `String`/`usize`): object keys ride as strings, array indices ride as
 * numbers. The unified `move_leaf` IPC carries paths in this shape.
 */
export type PathSeg = string | number;

export interface ScopeView {
  scope: Scope;
  path: string | null;
  exists: boolean;
  // Every top-level key on disk in source order, including `permissions`.
  // Rust preserves on-disk key order via serde_json's `preserve_order`
  // feature; JS preserves insertion order too for string keys, *except* it
  // shuffles integer-like keys ("0", "1", …) to the front. Realistic Claude
  // Code settings keys are non-integer-like, so this is a non-issue.
  values: { [key: string]: JsonValue };
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

/**
 * Path-based move request. Replaces the per-rule `MoveRequest` and per-key
 * `MoveKeyRequest` shapes with a single primitive: any movable JSON path
 * (whole top-level key, whole `permissions.<kind>` array, or a single rule
 * under `permissions.<kind>`) plus source / destination scopes.
 */
export interface MoveLeafRequest {
  path: PathSeg[];
  from: Scope;
  to: Scope;
}

/**
 * Caller hints for `onMoveLeaf`. Drop handlers pass `skipConfirm: true`
 * because dragging onto a target column already expresses intent — the
 * diff/confirm modal is friction at that point. Click-to-move keeps the
 * modal as the safer default for less explicit gestures. Recovery on
 * accidental drops still falls back to the per-write `.bak` files until an
 * audit log / undo lands (#19).
 */
export interface MoveOptions {
  skipConfirm?: boolean;
}

/**
 * What kind of movable path the preview describes. Mirrors Rust's
 * `MoveLeafKind` (snake_case on the wire) so the diff modal can branch on a
 * compact discriminator instead of re-classifying the path itself.
 */
export type MoveLeafKind = "top_level_key" | "permission_list" | "permission_rule";

export interface MoveLeafSide {
  scope: Scope;
  file_path: string;
  file_path_exists: boolean;
  // The Rust side omits these fields entirely when the affected top-level
  // key is absent (see `#[serde(skip_serializing_if = "Option::is_none")]`),
  // so `undefined` means "key absent" while `null` is a real JSON null
  // value. The diff modal keys off that distinction when rendering.
  key_before?: JsonValue;
  key_after?: JsonValue;
  will_write: boolean;
  note: string | null;
}

export interface MoveLeafPreview {
  path: PathSeg[];
  kind: MoveLeafKind;
  from: MoveLeafSide;
  to: MoveLeafSide;
}

/**
 * DOM id of the global rule-filter input. Exported as a single source of
 * truth because both the render path (ui.ts sets it) and the keyboard
 * shortcut (main.ts looks it up) need to agree.
 */
export const SEARCH_INPUT_ID = "rule-search";

/**
 * Color theme override. `auto` defers to the OS `prefers-color-scheme`
 * value at runtime; `light` / `dark` pin the palette regardless of OS
 * preference. Keep the variants lowercase — the Rust side serializes via
 * `serde(rename_all = "lowercase")` and these strings cross the IPC verbatim.
 */
export type Theme = "auto" | "light" | "dark";

/**
 * User preferences persisted to the OS config dir. Schema mirrors the Rust
 * `Preferences` struct; the backend fills in defaults for missing fields,
 * so this is always complete as read from the IPC.
 */
export interface Preferences {
  visible_scopes: Scope[];
  theme: Theme;
}

/**
 * Launch-time override snapshot from the Rust side. Mirrors `RuntimeInfo`
 * in `src-tauri/src/runtime.rs`. Both fields are null when the app is
 * running unsandboxed; either being set means the user is in scratch mode
 * (#66) and the UI should show the persistent warning banner under the
 * toolbar.
 */
export interface RuntimeInfo {
  home_override: string | null;
  project_override: string | null;
}
