use serde::{Deserialize, Serialize};

/// Result of a successful [`Saf::pick_folder`] call.
///
/// `uri` is a `content://` tree URI whose permission has already been
/// persisted via `ContentResolver.takePersistableUriPermission` — the
/// caller can hold on to it across app launches. `display_name` is the
/// human-readable name of the chosen folder, when the OS returns one
/// (sometimes empty, depending on the storage provider).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedFolder {
    pub uri: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafEntry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafListResponse {
    pub entries: Vec<SafEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafReadResponse {
    pub base64: String,
}

/// Argument shape for mobile plugin invocations. camelCase on the wire
/// because Tauri's mobile bridge uses camelCase fields when deserializing
/// into `@InvokeArg` Kotlin classes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListArgs<'a> {
    pub tree_uri: &'a str,
    pub rel_dir: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadArgs<'a> {
    pub tree_uri: &'a str,
    pub rel: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteArgs<'a> {
    pub tree_uri: &'a str,
    pub rel: &'a str,
    pub base64: String,
    pub create_new: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelArgs<'a> {
    pub tree_uri: &'a str,
    pub rel: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameArgs<'a> {
    pub tree_uri: &'a str,
    pub from_rel: &'a str,
    pub to_rel: &'a str,
}

/// Empty success response type — Kotlin resolves with `{}` on these ops.
#[derive(Debug, Clone, Deserialize)]
pub struct SafEmpty {}

/// Result of a `statDocument` call. `size` is `-1` for directories or
/// when the entry doesn't exist; only meaningful when `exists` is true
/// and `is_dir` is false.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafStatResponse {
    pub exists: bool,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default = "default_size")]
    pub size: i64,
}

fn default_size() -> i64 {
    -1
}
