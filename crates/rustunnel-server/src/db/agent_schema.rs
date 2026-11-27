//! Schema contract for agent-native accounts.
//!
//! The migration is the source of the column names. These tests pin the
//! values platform-api must copy, and they check that replaying the file
//! does not delete users.

use chrono::{Duration, Utc};
use uuid::Uuid;

use super::{
    count_admin_users, get_admin_user, list_admin_plans, list_admin_users, AdminUserFilters,
    AdminUserSort,
};
use crate::config::DatabaseSection;
use crate::db::{init_db, AgentPlanLimits};

const MIGRATION_SQL: &str = include_str!("../../migrations/pg/20261127232000_agent_accounts.sql");
const MIGRATION_VERSION: i64 = 20_261_127_232_000;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustunnel:test@127.0.0.1:5432/rustunnel_test".into());
    init_db(&DatabaseSection {
        url,
        captured_path: ":memory:".into(),
    })
    .await
    .expect("init_db")
    .pg
}

#[tokio::test]
async fn migration_is_idempotent_and_records_its_version() {
    let pool = pool().await;
    assert!(
        !MIGRATION_SQL.starts_with("-- no-transaction"),
        "the file must stay inside sqlx's transaction"
    );
    // Comments document the absence of these statements. Only the SQL counts.
    let sql_only = MIGRATION_SQL
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    assert!(
        !sql_only.contains("delete from"),
        "deactivation keeps the row; this migration must not delete anything"
    );
    assert!(
        !sql_only.contains("on delete cascade"),
        "expired accounts must not gain an ON DELETE CASCADE"
    );

    let (version,): (i64,) =
        sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE version = $1 AND success")
            .bind(MIGRATION_VERSION)
            .fetch_one(&pool)
            .await
            .expect("migration recorded");
    assert_eq!(version, MIGRATION_VERSION);

    sqlx::raw_sql(MIGRATION_SQL)
        .execute(&pool)
        .await
        .expect("replay once");
    sqlx::raw_sql(MIGRATION_SQL)
        .execute(&pool)
        .await
        .expect("replay twice");
}

