export type Scope = "local" | "project" | "user";

export const SCOPES: readonly Scope[] = ["local", "project", "user"] as const;

export interface ScopeView {
  scope: Scope;
  path: string | null;
  exists: boolean;
  permissions: PermissionRules;
  other_keys: string[];
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
