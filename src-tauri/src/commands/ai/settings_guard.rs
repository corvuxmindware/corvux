//! The single choke point for `save_settings`: decide, from the CURRENT
//! settings and the renderer's INCOMING struct, (a) what the merged result is,
//! (b) what the save would escalate, and (c) what happens to each provider key.
//!
//! Pure on purpose — no I/O, no Tauri — so the policy is unit-testable and the
//! command in `settings.rs` is thin glue (plan → native confirm → apply).
//!
//! Sharp-edges audit 2026-11-11:
//! * **F1** one IPC call used to disarm the whole agent approval gate;
//! * **F2** `ollama_base_url` was an unvalidated destination for note content;
//! * **F11** `get_settings` handed live provider keys to the webview, so the
//!   save path now treats a key field of `None` as "unchanged", never "delete".
//!
//! One mechanism generates all three: the renderer is trusted to decide
//! security-bearing settings. The fix is not N per-field patches but one place
//! that classifies a change as an *escalation* and requires an answer the
//! renderer cannot forge (`commands::native_confirm`).

use crate::sensitive::SecretString;

use super::provider_url::{self, HostClass};
use super::types::Settings;

/// A change that makes the agent or the app more permissive, or sends the
/// user's content somewhere new. Each needs an out-of-band confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Escalation {
    /// `agent_autonomy.auto_approve_writes` false → true.
    AutoApproveWrites,
    /// A tool name newly added to `agent_autonomy.auto_approve_tools`.
    AutoApproveTool(String),
    /// A path pattern removed from `agent_autonomy.agent_blocked_paths`.
    UnblockPath(String),
    /// `ollama_base_url` now names a host on the open internet (over https).
    ProviderHost(String),
    /// `ollama_base_url` now names a host on the open internet over plain
    /// `http://` — a split-DNS home lab or an exfil channel; only the user
    /// can tell which, and the traffic is unencrypted either way.
    ProviderHostPlaintext(String),
    /// `agent_autonomy.auto_approve_reads` false → true: someone who chose to
    /// review every read loses that choice.
    AutoApproveReads,
    /// A provider API key is being saved or replaced (R3). Every FUTURE chat
    /// is sent to that provider under this key's account — a key chosen by
    /// someone else is a persistent exfil channel that outlives the attack
    /// (the CSP trim closed the webview's direct POST; this closes the
    /// "future chats" half).
    ProviderKey(&'static str),
    /// A saved provider API key is being REMOVED (review AK-8). Not a loosening
    /// — an availability attack: a script could silently wipe every key and
    /// leave chat dead. Asked in the same native dialog as every other key
    /// change. A token dropped because the server address changed is NOT this
    /// (the user did not ask to remove it; the address change is what is asked).
    ProviderKeyRemoval(&'static str),
    /// `ollama_base_url` now names a link-local address (`169.254.0.0/16`,
    /// `fe80::/10`): the cloud-metadata range, or a device with no router. The
    /// well-known metadata addresses themselves are refused before this point.
    ProviderHostLinkLocal(String),
    /// `support_endpoint` — where "Report a problem" sends the user (and their
    /// pasted diagnostic report) — now points somewhere other than the built-in
    /// default. An ungated setter is a standing phishing page (review AK-8).
    SupportEndpoint(String),
}

/// Upper bound on escalations in ONE save. The native dialog lists every one of
/// them, so a save that needs more is REFUSED rather than shortened: a dialog
/// that says "…and 3 more" lets a script pad the list with harmless lines and
/// hide the dangerous ones behind the fold (reviewer finding #6).
pub(crate) const MAX_ESCALATIONS_PER_SAVE: usize = 8;

impl Escalation {
    /// One line for the native dialog. Renderer-influenced text is sanitized.
    pub(crate) fn describe(&self) -> String {
        use crate::commands::native_confirm::dialog_safe;
        match self {
            Self::AutoApproveWrites => {
                "Let the AI agent edit, create and delete notes without asking you first".into()
            }
            Self::AutoApproveTool(t) => {
                format!(
                    "Always allow the agent tool \"{}\" without asking",
                    dialog_safe(t, 60)
                )
            }
            Self::UnblockPath(p) => format!(
                "Let the agent write to \"{}\" (currently off-limits)",
                dialog_safe(p, 80)
            ),
            Self::ProviderHost(h) => format!(
                "Send your chats (including note excerpts) to the internet host \"{}\"",
                dialog_safe(h, 80)
            ),
            Self::ProviderHostPlaintext(h) => format!(
                "Send your chats (including note excerpts) to \"{}\" over plain http://, \
                 UNENCRYPTED, on a name that looks like the open internet",
                dialog_safe(h, 80)
            ),
            Self::AutoApproveReads => {
                "Let the AI agent read your notes without asking you first".into()
            }
            Self::ProviderKey(p) => format!(
                "Save an API key for {p} — every future chat is sent under this key's account",
            ),
            Self::ProviderKeyRemoval(p) => format!(
                "Remove the saved API key for {p} — chat with {p} stops working until a key is added again",
            ),
            Self::ProviderHostLinkLocal(h) => format!(
                "Send your chats (including note excerpts) to the link-local address \"{}\" \
                 — an address range cloud services use for internal credentials, and that a \
                 device with no router uses for itself",
                dialog_safe(h, 80)
            ),
            Self::SupportEndpoint(e) => format!(
                "Send problem reports (your description plus diagnostic details) to \"{}\"",
                dialog_safe(e, 120)
            ),
        }
    }

    /// Stable short label for the audit log.
    pub(crate) fn audit_tool(&self) -> &'static str {
        match self {
            Self::AutoApproveWrites
            | Self::AutoApproveTool(_)
            | Self::UnblockPath(_)
            | Self::AutoApproveReads => "settings.agent_autonomy",
            Self::ProviderHost(_)
            | Self::ProviderHostPlaintext(_)
            | Self::ProviderHostLinkLocal(_) => "settings.ollama_base_url",
            Self::ProviderKey(_) | Self::ProviderKeyRemoval(_) => "settings.provider_key",
            Self::SupportEndpoint(_) => "settings.support_endpoint",
        }
    }

    /// True for the routine "save / replace / remove my API key" changes. The
    /// dialog for a save made up only of these is worded as an account change,
    /// not as "loosening a safety setting" (review AK-5): every key save asks
    /// now, so the alarming wording read as an attack on first run.
    pub(crate) fn is_key_change(&self) -> bool {
        matches!(self, Self::ProviderKey(_) | Self::ProviderKeyRemoval(_))
    }
}

