use serde::{Deserialize, Serialize};

/// Result of a successful folder pick.
///
/// `path` is the resolved `file://`-style absolute filesystem path the
/// user selected — usable directly with `std::fs` while a security-scoped
/// resource session is active on it.
///
/// `bookmark` is base64-encoded `Data` produced by
/// `URL.bookmarkData(options: .withSecurityScope, ...)`. Persist it across
/// launches and re-hydrate it via `resume_access` on next cold start —
/// iOS won't honor the raw `path` after the initial scope is dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedFolder {
    pub path: String,
    #[serde(default)]
    pub display_name: String,
    pub bookmark: String,
}

/// Resumed scoped-access response. `path` may differ from the original
/// pick if the user moved the folder via Files.app — the bookmark
/// resolves to the new location and `bookmark` carries a freshly minted
/// blob the caller should persist (overwriting the previous one).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumedAccess {
    pub path: String,
    pub bookmark: String,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeArgs<'a> {
    pub bookmark: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseArgs<'a> {
    pub path: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PickerEmpty {}
