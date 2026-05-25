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
 *
 * `to_kind` (#8) reclassifies a permission rule into a different kind on
 * the destination side. Only valid when the path is a single permission
 * rule. With `from === to` it produces an in-place allow ↔ deny ↔ ask
 * change; with `from !== to` it crosses scopes and changes kind in one
 * gesture.
 */
export interface MoveLeafRequest {
  path: PathSeg[];
  from: Scope;
  to: Scope;
  to_kind?: PermissionKind;
}

/** Path-based delete request (#8). */
export interface DeleteLeafRequest {
  path: PathSeg[];
  from: Scope;
}

/** Path-based add request (#8). Powers paste. */
export interface AddLeafRequest {
  path: PathSeg[];
  to: Scope;
  value: JsonValue;
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
  /**
   * Override the active project root for this single move. The Move-to
   * submenu (#111) lets a user redirect a rule into a *different*
   * project's `local` or `project` scope; without this override the
   * dispatch would silently use `state.projectDir` and land the write
   * in the currently-open project's settings instead of the chosen
   * one. See codex 6th-pass [P1].
   */
  projectDir?: string;
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
  /** Echo of the request's `to_kind` (#8). The frontend uses this to label
   *  the diff modal "Reclassify rule" instead of "Move rule" and to collapse
   *  the bilateral display when `from.scope === to.scope`. */
  to_kind?: PermissionKind;
}

/** One-sided diff preview returned by `diff_delete_leaf` (#8). */
export interface DeleteLeafPreview {
  path: PathSeg[];
  kind: MoveLeafKind;
  from: MoveLeafSide;
}

/** One-sided diff preview returned by `diff_add_leaf` (#8). */
export interface AddLeafPreview {
  path: PathSeg[];
  kind: MoveLeafKind;
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
  /** Whether to drop a `.bak` next to every settings file on the first
   *  write per session (#88). Default `true` on a fresh install — older
   *  configs without the field also default to `true` server-side, so the
   *  frontend can read it as a plain `boolean` here. */
  backup_on_write: boolean;
  /** LRU of recently-opened project roots, most-recent first (#47). The
   *  backend dedupes, drops empties, and caps the list, so the frontend
   *  can render this list directly without re-normalizing. Defaults to
   *  empty for older configs. */
  recent_projects: string[];
  /** Whether to rotate `audit.jsonl` once it exceeds
   *  `audit_log_max_size_mb` (#127). Default `true` on older configs —
   *  auto-rotation is the safer baseline. */
  audit_log_rotate: boolean;
  /** Size threshold in MB at which `audit.jsonl` is rotated. Clamped
   *  server-side to `[1, 1000]`; out-of-range values fall back to the
   *  default rather than the nearest boundary. */
  audit_log_max_size_mb: number;
  /** Tool-grouping threshold (#115). `null` = never group; otherwise
   *  group when this many rules share a tool prefix. Backend coerces
   *  `Some(n < 2)` back to the default, so anything the frontend reads
   *  is either `null` or `>= 2`. */
  group_rules_at: number | null;
}

/** Bounds on `Preferences.audit_log_max_size_mb`. Mirrors Rust's
 *  `AUDIT_LOG_MAX_SIZE_MB_MIN` / `_MAX` / `_DEFAULT` constants — the
 *  settings UI uses these to populate the number input's min/max and
 *  to compute the default-restore action. Kept as a single object so a
 *  future schema bump is one site to update. */
export const AUDIT_LOG_MAX_SIZE_MB = {
  min: 1,
  max: 1000,
  default: 10,
} as const;

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

/**
 * One Claude project discovered on this machine (#106). Mirrors Rust's
 * `KnownProject`. `name` is the project root's basename; `root` is the
 * absolute path. Used to populate the per-project section of the Move-to
 * context-menu submenu.
 */
export interface KnownProject {
  name: string;
  root: string;
}

/**
 * Build- and runtime-time diagnostic block surfaced by the About dialog
 * (#21). Mirrors Rust's `AppInfo` in `src-tauri/src/app_info.rs`. Optional
 * fields are absent (not present-but-empty) when the build couldn't
 * capture them — the dialog formats those as "unknown" so a pasted
 * bug-report block has a stable shape.
 */
export interface AppInfo {
  version: string;
  git_sha: string | null;
  tauri_version: string;
  webview_version: string | null;
  rust_version: string;
  os: string;
  arch: string;
}

/**
 * Audit log entry kinds (#19). Mirrors Rust's `audit::Kind` — snake_case
 * wire strings pinned by `record_kind_serializes_to_snake_case_strings`
 * in audit.rs. `change_kind` is the same-scope reclassification flow;
 * `restore` is the meta-entry an undo / redo / restore-to-point writes
 * (#124), with its payload in [[AuditRecordView]]`.restore`.
 */
export type AuditKind = "move" | "change_kind" | "add" | "delete" | "restore";

/**
 * Direction of a `restore` audit entry. Mirrors Rust's
 * `audit::RestoreDirection`: `undo` / `redo` are cursor moves, `to_point`
 * is a restore-to-point (#125).
 */
export type AuditRestoreDirection = "undo" | "redo" | "to_point";

/**
 * Payload of a `restore` audit entry (#124). Mirrors Rust's
 * `audit::RestoreMeta`: the id of the entry acted on, the direction, and a
 * per-file before/after snapshot of everything the restore rewrote.
 */
export interface AuditRestoreMeta {
  target_id: string;
  direction: AuditRestoreDirection;
  files: AuditSide[];
}

/**
 * What shape of leaf the audit record's `path` targets. Mirrors Rust's
 * `audit::LeafKind`. The History view uses this with [[AuditKind]] to
 * compose human-readable labels like "Move permission rule" or
 * "Delete top-level key".
 */
export type AuditLeafKind = "top_level_key" | "permission_list" | "permission_rule";

/**
 * Who triggered an audit record. Mirrors Rust's `audit::Actor`. Orthogonal
 * to [[AuditKind]]: an undo run from the CLI is `actor: "cli"`,
 * `kind: "restore"` — there is deliberately no `restore` actor.
 */
export type AuditActor = "gui" | "cli" | "skill";

/**
 * One side of an audit record's write. Mirrors Rust's `audit::Side` —
 * scope, file path, and a snapshot of the affected top-level key both
 * before and after the op. `null` distinguishes "key absent / file
 * absent" from a literal JSON `null` (which arrives as `null` in the
 * `key_*` fields but the surrounding object is still present).
 */
export interface AuditSide {
  scope: Scope;
  file_path: string;
  top_level_key: string;
  key_before?: JsonValue | null;
  key_after?: JsonValue | null;
}

/**
 * One entry in the History view. Mirrors the wire shape from Rust's
 * `commands::AuditRecordView` — `audit::Record` fields flattened with a
 * derived `ts_ms` so the frontend doesn't need to parse Crockford base32
 * ULIDs to render timestamps.
 */
export interface AuditRecordView {
  id: string;
  kind: AuditKind;
  leaf_kind: AuditLeafKind;
  actor: AuditActor;
  project_dir?: string;
  from?: AuditSide;
  to?: AuditSide;
  path: PathSeg[];
  to_kind?: PermissionKind;
  /** Present only on `kind: "restore"` entries — the undo / redo /
   *  restore-to-point payload. */
  restore?: AuditRestoreMeta;
  claude_scope_version: string;
  ts_ms: number;
}

/**
 * One file's diff in a [[RestorePreview]]. Mirrors Rust's
 * `RestoreSidePreview`. `key_current` / `key_target` follow the
 * skip-on-absent convention: `undefined` means the key is unset, distinct
 * from a literal JSON `null`.
 */
export interface RestoreSidePreview {
  scope: Scope;
  file_path: string;
  file_path_exists: boolean;
  top_level_key: string;
  key_current?: JsonValue;
  key_target?: JsonValue;
  will_write: boolean;
  /** True when the on-disk value differs from what the audit log expected
   *  — the file was hand-edited since the logged op. The restore still
   *  proceeds on confirm; this drives a warning band in the modal. */
  state_mismatch: boolean;
}

/**
 * Diff preview for an undo / redo / restore-to-point (#124 / #125).
 * Returned by `audit_undo_preview` / `audit_redo_preview`. Mirrors Rust's
 * `RestorePreview`.
 */
export interface RestorePreview {
  direction: AuditRestoreDirection;
  /** The audit entry being undone / redone / restored-to. */
  target: AuditRecordView;
  sides: RestoreSidePreview[];
  /** How many ops the restore reverts: 1 for undo / redo. */
  ops_spanned: number;
  /**
   * ULID (Crockford base32) of the audit log's trailing record at preview
   * time. The apply call passes this back as `expected_tail_id` so the
   * backend can refuse if the log has changed since — closes the gap
   * `ops_spanned` alone leaves open (a same-length window after a
   * concurrent rotate+append). Populated for restore-to-point only;
   * absent for undo / redo. See #171.
   */
  tail_id?: string;
}

/**
 * Undo / redo availability for the topbar buttons (#124). Returned by
 * `audit_undo_status`. `undo` / `redo` carry the audit entry the next
 * click would act on; `sequence_break` is true when redo is unavailable
 * specifically because a change landed after the last undo.
 */
export interface UndoRedoStatus {
  undo?: AuditRecordView;
  redo?: AuditRecordView;
  sequence_break: boolean;
  /**
   * Count of unreadable audit-log lines (corrupt JSON, truncated tail,
   * schema drift the reader couldn't parse). When non-zero, the backend
   * withholds both `undo` and `redo` and the topbar should surface a
   * "log degraded" warning. Acting against a partial log can emit a
   * duplicate restore entry, so undo/redo stay disabled until the
   * issue is resolved. See #170.
   */
  skipped: number;
}

/**
 * Payload from `list_audit_records`. `skipped` is the count of malformed
 * lines the reader silently dropped (truncated tail, schema drift) so
 * the History view can surface a non-fatal footer warning rather than
 * acting as if the log were intact.
 */
export interface AuditLogPage {
  records: AuditRecordView[];
  skipped: number;
}