/// What to do with one provider key slot when persisting.
#[derive(Debug, Clone)]
pub(crate) enum KeyUpdate {
    /// Incoming `None` — the renderer never held the key, so it cannot have
    /// meant "delete it". Leave keychain and memory alone.
    Keep,
    /// A new non-empty key.
    Set(SecretString),
    /// Incoming empty string — the explicit "remove this key" gesture.
    Clear,
}

/// Result of planning a save.
pub(crate) struct SettingsPlan {
    /// The settings to persist and install in memory. Provider-key fields hold
    /// the FINAL values (kept, set, or cleared) so the caller needs no further
    /// merge; the on-disk form is derived by `settings::disk_form`.
    pub merged: Settings,
    /// Per-provider key handling, in the order
    /// gemini, openrouter, ollama, openai, anthropic.
    pub key_updates: [KeyUpdate; 5],
    /// Empty = the save changes nothing security-bearing.
    pub escalations: Vec<Escalation>,
}

fn plan_key(
    incoming: &Option<SecretString>,
    current: &Option<SecretString>,
) -> (KeyUpdate, Option<SecretString>) {
    match incoming {
        None => (KeyUpdate::Keep, current.clone()),
        Some(k) => {
            let trimmed = k.trim();
            if trimmed.is_empty() {
                (KeyUpdate::Clear, None)
            } else {
                let key = SecretString::from(trimmed);
                (KeyUpdate::Set(key.clone()), Some(key))
            }
        }
    }
}

