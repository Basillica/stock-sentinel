use serde::{Deserialize, Serialize};

/// Fixed-fractional position sizing: risk a fixed % of your account on any
/// single trade, sized by the distance to your stop - not by how much you
/// "want" to own. This is the actual mechanism that keeps one bad trade
/// from doing serious damage; it matters far more than which indicator
/// told you to buy.
#[derive(Debug, Deserialize)]
pub struct PositionSizeRequest {
    pub account_equity: f64,
    /// % of account equity you're willing to lose on this single trade.
    /// 1-2% is the conventional range; higher compounds losses fast.
    pub risk_pct: f64,
    pub entry_price: f64,
    pub stop_price: f64,
}

#[derive(Debug, Serialize)]
pub struct PositionSizeResponse {
    pub risk_amount: f64,
    pub risk_per_share: f64,
    pub shares: f64,
    pub position_value: f64,
    pub position_pct_of_account: f64,
    pub warning: Option<String>,
}

pub fn calculate(req: &PositionSizeRequest) -> PositionSizeResponse {
    let risk_amount = req.account_equity * (req.risk_pct / 100.0);
    let risk_per_share = (req.entry_price - req.stop_price).abs();

    let (shares, warning) = if risk_per_share <= 0.0 {
        (0.0, Some("Entry price equals stop price - can't size a position with zero risk-per-share. Set a real stop.".to_string()))
    } else {
        let raw_shares = (risk_amount / risk_per_share).floor();
        let position_value = raw_shares * req.entry_price;
        let warning = if position_value > req.account_equity {
            Some(format!(
                "Sizing by risk alone wants {position_value:.2} of position, more than your {:.2} account equity - your stop is too tight relative to your risk budget, or you're under-capitalized for this trade. Capping at account equity.",
                req.account_equity
            ))
        } else {
            None
        };
        let shares = if position_value > req.account_equity {
            (req.account_equity / req.entry_price).floor()
        } else {
            raw_shares
        };
        (shares, warning)
    };

    let position_value = shares * req.entry_price;
    let position_pct_of_account = if req.account_equity > 0.0 {
        position_value / req.account_equity * 100.0
    } else {
        0.0
    };

    PositionSizeResponse {
        risk_amount,
        risk_per_share,
        shares,
        position_value,
        position_pct_of_account,
        warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_a_normal_trade() {
        // $10,000 account, risking 1% ($100), entry 50, stop 45 -> risk/share = 5 -> 20 shares
        let req = PositionSizeRequest {
            account_equity: 10_000.0,
            risk_pct: 1.0,
            entry_price: 50.0,
            stop_price: 45.0,
        };
        let res = calculate(&req);
        assert_eq!(res.shares, 20.0);
        assert_eq!(res.risk_amount, 100.0);
        assert!(res.warning.is_none());
    }

    #[test]
    fn flags_a_stop_that_is_too_tight_for_the_account() {
        // Tiny account, tiny stop distance -> would want way more shares than affordable.
        let req = PositionSizeRequest {
            account_equity: 500.0,
            risk_pct: 1.0,
            entry_price: 50.0,
            stop_price: 49.99,
        };
        let res = calculate(&req);
        assert!(res.warning.is_some());
        assert!(res.position_value <= 500.0);
    }
}
