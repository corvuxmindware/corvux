//! Pro entitlement — the single source of truth for "is this device allowed
//! to use a paid capability?"
//!
//! ## Status: Phase A STUB
//!
//! [`current_tier`] returns [`Tier::Free`] for everyone. The real
//! implementation — validate a signed entitlement token from the OS keychain
//! (`secret_store`) — lands in **Phase B**, when payments go live. **Call
//! sites do not change when that happens**; only the body of
//! [`current_tier`] does. See `docs/decisions/pro-entitlement-spine.md` and
//! `docs/decisions/tier-feature-split.md`.
//!
//! ## Design rules (do not break)
//!
//! 1. **Ask by capability, never by tier.** Paid code paths call
//!    [`allows`]`(Capability::X)`, not `current_tier() == Tier::Pro`. Only the
//!    [`Capability::required_tier`] map below knows the tier split — it mirrors
//!    the table in `tier-feature-split.md`. Re-splitting tiers later changes
//!    that one map, not every call site.
//! 2. **Fail closed to Free.** A missing / expired / unparseable entitlement
//!    must never break the free path — it only removes the paid extra. Any
//!    doubt resolves to [`Tier::Free`]. (Prime invariant, spine doc §9.0.)
//! 3. **Thin gate, fat feature.** Check [`allows`] once at the *entry* of a
//!    paid path; don't sprinkle tier checks through the feature's internals.
//!
//! The client-side check here is a *soft* gate (drives UI + local routing).
//! The surfaces that actually cost money — the relay queue and the hosted-AI
//! proxy — additionally **hard-gate server-side** on the signed token; a
//! patched client can fake this bool but not the server check.

use serde::Serialize;
use specta::Type;

/// A paid, server-cost capability. Add a variant **only** when a new paid
/// feature ships — everything not listed here is free, forever (Locked
/// Decision: free is the whole personal product).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Type)]
pub enum Capability {
    /// Corvux-operated relay / store-and-forward queue: sync across networks
    /// and when the other device is offline. Direct P2P/LAN sync is **free**
    /// and is *not* gated through here.
    RelaySync,
    /// Corvux-managed ("hosted") AI — provider keys live on our side, no setup
    /// for the user. BYOK AI (the user's own key) is **free** and is *not*
    /// gated through here.
    HostedAi,
}

impl Capability {
    /// Every paid capability, for iteration. Keep in sync with the variants
    /// above — the snapshot/test code walks this.
    pub const ALL: [Capability; 2] = [Capability::RelaySync, Capability::HostedAi];

    /// The minimum [`Tier`] that unlocks this capability. **This map is the
    /// pricing decision in code** — keep it in lockstep with
    /// `tier-feature-split.md`.
    const fn required_tier(self) -> Tier {
        match self {
            Capability::RelaySync | Capability::HostedAi => Tier::Pro,
        }
    }
}

/// The device's entitlement tier. Two tiers only — subscription model, no
/// Lifetime (see `pro-entitlement-spine.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum Tier {
    Free,
    Pro,
}

/// Wire shape handed to the frontend so UI can gate without re-implementing
/// the tier→capability map: the current tier plus the resolved set of
/// unlocked capabilities. The frontend gates on membership in `allowed`.
#[derive(Debug, Clone, Serialize, Type)]
pub struct EntitlementSnapshot {
    pub tier: Tier,
    /// Capabilities unlocked at `tier`. Empty on Free today.
    pub allowed: Vec<Capability>,
}

/// The device's current entitlement tier.
///
/// **Phase A stub:** always [`Tier::Free`]. In **debug builds only**, the
/// `CORVUX_FORCE_TIER` env var (`pro` / `free`) overrides the result so QA can
/// exercise both states without a real token; it is compiled out of release
/// builds entirely. Phase B replaces the body with keychain-token validation.
pub fn current_tier() -> Tier {
    #[cfg(debug_assertions)]
    {
        if let Ok(forced) = std::env::var("CORVUX_FORCE_TIER") {
            return match forced.trim().to_ascii_lowercase().as_str() {
                "pro" => Tier::Pro,
                _ => Tier::Free,
            };
        }
    }
    Tier::Free
}

/// The one question every paid code path asks. `true` iff the current tier
/// meets `cap`'s required tier. Fails closed to free.
pub fn allows(cap: Capability) -> bool {
    allows_for(current_tier(), cap)
}

/// The current entitlement as a frontend-facing snapshot.
pub fn snapshot() -> EntitlementSnapshot {
    snapshot_for(current_tier())
}

/// Pure core of [`allows`] — split out so the gating logic is testable without
/// touching env / keychain. `have` is the device's tier; returns whether it
/// unlocks `cap`.
fn allows_for(have: Tier, cap: Capability) -> bool {
    tier_meets(have, cap.required_tier())
}

/// Pure core of [`snapshot`] — resolve the unlocked set for a given tier.
fn snapshot_for(tier: Tier) -> EntitlementSnapshot {
    let allowed = Capability::ALL
        .into_iter()
        .filter(|cap| allows_for(tier, *cap))
        .collect();
    EntitlementSnapshot { tier, allowed }
}

/// Does `have` satisfy `need`? Pro satisfies everything; Free satisfies only
/// Free-tier requirements. (With two tiers this is trivial; written
/// exhaustively so adding a tier forces a compile-time review here.)
fn tier_meets(have: Tier, need: Tier) -> bool {
    match (have, need) {
        (Tier::Pro, _) => true,
        (Tier::Free, Tier::Free) => true,
        (Tier::Free, Tier::Pro) => false,
    }
}

/// Frontend-facing entitlement snapshot (tier + unlocked capabilities). The
/// UI gates on this; it is a *soft* signal (a patched client can lie), so the
/// money-costing surfaces re-check server-side. Cheap + synchronous today
/// (stub); the async signature is future-proof for the Phase B keychain read.
#[tauri::command]
#[specta::specta]
pub async fn get_entitlement() -> Result<EntitlementSnapshot, String> {
    Ok(snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_denies_every_paid_capability() {
        // The whole point of the gate: Free unlocks none of the paid set.
        for cap in Capability::ALL {
            assert!(!allows_for(Tier::Free, cap), "Free must not unlock {cap:?}");
        }
    }

    #[test]
    fn pro_unlocks_every_paid_capability() {
        for cap in Capability::ALL {
            assert!(allows_for(Tier::Pro, cap), "Pro must unlock {cap:?}");
        }
    }

    #[test]
    fn every_capability_currently_requires_pro() {
        // Guards the invariant that nothing in `Capability` is accidentally
        // free-gated. If a future capability is Free-tier, update this test
        // deliberately rather than letting a typo slip a paid feature open.
        for cap in Capability::ALL {
            assert_eq!(
                cap.required_tier(),
                Tier::Pro,
                "{cap:?} should be Pro-gated"
            );
        }
    }

    #[test]
    fn tier_meets_is_exhaustive_and_correct() {
        assert!(tier_meets(Tier::Pro, Tier::Pro));
        assert!(tier_meets(Tier::Pro, Tier::Free));
        assert!(tier_meets(Tier::Free, Tier::Free));
        assert!(!tier_meets(Tier::Free, Tier::Pro));
    }

    #[test]
    fn snapshot_reflects_tier() {
        // Free → nothing unlocked; Pro → the whole paid set.
        assert!(snapshot_for(Tier::Free).allowed.is_empty());
        assert_eq!(snapshot_for(Tier::Pro).allowed, Capability::ALL.to_vec());
    }
}