/// Plan a `save_settings` call. `Err` means the incoming struct is invalid and
/// must be rejected outright (nothing to confirm, nothing to write).
pub(crate) fn plan_settings_update(
    current: &Settings,
    incoming: Settings,
) -> Result<SettingsPlan, String> {
    let mut merged = incoming;
    // Pushed most-serious first, deduplicated (see `push_unique`).
    let mut escalations: Vec<Escalation> = Vec::new();

    let cur = &current.agent_autonomy;
    let inc = &merged.agent_autonomy;
    if !cur.auto_approve_writes && inc.auto_approve_writes {
        push_unique(Escalation::AutoApproveWrites, &mut escalations);
    }
    for pat in &cur.agent_blocked_paths {
        if !inc.agent_blocked_paths.contains(pat) {
            push_unique(Escalation::UnblockPath(pat.clone()), &mut escalations);
        }
    }

    // ── F2: the provider base URL ───────────────────────────────────────
    // Validate only when it CHANGES. An install that already holds an
    // unusual value keeps working until the user edits it (migration-safe,
    // SYN-I10 spirit); `build_chat_provider` still enforces the shape on every
    // chat, with a message that says what to fix.
    let mut host_changed = false;
    if merged.ollama_base_url.trim() != current.ollama_base_url.trim() {
        // The CHOSEN value: no `user:pass@` (AK-9 keeps a legacy SAVED one
        // working, but nothing new may be saved in that shape).
        let v = provider_url::validate_new_provider_base_url(&merged.ollama_base_url)
            .map_err(|e| format!("The Ollama server address {e}"))?;
        if v.is_plaintext_to_internet() {
            push_unique(
                Escalation::ProviderHostPlaintext(v.host.clone()),
                &mut escalations,
            );
        } else if v.class == HostClass::Public {
            push_unique(Escalation::ProviderHost(v.host.clone()), &mut escalations);
        } else if v.class == HostClass::LinkLocal {
            push_unique(
                Escalation::ProviderHostLinkLocal(v.host.clone()),
                &mut escalations,
            );
        }
        host_changed = origin(&current.ollama_base_url) != origin(&v.url);
        merged.ollama_base_url = v.url;
    }

    for tool in &inc_tools(&merged) {
        if !cur.auto_approve_tools.contains(tool) {
            push_unique(Escalation::AutoApproveTool(tool.clone()), &mut escalations);
        }
    }
    if !cur.auto_approve_reads && merged.agent_autonomy.auto_approve_reads {
        push_unique(Escalation::AutoApproveReads, &mut escalations);
    }

    // ── F11: provider keys — None = unchanged ───────────────────────────
    let (gem_u, gem) = plan_key(&merged.gemini_api_key, &current.gemini_api_key);
    let (or_u, or) = plan_key(&merged.openrouter_api_key, &current.openrouter_api_key);
    let (mut ol_u, mut ol) = plan_key(&merged.ollama_api_key, &current.ollama_api_key);
    let (oa_u, oa) = plan_key(&merged.openai_api_key, &current.openai_api_key);
    let (an_u, an) = plan_key(&merged.anthropic_api_key, &current.anthropic_api_key);
    // Which removals the RENDERER asked for (`Some("")`), captured before the
    // address-change rule below turns a left-alone token into a `Clear`.
    let removal_requested =
        [&gem_u, &or_u, &ol_u, &oa_u, &an_u].map(|u| matches!(u, KeyUpdate::Clear));
    // A token saved for server A must not follow the user's chats to server B
    // just because the webview left the field alone (reviewer finding #4c; the
    // probe commands already refuse this). A new token supplied together with
    // the new address is kept.
    if host_changed && matches!(ol_u, KeyUpdate::Keep) {
        ol_u = KeyUpdate::Clear;
        ol = None;
    }

    // ── R3: saving/replacing a key routes every FUTURE chat to that key's
    // account — confirm out-of-band. Re-saving the SAME key widens nothing.
    // AK-8: removing a saved key is confirmed too (a script wiping every key is
    // an availability attack); removing a key that is not there is a no-op.
    for (i, (update, current_key, provider)) in [
        (&gem_u, &current.gemini_api_key, "Gemini"),
        (&or_u, &current.openrouter_api_key, "OpenRouter"),
        (&ol_u, &current.ollama_api_key, "Ollama"),
        (&oa_u, &current.openai_api_key, "OpenAI"),
        (&an_u, &current.anthropic_api_key, "Anthropic"),
    ]
    .into_iter()
    .enumerate()
    {
        if let KeyUpdate::Set(new_key) = update {
            if current_key.as_ref() != Some(new_key) {
                push_unique(Escalation::ProviderKey(provider), &mut escalations);
            }
        }
        if removal_requested[i] && current_key.is_some() {
            push_unique(Escalation::ProviderKeyRemoval(provider), &mut escalations);
        }
    }

    // ── AK-8: where "Report a problem" sends the user. Validated and, unless it
    // is the built-in default, confirmed — only when it CHANGES, so an existing
    // value (including an old `http://` one) keeps working until edited.
    if merged.support_endpoint.as_deref().map(str::trim)
        != current.support_endpoint.as_deref().map(str::trim)
    {
        match validate_support_endpoint(merged.support_endpoint.as_deref())? {
            None => merged.support_endpoint = None,
            Some(v) => {
                let is_default =
                    Settings::default().support_endpoint.as_deref() == Some(v.as_str());
                if !is_default {
                    push_unique(Escalation::SupportEndpoint(v.clone()), &mut escalations);
                }
                merged.support_endpoint = Some(v);
            }
        }
    }

    if escalations.len() > MAX_ESCALATIONS_PER_SAVE {
        return Err(format!(
            "This save changes {} security settings at once. Apply them a few at a time \
             (at most {MAX_ESCALATIONS_PER_SAVE}) so each one can be confirmed.",
            escalations.len()
        ));
    }

    merged.gemini_api_key = gem;
    merged.openrouter_api_key = or;
    merged.ollama_api_key = ol;
    merged.openai_api_key = oa;
    merged.anthropic_api_key = an;

    Ok(SettingsPlan {
        merged,
        key_updates: [gem_u, or_u, ol_u, oa_u, an_u],
        escalations,
    })
}

fn inc_tools(s: &Settings) -> Vec<String> {
    s.agent_autonomy.auto_approve_tools.clone()
}

/// Longest support address accepted (a `mailto:` with a long subject line is
/// the realistic upper end).
const MAX_SUPPORT_ENDPOINT_LEN: usize = 2000;