#[tokio::test]
async fn legacy_user_insert_defaults_to_human_and_stays_listed() {
    let pool = pool().await;
    let id = Uuid::new_v4();
    let email = format!("legacy-{id}@example.com");
    sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')")
        .bind(id)
        .bind(&email)
        .execute(&pool)
        .await
        .expect("legacy insert omits the new columns");

    let row = get_admin_user(&pool, &id)
        .await
        .expect("query")
        .expect("row");
    assert_eq!(row.kind, "human");
    assert!(row.claimed_at.is_none());
    assert!(row.signup_ip_hash.is_none());
    assert!(row.source.is_none());
    assert!(row.terms_version.is_none());
    assert!(row.deactivated_at.is_none());

    let rejected = sqlx::query(
        "INSERT INTO users (id, email, password_hash, kind) VALUES ($1, $2, 'x', 'robot')",
    )
    .bind(Uuid::new_v4())
    .bind(format!("bad-{id}@example.com"))
    .execute(&pool)
    .await;
    assert!(
        rejected.is_err(),
        "kind CHECK rejects values outside human|agent"
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn agent_plan_row_matches_the_published_limits() {
    let pool = pool().await;
    let plans = list_admin_plans(&pool).await.expect("plans");
    let agent = plans
        .iter()
        .find(|plan| plan.name == AgentPlanLimits::NAME)
        .expect("agent plan row");

    assert_eq!(agent.billing_model, AgentPlanLimits::BILLING_MODEL);
    assert_eq!(agent.max_tunnels, Some(AgentPlanLimits::MAX_TUNNELS));
    assert!(!agent.allow_custom_subdomains);
    assert_eq!(agent.allowed_protocols, AgentPlanLimits::ALLOWED_PROTOCOLS);
    assert!(agent.bandwidth_hard_cap);
    assert_eq!(
        agent.bandwidth_limit_bytes,
        Some(AgentPlanLimits::BANDWIDTH_LIMIT_BYTES)
    );
    assert_eq!(AgentPlanLimits::BANDWIDTH_LIMIT_BYTES, 250_000_000);
    assert!(agent.bandwidth_limit_gb.is_none());
    assert_eq!(agent.monthly_price_cents, 0);
    assert!(agent.is_active);

    let free = plans.iter().find(|plan| plan.name == "free").expect("free");
    assert_eq!(
        free.allowed_protocols,
        vec!["http", "https", "tcp", "udp", "p2p"]
    );
    assert!(!free.bandwidth_hard_cap);
    assert!(free.bandwidth_limit_bytes.is_none());
    assert_eq!(free.bandwidth_limit_gb, Some(1));

    let payg = plans.iter().find(|plan| plan.name == "payg").expect("payg");
    assert!(payg.allow_custom_subdomains);
    assert!(!payg.bandwidth_hard_cap);
}

#[tokio::test]
async fn token_expires_at_stays_nullable() {
    let pool = pool().await;
    let nullable: bool = sqlx::query_scalar(
        "SELECT is_nullable = 'YES' FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'tokens' AND column_name = 'expires_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("expires_at column");
    assert!(nullable);

    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email, password_hash, kind) VALUES ($1, $2, 'x', 'agent')")
        .bind(user_id)
        .bind(format!("agent-token-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();

    let open_id = Uuid::new_v4().to_string();
    let expiring_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO tokens (id, token_hash, label, created_at, user_id, expires_at) \
         VALUES ($1, $2, 'open', $3, $4, NULL), ($5, $6, 'expiring', $3, $4, $7)",
    )
    .bind(&open_id)
    .bind(format!("hash-{open_id}"))
    .bind(now)
    .bind(user_id)
    .bind(&expiring_id)
    .bind(format!("hash-{expiring_id}"))
    .bind(now + Duration::days(AgentPlanLimits::TOKEN_TTL_DAYS))
    .execute(&pool)
    .await
    .expect("nullable and 7-day expires_at both insert");

    sqlx::query("DELETE FROM tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn deactivated_unclaimed_agent_stays_in_admin_analytics() {
    let pool = pool().await;
    let id = Uuid::new_v4();
    let email = format!("deactivated-{id}@example.com");
    let deactivated_at = Utc::now();
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, kind, source, signup_ip_hash, \
                            terms_version, deactivated_at) \
         VALUES ($1, $2, '', 'agent', 'mcp', $3, '2026-11-27', $4)",
    )
    .bind(id)
    .bind(&email)
    .bind(format!("salted-{id}"))
    .bind(deactivated_at)
    .execute(&pool)
    .await
    .unwrap();

    let plan_id: Uuid = sqlx::query_scalar("SELECT id FROM plans WHERE name = 'agent'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO subscriptions (user_id, plan_id, status) VALUES ($1, $2, 'active')")
        .bind(id)
        .bind(plan_id)
        .execute(&pool)
        .await
        .unwrap();

    let filters = AdminUserFilters {
        search: Some(&email),
        ..AdminUserFilters::default()
    };
    let listed = list_admin_users(&pool, 10, 0, filters, AdminUserSort::Created)
        .await
        .expect("list");
    let row = listed
        .iter()
        .find(|user| user.id == id)
        .expect("still listed");
    assert_eq!(row.kind, "agent");
    assert_eq!(row.source.as_deref(), Some("mcp"));
    assert!(row.claimed_at.is_none());
    assert!(row.deactivated_at.is_some());
    assert_eq!(
        row.signup_ip_hash.as_deref(),
        Some(format!("salted-{id}").as_str())
    );
    assert_eq!(row.terms_version.as_deref(), Some("2026-11-27"));
    assert_eq!(row.plan_name.as_deref(), Some("agent"));
    assert_eq!(row.status, "active");

    let matched = count_admin_users(&pool, filters).await.unwrap();
    assert_eq!(matched, 1);
    let not_paid = count_admin_users(
        &pool,
        AdminUserFilters {
            search: Some(&email),
            previously_paid: true,
            ..AdminUserFilters::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        not_paid, 0,
        "an active agent subscription is not a paid plan"
    );

    sqlx::query(
        "UPDATE subscriptions SET status = 'canceled', canceled_at = now() WHERE user_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let canceled = get_admin_user(&pool, &id).await.unwrap().unwrap();
    assert!(canceled.deactivated_at.is_some());
    assert_eq!(canceled.plan_name.as_deref(), Some("free"));
    assert!(
        canceled.previous_plan_name.is_none(),
        "agent is not a paid billing model, so cancel is not win-back churn"
    );
    let still_not_paid = count_admin_users(
        &pool,
        AdminUserFilters {
            search: Some(&email),
            previously_paid: true,
            ..AdminUserFilters::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(still_not_paid, 0);

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes \
         WHERE schemaname = 'public' AND indexname = ANY($1) \
         ORDER BY indexname",
    )
    .bind(vec![
        "idx_users_kind",
        "idx_users_source",
        "idx_users_deactivated_at",
        "idx_users_signup_ip_hash_created_at",
    ])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        indexes,
        vec![
            "idx_users_deactivated_at",
            "idx_users_kind",
            "idx_users_signup_ip_hash_created_at",
            "idx_users_source",
        ]
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
}
