//! Out-of-band confirmation through a native OS dialog.
//!
//! Every registered Tauri command is callable by whatever script runs in the
//! webview (XSS through a rendered note, a poisoned dependency, devtools). A
//! "confirmation" assembled from values the webview itself supplies — typing a
//! host twice, a `confirmed: true` flag, a modal rendered in the page — can be
//! satisfied by that same script in the same IPC message. The only answer a
//! compromised renderer cannot forge is one given in a window it does not
//! control: the OS dialog.
//!
//! Use [`NativeConfirm`] for commands whose effect is either
//!
//! * a **privilege escalation** the user did not ask for implicitly
//!   (auto-approve agent writes, un-block a path the agent must not touch,
//!   point chat traffic at a new internet host, trust a new sync-queue host), or
//! * **irreversible loss of app state** that the renderer alone authorizes
//!   (dropping a roost from the registry, which later deletes its index and
//!   sync pairing).
//!
//! The decision logic stays in pure functions (what needs confirming); this
//! module is only the thin, fail-closed prompt. Tests use [`test_support`].
//!
//! Sharp-edges audit 2026-11-11, findings F1 / F2 / F3 / F10 — one mechanism,
//! one choke point.

use async_trait::async_trait;

/// Ask the person at the keyboard, outside the webview.
#[async_trait]
pub(crate) trait NativeConfirm: Send + Sync {
    /// `true` only when the user explicitly pressed `allow_label`. Closing the
    /// dialog, pressing Cancel, or any failure to show it is `false`
    /// (fail closed).
    async fn confirm(&self, title: &str, body: &str, allow_label: &str) -> bool;
}

/// The real thing: a warning-style OS message dialog with `allow_label` and
/// Cancel buttons. Non-blocking (callback + oneshot), so it is safe to await
/// from an async command without parking a runtime thread on the dialog.
///
/// Generic over the runtime so the agent loop (generic over `R`) can use it too;
/// everywhere else `R` is inferred as the real `Wry`.
pub(crate) struct OsDialog<R: tauri::Runtime = tauri::Wry>(pub tauri::AppHandle<R>);

#[async_trait]
impl<R: tauri::Runtime> NativeConfirm for OsDialog<R> {
    async fn confirm(&self, title: &str, body: &str, allow_label: &str) -> bool {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        self.0
            .dialog()
            .message(body)
            .title(title)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                allow_label.to_string(),
                "Cancel".to_string(),
            ))
            .show(move |allowed| {
                let _ = tx.send(allowed);
            });
        // A dropped sender (dialog torn down, app closing) is "no".
        rx.await.unwrap_or(false)
    }
}

/// Dialog text is built from renderer-influenced strings (host names, path
/// globs, tool names). Keep it short and free of control characters so a
/// hostile value cannot stretch the dialog, forge extra lines, or hide the real
/// request behind its own text.
pub(crate) fn dialog_safe(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() <= max_chars {
        cleaned.to_string()
    } else {
        let cut: String = cleaned.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers every prompt the same way and counts how often it was asked.
    pub(crate) struct Scripted {
        answer: bool,
        asked: AtomicUsize,
    }

    impl Scripted {
        pub(crate) fn allow() -> Self {
            Self {
                answer: true,
                asked: AtomicUsize::new(0),
            }
        }
        pub(crate) fn deny() -> Self {
            Self {
                answer: false,
                asked: AtomicUsize::new(0),
            }
        }
        pub(crate) fn times_asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl NativeConfirm for Scripted {
        async fn confirm(&self, _title: &str, _body: &str, _allow_label: &str) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_safe_strips_control_characters() {
        assert_eq!(dialog_safe("a\nb\r\tc", 50), "a b  c");
    }

    #[test]
    fn dialog_safe_truncates_on_char_boundaries() {
        let s = "\u{754c}".repeat(100);
        let out = dialog_safe(&s, 10);
        assert_eq!(out.chars().count(), 11, "10 chars + ellipsis");
        assert!(out.ends_with('…'));
        assert_eq!(dialog_safe("short", 10), "short");
    }
}
