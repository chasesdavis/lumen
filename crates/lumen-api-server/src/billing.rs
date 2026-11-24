//! Pricing model + COGS simulation for the pass-through billing plan.
//!
//! Test plan requirement: "simulate storage cost curves at 10GB/100GB/1TB
//! user profiles against each provider to confirm our $3+pass-through model
//! remains cheaper than $10/mo down to ~100GB usage point". Provider rates
//! are pinned here so the tests double as the documented assumptions; update
//! this file whenever a provider changes published pricing.

use lumen_core::error::{LumenError, Result};

pub const BASE_MONTHLY_CENTS: u64 = 300;
pub const STORAGE_MARGIN_BPS: u64 = 1_000; // 10% above provider cost
pub const HELUMI_MONTHLY_CENTS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provider {
    CloudflareR2,
    BackblazeB2,
    Storj,
}

impl Provider {
    /// Storage price in cents per GB-month, from provider pricing pages.
    /// Re-verify against published rates whenever tiers are shipped.
    pub fn cents_per_gb_month(self) -> f64 {
        match self {
            Provider::CloudflareR2 => 1.5,
            Provider::BackblazeB2 => 0.695,
            Provider::Storj => 0.7,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::CloudflareR2 => "r2",
            Provider::BackblazeB2 => "b2",
            Provider::Storj => "storj",
        }
    }
}

/// Monthly customer price for one profile: base fee + metered storage at
/// provider cost plus margin. Chunking overhead is accounted for as ~5%
/// extra stored bytes (content-defined chunks average slightly under full
/// alignment at the tail).
pub fn monthly_price_cents(provider: Provider, used_gb: f64) -> Result<u64> {
    if used_gb < 0.0 {
        return Err(LumenError::Other("used_gb must be non-negative".into()));
    }
    let overhead_gb = used_gb * 1.05;
    let raw_storage_cost = overhead_gb * provider.cents_per_gb_month();
    let storage_with_margin = raw_storage_cost * (1.0 + STORAGE_MARGIN_BPS as f64 / 10_000.0);
    Ok(BASE_MONTHLY_CENTS + storage_with_margin.round() as u64)
}

/// BYOB pricing: user brings their own bucket and credentials, so we carry
/// zero storage cost and charge only the base fee at any volume.
pub fn monthly_price_cents_byob(_used_gb: f64) -> Result<u64> {
    Ok(BASE_MONTHLY_CENTS)
}

/// Simulated price curve across the test-plan profiles and providers.
pub fn simulate_cost_curve() -> Vec<(f64, Vec<(Provider, u64)>)> {
    [10.0_f64, 100.0, 1024.0]
        .iter()
        .map(|&gb| {
            (
                gb,
                vec![
                    (Provider::CloudflareR2, monthly_price_cents(Provider::CloudflareR2, gb).unwrap()),
                    (Provider::BackblazeB2, monthly_price_cents(Provider::BackblazeB2, gb).unwrap()),
                    (Provider::Storj, monthly_price_cents(Provider::Storj, gb).unwrap()),
                ],
            )
        })
        .collect()
}

#[cfg(test)]
fn providers_all() -> [Provider; 3] {
    [Provider::CloudflareR2, Provider::BackblazeB2, Provider::Storj]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undercut_helumi_at_10gb_and_100gb_for_every_provider() {
        for gb in [10.0_f64, 100.0] {
            for p in providers_all() {
                let price = monthly_price_cents(p, gb).unwrap();
                assert!(
                    price < HELUMI_MONTHLY_CENTS,
                    "{}GB on {:?} costs {}c - fails to beat flat rate",
                    gb,
                    p,
                    price
                );
            }
        }
    }

    #[test]
    fn one_tb_managed_pass_through_exceeds_flat_rate_by_design() {
        // At 1TB our pure pass-through model exceeds $10 on all listed
        // providers (base 300c + ~750c storage + margin). This is expected
        // and matches the plan: the $3+pass-through model targets typical
        // usage up to a few hundred GB, with heavier users steered toward
        // BYOB mode ($3 flat, their own provider credentials, zero storage
        // markup) instead of managed pass-through pricing.
        let r2 = monthly_price_cents(Provider::CloudflareR2, 1024.0).unwrap();
        assert!(r2 > HELUMI_MONTHLY_CENTS, "R2 at 1TB unexpectedly below flat rate");
        let b2 = monthly_price_cents(Provider::BackblazeB2, 1024.0).unwrap();
        assert!(b2 > HELUMI_MONTHLY_CENTS, "B2 at 1TB unexpectedly below flat rate");
    }

    #[test]
    fn byob_mode_is_base_price_only_for_any_volume() {
        // BYOB users store on their own bucket: base fee with zero storage
        // markup regardless of volume. A 10TB hoarder pays the same $3 as a
        // 50GB user - that is the intended anchor against flat-rate
        // competitors at the high end.
        for gb in [50.0_f64, 500.0, 10_000.0] {
            assert_eq!(monthly_price_cents_byob(gb).unwrap(), BASE_MONTHLY_CENTS);
        }
        assert!(BASE_MONTHLY_CENTS < HELUMI_MONTHLY_CENTS);
    }

    #[test]
    fn curve_contains_all_profiles_and_providers() {
        let curve = simulate_cost_curve();
        assert_eq!(curve.len(), 3); // 10GB / 100GB / 1TB
        assert_eq!(curve[0].1.len(), 3); // every row has all three providers
    }

    #[test]
    fn negative_usage_rejected() {
        assert!(monthly_price_cents(Provider::Storj, -1.0).is_err());
    }
}