/// Validate a NEW `support_endpoint`: `None` for blank (clearing it), else
/// `mailto:` or `https://` only, no control characters, and for https a real
/// host with no `user:pass@` (the host a reader sees must be the host that
/// opens). `http://` is refused: the report would travel in the clear to a
/// page the user cannot verify.
fn validate_support_endpoint(raw: Option<&str>) -> Result<Option<String>, String> {
    let t = raw.map(str::trim).unwrap_or("");
    if t.is_empty() {
        return Ok(None);
    }
    if t.len() > MAX_SUPPORT_ENDPOINT_LEN || t.chars().any(char::is_control) {
        return Err("The support address is too long or contains control characters.".into());
    }
    let lower = t.to_ascii_lowercase();
    let parsed =
        url::Url::parse(t).map_err(|e| format!("The support address is not a valid URL ({e})."))?;
    if lower.starts_with("mailto:") && parsed.scheme() == "mailto" {
        return Ok(Some(t.to_string()));
    }
    if lower.starts_with("https://") && parsed.scheme() == "https" {
        if parsed.host().is_none() {
            return Err("The support address is missing a host.".into());
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err("The support address must not contain user:password@.".into());
        }
        return Ok(Some(t.to_string()));
    }
    Err("The support address must start with mailto: or https://".into())
}

/// Add `e` unless it is already listed: a script cannot pad the dialog by
/// repeating one line.
fn push_unique(e: Escalation, list: &mut Vec<Escalation>) {
    if !list.contains(&e) {
        list.push(e);
    }
}

/// Scheme + host + port of a base URL: what a bearer token is bound to. `None`
/// for anything unparseable, which compares unequal to every real origin.
fn origin(base_url: &str) -> Option<(String, String, Option<u16>)> {
    let v = provider_url::validate_provider_base_url(base_url).ok()?;
    let u = url::Url::parse(&v.url).ok()?;
    Some((u.scheme().to_string(), v.host, u.port_or_known_default()))
}

