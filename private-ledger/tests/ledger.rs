use ed25519_dalek::{Signer, SigningKey};
use private_ledger::*;
use std::collections::BTreeMap;
const NOW: u64 = 1_800_000_000_000;
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn id(n: u8) -> String {
    hex::encode(key(n).verifying_key().as_bytes())
}
fn label(n: u8) -> String {
    hex::encode([n; 32])
}
fn context() -> Context {
    Context {
        now_ms: NOW,
        markets: BTreeMap::from([(
            1,
            Market {
                asset: "ETH".into(),
                kind: Kind::Call,
                low: 2000 * SCALE,
                high: 2100 * SCALE,
                expiry_ms: NOW + 3_600_000,
                quote_at_ms: NOW,
                mid: 20 * SCALE,
                fixed_price: None,
                voided: false,
            },
        )]),
    }
}
fn envelope(l: &Ledger, n: u8, c: Command) -> SignedCommand {
    SignedCommand {
        actor: id(n),
        nonce: l.accounts.get(&id(n)).map_or(0, |a| a.nonce),
        deadline_ms: NOW + 60_000,
        network: "testnet".into(),
        vault: "private.example.testnet".into(),
        command: c,
        reply_key: label(88),
    }
}
fn apply(l: &mut Ledger, n: u8, c: Command, ctx: &Context) -> Result<(), String> {
    let mut req = envelope(l, n, c);
    req.deadline_ms = ctx.now_ms + 60_000;
    let sig = key(n).sign(&req.signing_bytes()?);
    l.apply(&req, &sig.to_bytes(), ctx)
}
fn journal(l: &mut Ledger, n: u8, profile: &str, now: u64) -> Result<JournalPage, String> {
    let request = JournalRequest {
        actor: id(n),
        profile: profile.into(),
        nonce: l.accounts.get(&id(n)).map_or(0, |a| a.nonce),
        deadline_ms: now + 60_000,
        network: "testnet".into(),
        vault: "private.example.testnet".into(),
        reply_key: label(88),
        cursor: 0,
        limit: 4,
    };
    let sig = key(n).sign(&request.signing_bytes()?);
    l.read_journal(&request, &sig.to_bytes(), now)
}
fn setup() -> Ledger {
    let mut l = Ledger::new("testnet".into(), "private.example.testnet".into(), [91; 32]).unwrap();
    for n in [1, 2, 3] {
        l.credit_verified_deposit(label(n), &id(n), 1000 * SCALE)
            .unwrap();
    }
    apply(
        &mut l,
        1,
        Command::PoolDeposit {
            amount: 900 * SCALE,
        },
        &context(),
    )
    .unwrap();
    l
}
fn profile(l: &mut Ledger) {
    apply(
        l,
        2,
        Command::CreateProfile {
            id: label(9),
            price: 10 * SCALE,
            period_ms: 60_000,
        },
        &context(),
    )
    .unwrap();
}
fn subscribe(l: &mut Ledger) {
    apply(
        l,
        3,
        Command::Subscribe {
            profile: label(9),
            max_price: 10 * SCALE,
        },
        &context(),
    )
    .unwrap();
}
fn buy(publish: Option<String>) -> Command {
    Command::Buy {
        series: 1,
        quantity: SCALE,
        max_premium: 21 * SCALE,
        held: false,
        publish,
    }
}
fn mandate() -> Mandate {
    Mandate {
        profile: label(9),
        quantity: SCALE,
        max_each: 21 * SCALE,
        max_total: 42 * SCALE,
        spent: 0,
        until_ms: NOW + 120_000,
        max_expiry_ms: NOW + 7_200_000,
        asset: "ETH".into(),
        max_exit_loss_bps: 1000,
    }
}

