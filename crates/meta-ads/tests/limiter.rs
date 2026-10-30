use std::time::Duration;

use adsconsole_meta_ads::client::{AccessTier, Usage};
use adsconsole_meta_ads::error::MetaError;
use adsconsole_meta_ads::limiter::{AccountLimiter, COST_READ, COST_WRITE};
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn limiter_waits_before_80_percent() {
    let l = AccountLimiter::new();
    let t0 = Instant::now();
    for _ in 0..8 {
        l.acquire("act_1", COST_WRITE).await.unwrap();
    }
    tokio::time::advance(Duration::from_secs(250)).await;
    for _ in 0..8 {
        l.acquire("act_1", COST_WRITE).await.unwrap();
    }
    // 48 pontos = 80% de 60: a próxima escrita espera os primeiros 24 decaírem (t0 + 300 s).
    assert_eq!(t0.elapsed(), Duration::from_secs(250));
    l.acquire("act_1", COST_WRITE).await.unwrap();
    assert_eq!(t0.elapsed(), Duration::from_secs(300));
    // Outra conta não espera.
    let t1 = Instant::now();
    l.acquire("act_2", COST_WRITE).await.unwrap();
    assert_eq!(t1.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn burst_over_limit_gives_up() {
    let l = AccountLimiter::new();
    for _ in 0..16 {
        l.acquire("act_1", COST_WRITE).await.unwrap();
    }
    // Tudo no mesmo instante: só decai em 300 s, mais que os 60 s que o limiter espera.
    let err = l.acquire("act_1", COST_WRITE).await.unwrap_err();
    match err {
        MetaError::RateLimited { account, wait } => {
            assert_eq!(account, "act_1");
            assert_eq!(wait, Duration::from_secs(300));
        }
        other => panic!("esperava RateLimited: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn limiter_gives_up_over_60s() {
    let l = AccountLimiter::new();
    l.observe(
        "act_1",
        &Usage {
            tier: Some(AccessTier::Limited),
            buc_max_pct: Some(95.0),
            regain_after: Some(Duration::from_secs(300)),
        },
    );
    let err = l.acquire("act_1", COST_READ).await.unwrap_err();
    assert!(
        matches!(err, MetaError::RateLimited { wait, .. } if wait == Duration::from_secs(300)),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn short_block_is_waited() {
    let l = AccountLimiter::new();
    l.observe(
        "act_1",
        &Usage {
            tier: None,
            buc_max_pct: Some(92.0),
            regain_after: Some(Duration::from_secs(30)),
        },
    );
    let t0 = Instant::now();
    l.acquire("act_1", COST_READ).await.unwrap();
    assert_eq!(t0.elapsed(), Duration::from_secs(30));
    // Sem regain, 90% bloqueia por 60 s.
    l.observe(
        "act_1",
        &Usage {
            tier: None,
            buc_max_pct: Some(90.0),
            regain_after: None,
        },
    );
    let t1 = Instant::now();
    l.acquire("act_1", COST_READ).await.unwrap();
    assert_eq!(t1.elapsed(), Duration::from_secs(60));
}

#[tokio::test(start_paused = true)]
async fn full_tier_allows_more() {
    let l = AccountLimiter::new();
    l.observe(
        "act_1",
        &Usage {
            tier: Some(AccessTier::Full),
            buc_max_pct: Some(1.0),
            regain_after: None,
        },
    );
    let t0 = Instant::now();
    for _ in 0..1000 {
        l.acquire("act_1", COST_READ).await.unwrap();
    }
    assert_eq!(t0.elapsed(), Duration::ZERO);
}