/// True when every escalation in `now` was already shown to the user in
/// `confirmed`. Used after the dialog returns: the plan is recomputed under the
/// settings lock, and if it grew in the meantime (another save landed while the
/// dialog was open) the save is refused rather than riding on a confirmation
/// that never mentioned the new change.
pub(crate) fn covered_by(confirmed: &[Escalation], now: &[Escalation]) -> bool {
    now.iter().all(|e| confirmed.contains(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Settings {
        Settings::default()
    }

    fn key(s: &str) -> Option<SecretString> {
        Some(SecretString::from(s))
    }

    // ── F1 ──────────────────────────────────────────────────────────────

    #[test]
    fn unchanged_settings_escalate_nothing() {
        let cur = base();
        let plan = plan_settings_update(&cur, cur.clone()).unwrap();
        assert!(plan.escalations.is_empty());
    }

    #[test]
    fn enabling_auto_approve_writes_escalates() {
        let cur = base();
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_writes = true;
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(plan.escalations, vec![Escalation::AutoApproveWrites]);
    }

    #[test]
    fn disabling_auto_approve_writes_is_not_an_escalation() {
        let mut cur = base();
        cur.agent_autonomy.auto_approve_writes = true;
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_writes = false;
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn already_enabled_writes_do_not_re_prompt_on_unrelated_saves() {
        let mut cur = base();
        cur.agent_autonomy.auto_approve_writes = true;
        let mut inc = cur.clone();
        inc.model = "another-model".into();
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn newly_pinned_tools_escalate_but_existing_ones_do_not() {
        let mut cur = base();
        cur.agent_autonomy.auto_approve_tools = vec!["roost_edit".into()];
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_tools = vec!["roost_edit".into(), "roost_write".into()];
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::AutoApproveTool("roost_write".into())]
        );
    }

    #[test]
    fn removing_a_pinned_tool_is_not_an_escalation() {
        let mut cur = base();
        cur.agent_autonomy.auto_approve_tools = vec!["roost_edit".into()];
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_tools.clear();
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn unblocking_a_path_escalates_but_adding_a_block_does_not() {
        let mut cur = base();
        cur.agent_autonomy.agent_blocked_paths = vec!["Journal/".into(), "Finance/".into()];
        let mut inc = cur.clone();
        inc.agent_autonomy.agent_blocked_paths = vec!["Finance/".into(), "Secrets/".into()];
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::UnblockPath("Journal/".into())]
        );
    }

    #[test]
    fn the_audit_misuse_example_is_fully_detected() {
        // Verbatim from the sharp-edges report: one call that disarms it all.
        let mut cur = base();
        cur.agent_autonomy.agent_blocked_paths = vec!["Journal/".into(), "Finance/".into()];
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_reads = true;
        inc.agent_autonomy.auto_approve_writes = true;
        inc.agent_autonomy.auto_approve_tools =
            vec!["roost_write".into(), "roost_edit".into(), "task".into()];
        inc.agent_autonomy.agent_blocked_paths = vec![];
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(plan.escalations.contains(&Escalation::AutoApproveWrites));
        assert!(plan
            .escalations
            .contains(&Escalation::AutoApproveTool("task".into())));
        assert!(plan
            .escalations
            .contains(&Escalation::UnblockPath("Journal/".into())));
        assert!(plan
            .escalations
            .contains(&Escalation::UnblockPath("Finance/".into())));
        assert_eq!(plan.escalations.len(), 6);
    }

    // ── F2 ──────────────────────────────────────────────────────────────

    #[test]
    fn pointing_ollama_at_a_public_host_escalates_and_normalizes() {
        let cur = base();
        let mut inc = cur.clone();
        inc.ollama_base_url = "https://Ollama.Example.com/".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::ProviderHost("ollama.example.com".into())]
        );
        assert_eq!(plan.merged.ollama_base_url, "https://ollama.example.com");
    }

    #[test]
    fn pointing_ollama_at_the_lan_or_loopback_needs_no_prompt() {
        let cur = base();
        for url in [
            "http://192.168.1.9:11434",
            "http://localhost:11434",
            "http://my-pc:11434",
        ] {
            let mut inc = cur.clone();
            inc.ollama_base_url = url.into();
            let plan = plan_settings_update(&cur, inc).unwrap();
            assert!(plan.escalations.is_empty(), "{url}");
        }
    }

    #[test]
    fn plain_http_to_the_internet_escalates_with_an_unencrypted_warning() {
        // Not rejected outright (split-DNS home labs resolve a public-looking
        // name to a LAN address and must stay configurable) — but the dialog
        // says it is unencrypted, and it IS a dialog.
        let cur = base();
        let mut inc = cur.clone();
        inc.ollama_base_url = "http://ollama.home.example.com:11434".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::ProviderHostPlaintext(
                "ollama.home.example.com".into()
            )]
        );
        assert!(plan.escalations[0].describe().contains("UNENCRYPTED"));
    }

    #[test]
    fn a_tailscale_funnel_name_is_not_a_free_pass() {
        let cur = base();
        let mut inc = cur.clone();
        inc.ollama_base_url = "https://x.tail1234.ts.net".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::ProviderHost("x.tail1234.ts.net".into())]
        );
    }

    #[test]
    fn invalid_shapes_are_still_rejected_outright() {
        let cur = base();
        for bad in [
            "http://localhost:11434@evil.example",
            "ftp://x",
            "https://h.example/?q=1",
        ] {
            let mut inc = cur.clone();
            inc.ollama_base_url = bad.into();
            let e = plan_settings_update(&cur, inc).err().expect(bad);
            assert!(e.contains("Ollama server address"), "{bad}: {e}");
        }
    }

    // ── a token must not follow chats to a different server ─────────────

    #[test]
    fn changing_the_server_drops_the_saved_token_unless_a_new_one_is_supplied() {
        let mut cur = base();
        cur.ollama_base_url = "https://ollama.mine.example".into();
        cur.ollama_api_key = key("TOKEN-FOR-OLD-SERVER");

        // Webview leaves the token field alone (None) and re-points the URL.
        let mut inc = cur.clone();
        inc.ollama_api_key = None;
        inc.ollama_base_url = "https://attacker.example".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[2], KeyUpdate::Clear));
        assert!(plan.merged.ollama_api_key.is_none());

        // A token supplied WITH the new address is the user's intent: keep it.
        let mut inc = cur.clone();
        inc.ollama_api_key = key("NEW-TOKEN");
        inc.ollama_base_url = "https://other.example".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[2], KeyUpdate::Set(_)));
        assert_eq!(plan.merged.ollama_api_key.as_deref(), Some("NEW-TOKEN"));
    }

    #[test]
    fn a_cosmetic_url_change_on_the_same_server_keeps_the_token() {
        let mut cur = base();
        cur.ollama_base_url = "https://ollama.mine.example".into();
        cur.ollama_api_key = key("TOKEN");
        let mut inc = cur.clone();
        inc.ollama_api_key = None;
        inc.ollama_base_url = "https://Ollama.Mine.Example/".into(); // same origin
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[2], KeyUpdate::Keep));
        assert_eq!(plan.merged.ollama_api_key.as_deref(), Some("TOKEN"));
    }

    // ── the dialog cannot be padded or shortened ────────────────────────

    #[test]
    fn repeating_one_line_cannot_pad_the_dialog() {
        let cur = base();
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_tools = vec!["roost_search".into(); 50];
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::AutoApproveTool("roost_search".into())]
        );
    }

    #[test]
    fn a_save_with_more_escalations_than_the_dialog_can_show_is_refused() {
        let mut cur = base();
        cur.agent_autonomy.agent_blocked_paths = vec!["Journal/".into()];
        let mut inc = cur.clone();
        // Nine distinct harmless-looking tool names + the dangerous un-block.
        inc.agent_autonomy.auto_approve_tools = (0..9).map(|i| format!("fake_tool_{i}")).collect();
        inc.agent_autonomy.agent_blocked_paths.clear();
        let e = plan_settings_update(&cur, inc)
            .err()
            .expect("must refuse, not shorten");
        assert!(e.contains("at once"), "{e}");
    }

    #[test]
    fn exactly_at_the_cap_everything_is_listed_most_serious_first() {
        let mut cur = base();
        cur.agent_autonomy.agent_blocked_paths = vec!["Journal/".into()];
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_writes = true;
        inc.agent_autonomy.agent_blocked_paths.clear();
        inc.agent_autonomy.auto_approve_tools = (0..(MAX_ESCALATIONS_PER_SAVE - 2))
            .map(|i| format!("t{i}"))
            .collect();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(plan.escalations.len(), MAX_ESCALATIONS_PER_SAVE);
        assert_eq!(plan.escalations[0], Escalation::AutoApproveWrites);
        assert_eq!(
            plan.escalations[1],
            Escalation::UnblockPath("Journal/".into())
        );
    }

    #[test]
    fn turning_read_review_back_off_is_an_escalation() {
        let mut cur = base();
        cur.agent_autonomy.auto_approve_reads = false; // user reviews every read
        let mut inc = cur.clone();
        inc.agent_autonomy.auto_approve_reads = true;
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(plan.escalations, vec![Escalation::AutoApproveReads]);
        // ...and the default (reads auto-approved) stays prompt-free.
        let cur = base();
        assert!(plan_settings_update(&cur, cur.clone())
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn an_unchanged_legacy_url_is_grandfathered_not_revalidated() {
        // Existing install with an odd value: saving an unrelated setting must
        // not start failing. (`build_chat_provider` still enforces at use.)
        let mut cur = base();
        cur.ollama_base_url = "http://old-public.example:11434".into();
        let mut inc = cur.clone();
        inc.model = "x".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(plan.escalations.is_empty());
        assert_eq!(
            plan.merged.ollama_base_url,
            "http://old-public.example:11434"
        );
    }

    #[test]
    fn a_changed_host_that_is_not_public_does_not_prompt_but_still_validates() {
        let cur = base();
        let mut inc = cur.clone();
        inc.ollama_base_url = "ftp://192.168.1.1".into();
        assert!(plan_settings_update(&cur, inc).is_err());
    }

    // ── AK-8: link-local, *.localhost, metadata ─────────────────────────

    #[test]
    fn a_link_local_address_needs_the_dialog_but_a_lan_one_does_not() {
        let cur = base();
        for url in ["http://169.254.10.10:11434", "http://[fe80::1]:11434"] {
            let mut inc = cur.clone();
            inc.ollama_base_url = url.into();
            let plan = plan_settings_update(&cur, inc).unwrap();
            assert_eq!(plan.escalations.len(), 1, "{url}");
            assert!(
                matches!(plan.escalations[0], Escalation::ProviderHostLinkLocal(_)),
                "{url}: {:?}",
                plan.escalations
            );
            assert!(plan.escalations[0].describe().contains("link-local"));
        }
    }

    #[test]
    fn a_dot_localhost_name_is_confirmed_like_any_internet_name() {
        let cur = base();
        let mut inc = cur.clone();
        inc.ollama_base_url = "https://ollama.localhost".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![Escalation::ProviderHost("ollama.localhost".into())]
        );
        // Bare `localhost` stays prompt-free.
        let mut inc = cur.clone();
        inc.ollama_base_url = "http://localhost:9999".into();
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn cloud_metadata_addresses_cannot_be_saved_at_all() {
        let cur = base();
        for url in [
            "http://169.254.169.254",
            "http://169.254.169.254/latest/meta-data",
            "http://metadata.google.internal",
            "http://[fd00:ec2::254]",
        ] {
            let mut inc = cur.clone();
            inc.ollama_base_url = url.into();
            let e = plan_settings_update(&cur, inc).err().expect(url);
            assert!(e.contains("metadata"), "{url}: {e}");
        }
    }

    // ── AK-9: a legacy credentialed address keeps working ───────────────

    #[test]
    fn an_unchanged_legacy_address_with_credentials_survives_unrelated_saves() {
        let mut cur = base();
        cur.ollama_base_url = "http://alice:s3cret@proxy.lan:11434".into();
        cur.ollama_api_key = key("TOKEN");
        let mut inc = cur.clone();
        inc.ollama_api_key = None;
        inc.model = "x".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(plan.escalations.is_empty());
        // Byte-for-byte: rewriting it would silently break the proxy login.
        assert_eq!(
            plan.merged.ollama_base_url,
            "http://alice:s3cret@proxy.lan:11434"
        );
        assert!(matches!(plan.key_updates[2], KeyUpdate::Keep));
    }

    #[test]
    fn a_new_address_with_credentials_is_still_refused_even_to_replace_a_legacy_one() {
        let mut cur = base();
        cur.ollama_base_url = "http://alice:s3cret@proxy.lan:11434".into();
        let mut inc = cur.clone();
        inc.ollama_base_url = "http://bob:pw@proxy.lan:11434".into();
        let e = plan_settings_update(&cur, inc).err().expect("must refuse");
        assert!(e.contains("user:password@"), "{e}");
        // The way out: re-enter it without credentials. Same server, so the
        // saved token stays.
        let mut cur = base();
        cur.ollama_base_url = "http://alice:s3cret@proxy.lan:11434".into();
        cur.ollama_api_key = key("TOKEN");
        let mut inc = cur.clone();
        inc.ollama_api_key = None;
        inc.ollama_base_url = "http://proxy.lan:11434".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(plan.escalations.is_empty());
        assert!(matches!(plan.key_updates[2], KeyUpdate::Keep));
        assert_eq!(plan.merged.ollama_api_key.as_deref(), Some("TOKEN"));
    }

    // ── AK-8: removing a key, and the support address ───────────────────

    #[test]
    fn removing_a_saved_key_is_confirmed_but_removing_an_absent_one_is_not() {
        let mut cur = base();
        cur.gemini_api_key = key("old");
        cur.anthropic_api_key = key("old2");
        // Remove both saved keys, plus clear one that was never set.
        let mut inc = cur.clone();
        inc.gemini_api_key = key("");
        inc.anthropic_api_key = key("   ");
        inc.openai_api_key = key("");
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert_eq!(
            plan.escalations,
            vec![
                Escalation::ProviderKeyRemoval("Gemini"),
                Escalation::ProviderKeyRemoval("Anthropic"),
            ]
        );
        assert!(plan.escalations[0]
            .describe()
            .contains("Remove the saved API key"));
        assert!(plan.escalations.iter().all(Escalation::is_key_change));
        // Only the never-set one: nothing to ask.
        let mut inc = cur.clone();
        inc.openai_api_key = key("");
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn the_token_dropped_by_an_address_change_is_not_a_removal_prompt() {
        // The user did not ask to remove it; the address change is what is asked.
        let mut cur = base();
        cur.ollama_base_url = "https://ollama.mine.example".into();
        cur.ollama_api_key = key("TOKEN-FOR-OLD-SERVER");
        let mut inc = cur.clone();
        inc.ollama_api_key = None;
        inc.ollama_base_url = "https://attacker.example".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[2], KeyUpdate::Clear));
        assert_eq!(
            plan.escalations,
            vec![Escalation::ProviderHost("attacker.example".into())]
        );
    }

    #[test]
    fn key_changes_are_told_apart_from_safety_loosening() {
        assert!(Escalation::ProviderKey("Gemini").is_key_change());
        assert!(Escalation::ProviderKeyRemoval("Gemini").is_key_change());
        for e in [
            Escalation::AutoApproveWrites,
            Escalation::AutoApproveReads,
            Escalation::UnblockPath("x".into()),
            Escalation::ProviderHost("h".into()),
            Escalation::SupportEndpoint("mailto:x@y.z".into()),
        ] {
            assert!(!e.is_key_change(), "{e:?}");
        }
    }

    #[test]
    fn a_new_support_address_is_confirmed_but_the_default_and_clearing_are_not() {
        let cur = base();
        let default = Settings::default().support_endpoint.unwrap();
        // Restoring / re-sending the built-in default: silent.
        let mut inc = cur.clone();
        inc.support_endpoint = Some(default.clone());
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
        // Clearing: silent (it only makes the app copy-to-clipboard).
        for blank in [None, Some(String::new()), Some("   ".to_string())] {
            let mut inc = cur.clone();
            inc.support_endpoint = blank;
            let plan = plan_settings_update(&cur, inc).unwrap();
            assert!(plan.escalations.is_empty());
            assert_eq!(plan.merged.support_endpoint, None);
        }
        // Anywhere else: asked, and normalized (trimmed).
        for addr in [
            "mailto:help@my.example",
            "https://github.com/me/repo/issues/new",
        ] {
            let mut inc = cur.clone();
            inc.support_endpoint = Some(format!("  {addr} "));
            let plan = plan_settings_update(&cur, inc).unwrap();
            assert_eq!(
                plan.escalations,
                vec![Escalation::SupportEndpoint(addr.into())]
            );
            assert_eq!(plan.merged.support_endpoint.as_deref(), Some(addr));
        }
        // A user who already has a custom value is not re-asked on unrelated saves.
        let mut cur2 = base();
        cur2.support_endpoint = Some("https://mine.example/report".into());
        let mut inc = cur2.clone();
        inc.model = "other".into();
        assert!(plan_settings_update(&cur2, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    #[test]
    fn a_support_address_must_be_mailto_or_https_without_credentials() {
        let cur = base();
        for bad in [
            "http://plain.example/report",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://x.example",
            "https://user:pw@evil.example/",
            "https://disguised.example@evil.example/",
            "not a url",
            "https://x.example/\u{0007}bell",
        ] {
            let mut inc = cur.clone();
            inc.support_endpoint = Some(bad.into());
            let e = plan_settings_update(&cur, inc)
                .err()
                .unwrap_or_else(|| panic!("{bad}"));
            assert!(e.contains("support address"), "{bad}: {e}");
        }
        let mut inc = cur.clone();
        inc.support_endpoint = Some(format!("https://x.example/{}", "a".repeat(3000)));
        assert!(plan_settings_update(&cur, inc).is_err());
    }

    #[test]
    fn a_legacy_http_support_address_survives_until_it_is_edited() {
        let mut cur = base();
        cur.support_endpoint = Some("http://old.example/report".into());
        let mut inc = cur.clone();
        inc.model = "x".into();
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(plan.escalations.is_empty());
        assert_eq!(
            plan.merged.support_endpoint.as_deref(),
            Some("http://old.example/report")
        );
    }

    #[test]
    fn new_dialog_lines_are_sanitized_and_name_the_real_target() {
        for e in [
            Escalation::SupportEndpoint("https://x.example/\nFAKE: ok".into()),
            Escalation::ProviderHostLinkLocal("169.254.1.1\r\nFAKE".into()),
        ] {
            let line = e.describe();
            assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        }
        let long = format!(
            "https://ollama.example.com.{}.evil.net/report",
            "a".repeat(300)
        );
        let line = Escalation::SupportEndpoint(long).describe();
        assert!(
            line.contains("evil.net/report"),
            "tail stays visible: {line}"
        );
    }

    // ── F11 ─────────────────────────────────────────────────────────────

    #[test]
    fn a_none_key_means_unchanged_not_delete() {
        let mut cur = base();
        cur.gemini_api_key = key("AIza-live");
        let mut inc = cur.clone();
        inc.gemini_api_key = None; // what a redacted get_settings round-trips as
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[0], KeyUpdate::Keep));
        assert_eq!(plan.merged.gemini_api_key.as_deref(), Some("AIza-live"));
    }

    #[test]
    fn an_empty_string_clears_the_key() {
        let mut cur = base();
        cur.openrouter_api_key = key("sk-or-v1-abc");
        let mut inc = cur.clone();
        inc.openrouter_api_key = key("   ");
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[1], KeyUpdate::Clear));
        assert!(plan.merged.openrouter_api_key.is_none());
    }

    #[test]
    fn a_new_key_is_set_trimmed() {
        let cur = base();
        let mut inc = cur.clone();
        inc.anthropic_api_key = key("  sk-ant-xyz \n");
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(matches!(plan.key_updates[4], KeyUpdate::Set(_)));
        assert_eq!(plan.merged.anthropic_api_key.as_deref(), Some("sk-ant-xyz"));
    }

    #[test]
    fn each_provider_maps_to_its_own_slot() {
        let cur = base();
        let mut inc = cur.clone();
        inc.gemini_api_key = key("g");
        inc.openrouter_api_key = key("r");
        inc.ollama_api_key = key("o");
        inc.openai_api_key = key("a");
        inc.anthropic_api_key = key("n");
        let plan = plan_settings_update(&cur, inc).unwrap();
        assert!(plan
            .key_updates
            .iter()
            .all(|u| matches!(u, KeyUpdate::Set(_))));
        assert_eq!(plan.merged.ollama_api_key.as_deref(), Some("o"));
        assert_eq!(plan.merged.openai_api_key.as_deref(), Some("a"));
    }

    #[test]
    fn setting_a_new_key_escalates_but_clearing_or_resaving_does_not() {
        // R3: replacing a key (or setting a first one) routes every FUTURE
        // chat to that key's vendor account — a persistence channel that
        // outlives the attack. Gated now that the CSP trim closed the
        // webview's direct POST to those hosts. Removing a key or re-saving
        // the SAME key widens nothing.
        let mut cur = base();
        cur.gemini_api_key = key("old");
        // Replace → escalate.
        let mut inc = cur.clone();
        inc.gemini_api_key = key("new");
        assert_eq!(
            plan_settings_update(&cur, inc).unwrap().escalations,
            vec![Escalation::ProviderKey("Gemini")]
        );
        // First-ever set → escalate (an attacker-chosen first key is the
        // same channel).
        let mut inc = base();
        inc.openai_api_key = key("a");
        assert_eq!(
            plan_settings_update(&base(), inc).unwrap().escalations,
            vec![Escalation::ProviderKey("OpenAI")]
        );
        // Clear only → no ProviderKey escalation (AK-8: removal has its own
        // `ProviderKeyRemoval`, covered in
        // `removing_a_saved_key_is_confirmed_but_removing_an_absent_one_is_not`).
        // `gemini` was saved above; `anthropic` never was, so it adds nothing.
        let mut inc = cur.clone();
        inc.gemini_api_key = key("");
        inc.anthropic_api_key = key("");
        assert_eq!(
            plan_settings_update(&cur, inc).unwrap().escalations,
            vec![Escalation::ProviderKeyRemoval("Gemini")]
        );
        // Re-save the same value → no re-prompt on unrelated saves.
        let mut inc = cur.clone();
        inc.model = "other".into();
        assert!(plan_settings_update(&cur, inc)
            .unwrap()
            .escalations
            .is_empty());
    }

    // ── confirmation coverage ───────────────────────────────────────────

    #[test]
    fn covered_by_requires_every_new_escalation_to_have_been_shown() {
        let shown = vec![Escalation::AutoApproveWrites];
        assert!(covered_by(&shown, &[Escalation::AutoApproveWrites]));
        assert!(covered_by(&shown, &[]));
        assert!(!covered_by(
            &shown,
            &[
                Escalation::AutoApproveWrites,
                Escalation::AutoApproveTool("x".into())
            ]
        ));
    }

    #[test]
    fn dialog_lines_are_sanitized() {
        let e = Escalation::UnblockPath("a\nFAKE: all fine\r\n".into());
        let line = e.describe();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
    }
}