#[test]
fn command_signatures_are_domain_bound_and_cannot_be_replayed() {
    let mut l = setup();
    let req = envelope(&l, 2, buy(None));
    let sig = key(2).sign(&req.signing_bytes().unwrap());
    l.apply(&req, &sig.to_bytes(), &context()).unwrap();
    let before = l.clone();
    assert!(l.apply(&req, &sig.to_bytes(), &context()).is_err());
    assert_eq!(l, before);
    let mut changed = envelope(&l, 2, buy(None));
    changed.vault = "other.testnet".into();
    let sig = key(2).sign(&changed.signing_bytes().unwrap());
    assert!(l.apply(&changed, &sig.to_bytes(), &context()).is_err());
    let req = envelope(&l, 2, buy(None));
    let wrong = key(3).sign(&req.signing_bytes().unwrap());
    assert!(l.apply(&req, &wrong.to_bytes(), &context()).is_err());
    assert_eq!(l, before);
}
#[test]
fn failed_funding_and_insolvent_orders_roll_back_every_field() {
    let mut l = setup();
    let before = l.clone();
    assert!(l.credit_verified_deposit(label(1), &id(2), SCALE).is_err());
    assert_eq!(l, before);
    assert!(apply(
        &mut l,
        2,
        Command::Buy {
            series: 1,
            quantity: 100 * SCALE,
            max_premium: 3000 * SCALE,
            held: false,
            publish: None
        },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
    assert!(apply(
        &mut l,
        2,
        Command::Buy {
            series: 1,
            quantity: 1,
            max_premium: SCALE,
            held: false,
            publish: None
        },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
}
#[test]
fn subscriptions_pay_creator_atomically_and_replay_cannot_charge_twice() {
    let mut l = setup();
    profile(&mut l);
    let before = l.clone();
    assert!(apply(
        &mut l,
        3,
        Command::Subscribe {
            profile: label(9),
            max_price: 9 * SCALE
        },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
    let req = envelope(
        &l,
        3,
        Command::Subscribe {
            profile: label(9),
            max_price: 10 * SCALE,
        },
    );
    let sig = key(3).sign(&req.signing_bytes().unwrap());
    l.apply(&req, &sig.to_bytes(), &context()).unwrap();
    assert_eq!(l.accounts[&id(3)].cash, 990 * SCALE);
    assert_eq!(l.accounts[&id(2)].cash, 1010 * SCALE);
    let before = l.clone();
    assert!(l.apply(&req, &sig.to_bytes(), &context()).is_err());
    assert_eq!(l, before);
}
#[test]
fn only_selected_published_trades_enter_the_subscription_journal() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    apply(&mut l, 2, buy(None), &context()).unwrap();
    apply(
        &mut l,
        2,
        Command::Close {
            position: 0,
            min_proceeds: 0,
        },
        &context(),
    )
    .unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    apply(
        &mut l,
        2,
        Command::Close {
            position: 1,
            min_proceeds: 0,
        },
        &context(),
    )
    .unwrap();
    let visible = journal(&mut l, 3, &label(9), NOW).unwrap();
    assert_eq!(visible.records.len(), 1);
    assert_eq!(visible.records[0].position, 1);
    assert!(journal(&mut l, 1, &label(9), NOW).is_err());
    assert!(journal(&mut l, 3, &label(9), NOW + 60_000).is_err());
}
#[test]
fn copying_requires_active_subscription_and_respects_total_budget_and_revocation() {
    let mut l = setup();
    profile(&mut l);
    let before = l.clone();
    assert!(apply(
        &mut l,
        3,
        Command::SetMandate { mandate: mandate() },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
    subscribe(&mut l);
    apply(
        &mut l,
        3,
        Command::SetMandate { mandate: mandate() },
        &context(),
    )
    .unwrap();
    for _ in 0..3 {
        apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    }
    let copied: Vec<_> = l.positions.values().filter(|p| p.owner == id(3)).collect();
    assert_eq!(copied.len(), 2);
    assert!(copied.iter().all(|p| p.copied_from.is_some()));
    apply(
        &mut l,
        3,
        Command::RevokeMandate { profile: label(9) },
        &context(),
    )
    .unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    assert_eq!(l.positions.values().filter(|p| p.owner == id(3)).count(), 2);
}
#[test]
fn expired_subscription_stops_new_copies_even_with_an_unexpired_mandate() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    apply(
        &mut l,
        3,
        Command::SetMandate { mandate: mandate() },
        &context(),
    )
    .unwrap();
    let mut ctx = context();
    ctx.now_ms += 60_000;
    ctx.markets.get_mut(&1).unwrap().quote_at_ms = ctx.now_ms;
    apply(&mut l, 2, buy(Some(label(9))), &ctx).unwrap();
    assert_eq!(l.positions.values().filter(|p| p.owner == id(3)).count(), 0);
}
#[test]
fn copied_orders_cannot_escape_authorized_asset_or_expiry() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    let mut m = mandate();
    m.asset = "BTC".into();
    apply(&mut l, 3, Command::SetMandate { mandate: m }, &context()).unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    assert_eq!(l.positions.values().filter(|p| p.owner == id(3)).count(), 0);
    let mut m = mandate();
    m.max_expiry_ms = NOW + 1;
    apply(&mut l, 3, Command::SetMandate { mandate: m }, &context()).unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    assert_eq!(l.positions.values().filter(|p| p.owner == id(3)).count(), 0);
}
#[test]
fn held_tickets_refuse_early_close_and_verified_expiry_pays_once() {
    let mut l = setup();
    apply(
        &mut l,
        2,
        Command::Buy {
            series: 1,
            quantity: SCALE,
            max_premium: 20 * SCALE,
            held: true,
            publish: None,
        },
        &context(),
    )
    .unwrap();
    let before = l.clone();
    assert!(apply(
        &mut l,
        2,
        Command::Close {
            position: 0,
            min_proceeds: 0
        },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
    let mut ctx = context();
    ctx.now_ms = NOW + 3_600_000;
    ctx.markets.get_mut(&1).unwrap().fixed_price = Some(2200 * SCALE);
    apply(&mut l, 2, Command::Settle { position: 0 }, &ctx).unwrap();
    assert_eq!(l.accounts[&id(2)].cash, 1080 * SCALE);
    assert!(l.positions.is_empty());
    let before = l.clone();
    assert!(apply(&mut l, 2, Command::Settle { position: 0 }, &ctx).is_err());
    assert_eq!(l, before);
}
#[test]
fn withdrawal_notes_remain_liabilities_until_verified_redemption_and_never_replay() {
    let mut l = setup();
    apply(
        &mut l,
        2,
        Command::Withdraw {
            note: label(11),
            amount: 50 * SCALE,
        },
        &context(),
    )
    .unwrap();
    assert_eq!(l.total_redeemed, 0);
    assert_eq!(l.notes[&label(11)].amount, 50 * SCALE);
    l.complete_verified_redemption(&label(11)).unwrap();
    assert_eq!(l.total_redeemed, 50 * SCALE);
    let before = l.clone();
    assert!(l.complete_verified_redemption(&label(11)).is_err());
    assert!(apply(
        &mut l,
        2,
        Command::Withdraw {
            note: label(11),
            amount: SCALE
        },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
}
#[test]
fn serialization_restores_replay_protection_and_blinding_changes_public_root() {
    let mut l = setup();
    let req = envelope(&l, 2, buy(None));
    let sig = key(2).sign(&req.signing_bytes().unwrap());
    l.apply(&req, &sig.to_bytes(), &context()).unwrap();
    let snapshot = l.with_private_snapshot(|bytes| Ok(bytes.to_vec())).unwrap();
    let mut restored =
        Ledger::restore_verified_snapshot(&snapshot, &l.checkpoint().unwrap()).unwrap();
    assert_eq!(l.root().unwrap(), restored.root().unwrap());
    assert!(restored.apply(&req, &sig.to_bytes(), &context()).is_err());
    let other = Ledger::new("testnet".into(), "private.example.testnet".into(), [92; 32]).unwrap();
    let first = Ledger::new("testnet".into(), "private.example.testnet".into(), [91; 32]).unwrap();
    assert_ne!(first.root().unwrap(), other.root().unwrap());
}

#[test]
fn old_or_tampered_snapshots_cannot_restore_against_latest_finalized_root() {
    let mut l = setup();
    let old = l.with_private_snapshot(|bytes| Ok(bytes.to_vec())).unwrap(); // tests only: production must seal
    apply(&mut l, 2, buy(None), &context()).unwrap();
    let anchor = l.checkpoint().unwrap();
    assert!(Ledger::restore_verified_snapshot(&old, &anchor).is_err());
    let fresh = l.with_private_snapshot(|bytes| Ok(bytes.to_vec())).unwrap();
    let restored = Ledger::restore_verified_snapshot(&fresh, &anchor).unwrap();
    assert_eq!(restored, l);
    let mut forged = anchor.clone();
    forged.vault = "other.testnet".into();
    assert!(Ledger::restore_verified_snapshot(&fresh, &forged).is_err());
    let mut tampered: serde_json::Value = serde_json::from_slice(&fresh).unwrap();
    tampered["accounts"][id(2)]["nonce"] = serde_json::json!(0);
    assert!(
        Ledger::restore_verified_snapshot(&serde_json::to_vec(&tampered).unwrap(), &anchor)
            .is_err()
    );
    assert!(
        !format!("{l:?}").contains(&id(2)),
        "Debug output must not expose private identities"
    );
}

#[test]
fn private_journal_requests_require_signatures_nonce_and_reply_key_binding() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    let request = JournalRequest {
        actor: id(3),
        profile: label(9),
        nonce: l.accounts[&id(3)].nonce,
        deadline_ms: NOW + 60_000,
        network: "testnet".into(),
        vault: "private.example.testnet".into(),
        reply_key: label(88),
        cursor: 0,
        limit: 4,
    };
    let sig = key(3).sign(&request.signing_bytes().unwrap());
    let mut substituted = request.clone();
    substituted.reply_key = label(87);
    assert!(l.read_journal(&substituted, &sig.to_bytes(), NOW).is_err());
    let wrong = key(1).sign(&request.signing_bytes().unwrap());
    assert!(l.read_journal(&request, &wrong.to_bytes(), NOW).is_err());
    assert!(l
        .read_journal(&request, &sig.to_bytes(), NOW + 60_001)
        .is_err());
    assert!(l.read_journal(&request, &sig.to_bytes(), NOW).is_ok());
    assert!(l.read_journal(&request, &sig.to_bytes(), NOW).is_err());
}

#[test]
fn active_copy_mandate_follows_leader_exit_and_verified_settlement_needs_no_new_user_signature() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    apply(
        &mut l,
        3,
        Command::SetMandate { mandate: mandate() },
        &context(),
    )
    .unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    assert_eq!(l.positions.len(), 2);
    apply(
        &mut l,
        2,
        Command::Close {
            position: 0,
            min_proceeds: 19 * SCALE,
        },
        &context(),
    )
    .unwrap();
    assert!(l.positions.is_empty());
    assert_eq!(l.records.len(), 2);
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    assert_eq!(l.positions.len(), 2);
    let mut ctx = context();
    ctx.now_ms += 3_600_000;
    ctx.markets.get_mut(&1).unwrap().fixed_price = Some(2200 * SCALE);
    assert_eq!(l.settle_verified_matured(&ctx).unwrap(), 2);
    assert!(l.positions.is_empty());
    let before = l.clone();
    assert_eq!(l.settle_verified_matured(&ctx).unwrap(), 0);
    assert_eq!(before, l);
}

#[test]
fn journal_pages_fit_transport_and_cursor_and_limits_are_signed() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    for position in 0..11 {
        apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
        apply(
            &mut l,
            2,
            Command::Close {
                position,
                min_proceeds: 0,
            },
            &context(),
        )
        .unwrap();
    }
    let mut cursor = 0;
    let mut seen = Vec::new();
    loop {
        let request = JournalRequest {
            actor: id(3),
            profile: label(9),
            nonce: l.accounts[&id(3)].nonce,
            deadline_ms: NOW + 60_000,
            network: "testnet".into(),
            vault: "private.example.testnet".into(),
            reply_key: label(88),
            cursor,
            limit: 4,
        };
        let signature = key(3).sign(&request.signing_bytes().unwrap());
        let before = l.clone();
        let mut changed = request.clone();
        changed.cursor += 1;
        assert!(l
            .read_journal(&changed, &signature.to_bytes(), NOW)
            .is_err());
        assert_eq!(l, before);
        let page = l
            .read_journal(&request, &signature.to_bytes(), NOW)
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 3000);
        assert!(page.records.len() <= 4);
        seen.extend(page.records.into_iter().map(|r| r.position));
        if let Some(next) = page.next_cursor {
            cursor = next;
        } else {
            break;
        }
    }
    assert_eq!(seen, (0..11).collect::<Vec<_>>());
}

#[test]
fn non_contributory_reply_keys_are_rejected_before_consuming_a_nonce() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    for reply_key in [label(0), format!("01{}", "00".repeat(31))] {
        let before = l.clone();
        let mut request = envelope(&l, 2, buy(None));
        request.reply_key = reply_key.clone();
        let signature = key(2).sign(&request.signing_bytes().unwrap());
        assert!(l
            .apply(&request, &signature.to_bytes(), &context())
            .is_err());
        let request = JournalRequest {
            actor: id(3),
            profile: label(9),
            nonce: l.accounts[&id(3)].nonce,
            deadline_ms: NOW + 60_000,
            network: "testnet".into(),
            vault: "private.example.testnet".into(),
            reply_key,
            cursor: 0,
            limit: 4,
        };
        let signature = key(3).sign(&request.signing_bytes().unwrap());
        assert!(l
            .read_journal(&request, &signature.to_bytes(), NOW)
            .is_err());
        assert_eq!(l, before);
    }
}

#[test]
fn changed_series_terms_cannot_change_an_existing_positions_payout_or_expiry() {
    let mut l = setup();
    apply(&mut l, 2, buy(None), &context()).unwrap();
    let before = l.clone();
    for field in 0..5 {
        let mut ctx = context();
        let m = ctx.markets.get_mut(&1).unwrap();
        match field {
            0 => m.kind = Kind::Put,
            1 => m.low -= SCALE,
            2 => m.high += SCALE,
            3 => m.expiry_ms += 1,
            _ => m.asset = "BTC".into(),
        }
        assert!(apply(
            &mut l,
            2,
            Command::Close {
                position: 0,
                min_proceeds: 0
            },
            &ctx
        )
        .is_err());
        ctx.now_ms = NOW + 7_200_000;
        ctx.markets.get_mut(&1).unwrap().fixed_price = Some(2200 * SCALE);
        assert!(l.settle_verified_matured(&ctx).is_err());
        assert_eq!(l, before);
    }
}

#[test]
fn automatic_copy_exit_obeys_the_subscribers_signed_loss_floor() {
    let mut l = setup();
    profile(&mut l);
    subscribe(&mut l);
    let mut m = mandate();
    m.max_exit_loss_bps = 1000;
    apply(&mut l, 3, Command::SetMandate { mandate: m }, &context()).unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    let mut lower = context();
    lower.markets.get_mut(&1).unwrap().mid = 10 * SCALE;
    apply(
        &mut l,
        2,
        Command::Close {
            position: 0,
            min_proceeds: 0,
        },
        &lower,
    )
    .unwrap();
    assert_eq!(
        l.positions.len(),
        1,
        "leader can exit, but follower's loss floor prevents an automatic exit"
    );
    assert_eq!(l.positions[&1].owner, id(3));
    assert_eq!(
        l.copy_attempts.last().unwrap().outcome,
        "loss_floor_quote_or_terms_refused"
    );
    // Refusal never traps funds: the owner retains direct close authority.
    apply(
        &mut l,
        3,
        Command::Close {
            position: 1,
            min_proceeds: 0,
        },
        &lower,
    )
    .unwrap();
    assert!(l.positions.is_empty());
}

#[test]
fn public_performance_is_opt_in_includes_unpublished_losses_and_can_be_disabled() {
    let mut l = setup();
    profile(&mut l);
    apply(&mut l, 2, buy(None), &context()).unwrap();
    let mut loss = context();
    loss.markets.get_mut(&1).unwrap().mid = 5 * SCALE;
    apply(
        &mut l,
        2,
        Command::Close {
            position: 0,
            min_proceeds: 0,
        },
        &loss,
    )
    .unwrap();
    apply(&mut l, 2, buy(Some(label(9))), &context()).unwrap();
    assert!(l.public_profile(&label(9)).unwrap().performance.is_none());
    let before = l.clone();
    assert!(apply(
        &mut l,
        3,
        Command::SetPerformanceDisclosure {
            profile: label(9),
            enabled: true
        },
        &context()
    )
    .is_err());
    assert_eq!(l, before);
    apply(
        &mut l,
        2,
        Command::SetPerformanceDisclosure {
            profile: label(9),
            enabled: true,
        },
        &context(),
    )
    .unwrap();
    let public = l.public_profile(&label(9)).unwrap();
    let p = public.performance.as_ref().unwrap();
    assert_eq!(p.closed_trades, 1);
    assert_eq!(p.open_positions, 1);
    assert!(
        p.realized_pnl.starts_with('-'),
        "unpublished loss must not be excluded from performance"
    );
    let json = serde_json::to_string(&public).unwrap();
    assert!(!json.contains(&id(2)) && !json.contains("at_ms"));
    assert!(!json.contains("\"owner\"") && !json.contains("\"series\""));
    apply(
        &mut l,
        2,
        Command::SetPerformanceDisclosure {
            profile: label(9),
            enabled: false,
        },
        &context(),
    )
    .unwrap();
    assert!(l.public_profile(&label(9)).unwrap().performance.is_none());
}
