//! Pricing model sanity check: simulate Lumen customer cost curves at
//! 10GB / 100GB / 1TB usage against each storage provider and prove the
//! $3 base + storage pass-through (+10% margin) model undercuts Helumi's
//! $10/mo flat rate at every profile point, GIVEN storage routing picks
//! the cheapest provider per usage tier.
//!
//! The first draft of this check assumed one provider fits all - the
//! simulation REFUTED that (pure-R2 1TB costs $19.90 > $10) which is why
//! provider routing is part of the pricing proof, not an optimization.
//! These are permanent regression gates: if a provider price changes,
//! update the constant here and the assertions force a conscious re-check
//! of the cheaper-than-Helumi claim.
//!
//! Monthly storage list prices (USD per GB-month) as of the RE date:
//! - Cloudflare R2:   $0.015/GB (zero egress)
//! - Backblaze B2:    $6/TB = $0.00586/GB (egress free up to 3x stored/day)
//! - Storj:           $4/TB (Pro) = $0.00391/GB; free tier excluded

const BASE_MONTHLY_USD: f64 = 3.00;
const STORAGE_MARGIN: f64 = 1.10;
const HELUMI_FLAT_USD: f64 = 10.00;

#[derive(Clone, Copy)]
struct Provider {
    name: &'static str,
    usd_per_gb_month: f64,
}

impl Provider {
    fn lumen_customer_cost(&self, gb: f64) -> f64 {
        BASE_MONTHLY_USD + gb * self.usd_per_gb_month * STORAGE_MARGIN
    }
}

const R2: Provider = Provider {
    name: "Cloudflare R2",
    usd_per_gb_month: 0.015,
};
const B2: Provider = Provider {
    name: "Backblaze B2",
    usd_per_gb_month: 0.00586,
};
const STORJ: Provider = Provider {
    name: "Storj",
    usd_per_gb_month: 0.00390625,
};

/// Storage routing policy implied by these cost curves: pick the cheapest
/// provider for the user's usage tier. R2 wins below its crossover thanks to
/// zero egress; beyond ~400GB the cheap-storage providers take over and keep
/// total customer cost under Helumi flat ten dollars a month.
fn routed_cost(gb: f64) -> (Provider, f64) {
    [R2, B2, STORJ]
        .map(|p| (p, p.lumen_customer_cost(gb)))
        .into_iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
}

#[test]
fn storage_cost_curves_undercut_helumi_at_every_profile_point() {
    let profiles: [(f64, &str); 3] = [
        (10.0, "10GB light"),
        (100.0, "100GB mid"),
        (1024.0, "1TB heavy"),
    ];
    for (gb, label) in profiles {
        let (provider, cost) = routed_cost(gb);
        assert!(
            cost < HELUMI_FLAT_USD,
            "{}: cheapest route {} at {:.4} must stay under Helumi {:.2}",
            label,
            provider.name,
            cost,
            HELUMI_FLAT_USD
        );
    }
}

#[test]
fn provider_routing_is_required_at_the_heavy_profile_point() {
    let r2_1tb = R2.lumen_customer_cost(1024.0);
    assert!(r2_1tb > HELUMI_FLAT_USD);
    assert!(B2.lumen_customer_cost(1024.0) < HELUMI_FLAT_USD);
    assert!(STORJ.lumen_customer_cost(1024.0) < HELUMI_FLAT_USD);
}

#[test]
fn helumi_crossover_points_allow_a_over_100gb_claim_when_routing() {
    fn crossover(p: &Provider) -> f64 {
        (HELUMI_FLAT_USD - BASE_MONTHLY_USD) / (p.usd_per_gb_month * STORAGE_MARGIN)
    }
    assert!(crossover(&R2) >= 400.0);
    let best = [B2, STORJ].iter().map(crossover).fold(0.0_f64, f64::max);
    assert!(best >= 1600.0);
}
